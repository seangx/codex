//! Builds and sends foreground and speculative remote V2 compaction attempts.

use crate::context::UserGoalUpdate;
use codex_protocol::ResponseItemId;
use std::collections::HashSet;
use std::sync::Arc;

use super::CompactionRequestTransport;
use super::RemoteCompactionV2Output;
use super::run_remote_compaction_request_v2;
use crate::Prompt;
use crate::client::ModelClientSession;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact_remote_history::retain_recent_history_item_groups;
use crate::compact_remote_history::trim_function_call_history_to_fit_context_window;
use crate::context::world_state::WorldState;
use crate::context_manager::ContextManager;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::session::CompactionPrefix;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionTrigger;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::TokenUsage;
use codex_rollout_trace::CompactionTraceContext;
use tracing::info;

// The Responses API accepts at most 16,384 top-level input items.
const RESPONSES_INPUT_ITEM_LIMIT: usize = 16_384;
const COMPACTION_TRIGGER_INPUT_ITEM_COUNT: usize = 1;

pub(super) struct RemoteCompactV2Attempt {
    pub(super) input_goal_ids: HashSet<ResponseItemId>,
    pub(super) trace_input_history: Option<Vec<ResponseItem>>,
    pub(super) prompt_input: Vec<ResponseItem>,
    pub(super) prompt_input_metadata: Vec<Option<CodexHarnessMetadata>>,
    pub(super) compaction_output: ResponseItem,
    pub(super) compaction_response_id: String,
    pub(super) token_usage: Option<TokenUsage>,
    /// Keeps a session created for standalone compaction alive through lifecycle completion.
    pub(super) owned_client_session: Option<ModelClientSession>,
}

pub(super) enum RemoteCompactV2AttemptInput<'a> {
    Foreground(Option<&'a mut ModelClientSession>),
    Background(Box<BackgroundCompactionInput>),
}

pub(crate) struct BackgroundCompactionCandidate {
    pub(crate) prefix: CompactionPrefix,
    pub(super) model_info: Arc<ModelInfo>,
    pub(super) cutoff_world_state: Arc<WorldState>,
    pub(super) attempt: RemoteCompactV2Attempt,
}

pub(crate) struct BackgroundCompactionInput {
    prefix: CompactionPrefix,
    turn_state: Option<String>,
    window: (String, u64, uuid::Uuid),
    base_instructions: BaseInstructions,
    snapshot: ContextManager,
}

pub(super) async fn run_remote_compact_v2_attempt(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    compaction_trace: &CompactionTraceContext,
    compaction_metadata: CompactionTurnMetadata,
    analytics_details: &mut CompactionAnalyticsDetails,
    input: RemoteCompactV2AttemptInput<'_>,
) -> CodexResult<RemoteCompactV2Attempt> {
    let turn_context = &step_context.turn;
    let mut owned_client_session = None;
    let (
        model_info,
        mut history,
        base_instructions,
        transport,
        captured_window,
        context_window,
        update_analytics_for_trim,
    ) = match input {
        RemoteCompactV2AttemptInput::Foreground(client_session) => {
            let model_info = turn_context.model_info();
            let history = crate::compact_image_references::prepare_history_for_compaction(
                sess.clone_history().await,
                turn_context.config.codex_home.to_path_buf(),
                &turn_context.session_source,
            )
            .await?;
            let tools_in_history = step_context.settings.model_info.use_responses_lite
                && history
                    .raw_items()
                    .any(|item| matches!(item, ResponseItem::AdditionalTools { .. }));
            let base_instructions = if tools_in_history {
                BaseInstructions {
                    text: String::new(),
                    provenance: None,
                }
            } else {
                sess.get_prompt_base_instructions().await
            };
            let client_session = client_session.unwrap_or_else(|| {
                owned_client_session.insert(sess.services.model_client.new_session())
            });
            (
                model_info,
                history,
                base_instructions,
                CompactionRequestTransport::Foreground(client_session),
                None,
                turn_context.model_context_window(),
                true,
            )
        }
        RemoteCompactV2AttemptInput::Background(input) => {
            let input = *input;
            let history = crate::compact_image_references::prepare_history_for_compaction(
                input.snapshot,
                turn_context.config.codex_home.to_path_buf(),
                &turn_context.session_source,
            )
            .await?;
            (
                &step_context.settings.model_info,
                history,
                input.base_instructions,
                CompactionRequestTransport::Background(
                    input.turn_state,
                    input.prefix.auth_owner_generation,
                ),
                Some(input.window),
                step_context.settings.model_info.usable_context_window(),
                false,
            )
        }
    };
    let input_goal_ids = UserGoalUpdate::message_ids(history.raw_items());
    let tools_in_history = model_info.use_responses_lite
        && history
            .raw_items()
            .any(|item| matches!(item, ResponseItem::AdditionalTools { .. }));
    let (rewritten_outputs, estimated_deleted_tokens) =
        trim_function_call_history_to_fit_context_window(
            &mut history,
            turn_context,
            context_window,
            &base_instructions,
        );
    if rewritten_outputs > 0 {
        info!(
            turn_id = %turn_context.sub_id,
            rewritten_outputs,
            "rewrote history outputs before remote compaction v2"
        );
    }
    if update_analytics_for_trim && estimated_deleted_tokens > 0 {
        let max_local_deleted_tokens = sess
            .estimated_tokens_after_last_model_generated_item()
            .await;
        analytics_details.active_context_tokens_before = analytics_details
            .active_context_tokens_before
            .map(|active_context_tokens_before| {
                active_context_tokens_before
                    .saturating_sub(estimated_deleted_tokens.min(max_local_deleted_tokens))
            });
    }

    // Both the request and post-compaction retention use the same image-handled snapshot.
    // Ordinary sessions use archived references so retained history does not reintroduce image
    // bytes; Guardian reviewers preserve their bounded, admitted evidence as model-visible images.
    let retained_history = history.clone();
    let prompt_history = history.for_compaction_prompt_annotated(&model_info.input_modalities);
    let original_prompt_item_count = prompt_history.len();
    // Remote compaction appends one trigger. Responses Lite additionally prepends the tool
    // inventory and, when present, the base instructions while constructing the wire request.
    // Reserve every generated item before bounding history so the serialized `input` stays within
    // the API limit rather than bounding only the pre-format Prompt.
    let responses_lite_prefix_item_count = if model_info.use_responses_lite {
        let base_instructions_item_count = if base_instructions.text.is_empty() {
            0
        } else {
            1
        };
        1 + base_instructions_item_count
    } else {
        0
    };
    let remote_compaction_history_item_limit = RESPONSES_INPUT_ITEM_LIMIT
        .saturating_sub(COMPACTION_TRIGGER_INPUT_ITEM_COUNT)
        .saturating_sub(responses_lite_prefix_item_count);
    let prompt_history = limit_remote_compaction_prompt_history(
        prompt_history,
        remote_compaction_history_item_limit,
    );
    if prompt_history.len() < original_prompt_item_count {
        info!(
            turn_id = %turn_context.sub_id,
            original_prompt_item_count,
            retained_prompt_item_count = prompt_history.len(),
            "retained recent history items to fit remote compaction input limit"
        );
    }
    let trace_input_history = compaction_trace.is_enabled().then(|| {
        prompt_history
            .iter()
            .map(|envelope| envelope.item.clone())
            .collect()
    });
    let mut input: Vec<ResponseItem> = prompt_history
        .into_iter()
        .map(|envelope| envelope.item)
        .collect();
    sess.services
        .executed_tool_calls
        .attach_to_compaction_prompt(&mut input);
    let tool_router = &step_context.tool_router;
    input.push(ResponseItem::CompactionTrigger {});
    let prompt = Prompt {
        input,
        tools: if tools_in_history {
            Arc::default()
        } else {
            tool_router.model_visible_specs()
        },
        parallel_tool_calls: true,
        base_instructions,
        output_schema: None,
        output_schema_strict: true,
        cyber_access_program: turn_context.cyber_access_program,
    };

    let mut responses_metadata = match captured_window {
        Some(window) => {
            sess.compaction_responses_metadata_for_window(turn_context, compaction_metadata, window)
        }
        None => {
            sess.compaction_responses_metadata(turn_context, compaction_metadata)
                .await
        }
    };
    responses_metadata.tool_namespaces_info = tool_router.tool_namespaces_info().cloned();
    let trace_attempt = compaction_trace.start_attempt(&serde_json::json!({
        "model": model_info.slug.as_str(),
        "instructions": prompt.base_instructions.text.as_str(),
        "input": &prompt.input,
        "parallel_tool_calls": prompt.parallel_tool_calls,
    }));
    let compaction_output_result = run_remote_compaction_request_v2(
        sess,
        step_context,
        &prompt,
        &responses_metadata,
        transport,
    )
    .await;
    trace_attempt.record_result(
        compaction_output_result
            .as_ref()
            .map(|output| std::slice::from_ref(&output.compaction_output)),
    );
    let RemoteCompactionV2Output {
        compaction_output,
        response_id,
        token_usage,
    } = compaction_output_result?;
    let retained_prompt_history =
        retained_history.for_prompt_annotated(&model_info.input_modalities);
    let (prompt_input, prompt_input_metadata): (Vec<_>, Vec<_>) = retained_prompt_history
        .into_iter()
        .map(|envelope| (envelope.item, envelope.metadata))
        .unzip();
    Ok(RemoteCompactV2Attempt {
        input_goal_ids,
        trace_input_history,
        prompt_input,
        prompt_input_metadata,
        compaction_output,
        compaction_response_id: response_id,
        token_usage,
        owned_client_session,
    })
}

fn limit_remote_compaction_prompt_history(
    prompt_history: Vec<ResponseItemEnvelope>,
    max_items: usize,
) -> Vec<ResponseItemEnvelope> {
    let mut prompt_history = retain_recent_history_item_groups(prompt_history, max_items);

    // The tail boundary can land between a call and its output. Calls always precede outputs in
    // normalized history, so retaining the tail cannot orphan a call, but it can orphan an output.
    // Remove only those boundary outputs; the canonical session history stays untouched.
    let mut function_call_ids = HashSet::new();
    let mut custom_tool_call_ids = HashSet::new();
    let mut tool_search_call_ids = HashSet::new();
    for envelope in &prompt_history {
        match &envelope.item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                ..
            } => {
                function_call_ids.insert(call_id.clone());
            }
            ResponseItem::CustomToolCall { call_id, .. } => {
                custom_tool_call_ids.insert(call_id.clone());
            }
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                ..
            } => {
                tool_search_call_ids.insert(call_id.clone());
            }
            _ => {}
        }
    }
    prompt_history.retain(|envelope| match &envelope.item {
        ResponseItem::FunctionCallOutput {
            call_id: Some(call_id),
            ..
        } => function_call_ids.contains(call_id),
        ResponseItem::CustomToolCallOutput { call_id, .. } => {
            custom_tool_call_ids.contains(call_id)
        }
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            execution,
            ..
        } if execution != "server" => tool_search_call_ids.contains(call_id),
        _ => true,
    });
    prompt_history
}

pub(crate) fn capture_background_input(
    _sess: &Session,
    step_context: &StepContext,
    snapshot: ContextManager,
    base_instructions: BaseInstructions,
    turn_state: Option<String>,
    auth_owner_generation: Option<u64>,
    window: (String, u64, uuid::Uuid),
) -> BackgroundCompactionInput {
    let base_instructions = if step_context.settings.model_info.use_responses_lite
        && snapshot
            .raw_items()
            .any(|item| matches!(item, ResponseItem::AdditionalTools { .. }))
    {
        BaseInstructions {
            text: String::new(),
            provenance: None,
        }
    } else {
        base_instructions
    };
    let prefix = CompactionPrefix {
        generation: snapshot.history_version(),
        len: snapshot.annotated_items().len(),
        auth_owner_generation,
    };
    BackgroundCompactionInput {
        prefix,
        turn_state,
        window,
        base_instructions,
        snapshot,
    }
}

pub(crate) async fn run_background_compaction(
    sess: &Arc<Session>,
    step_context: Arc<StepContext>,
    cutoff_world_state: Arc<WorldState>,
    phase: CompactionPhase,
    prepared_input: BackgroundCompactionInput,
) -> CodexResult<BackgroundCompactionCandidate> {
    let prefix = prepared_input.prefix;
    let metadata = CompactionTurnMetadata::new(
        CompactionTrigger::Auto,
        CompactionReason::ContextLimit,
        CompactionImplementation::ResponsesCompactionV2,
        phase,
    );
    let total_deadline = step_context.turn.provider.info().stream_idle_timeout();
    let attempt = tokio::time::timeout(
        total_deadline,
        run_remote_compact_v2_attempt(
            sess,
            &step_context,
            &CompactionTraceContext::disabled(),
            metadata,
            &mut CompactionAnalyticsDetails::default(),
            RemoteCompactV2AttemptInput::Background(Box::new(prepared_input)),
        ),
    )
    .await
    .map_err(|_| {
        codex_protocol::error::CodexErr::Stream(format!(
            "remote compaction v2 exceeded its total deadline of {} ms waiting for response.completed",
            total_deadline.as_millis()
        ))
    })??;
    Ok(BackgroundCompactionCandidate {
        prefix,
        model_info: Arc::clone(&step_context.settings.model_info),
        cutoff_world_state,
        attempt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use pretty_assertions::assert_eq;

    fn message(role: &str, text: impl Into<String>) -> ResponseItemEnvelope {
        ResponseItemEnvelope::new(ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: role.to_string(),
            content: vec![ContentItem::InputText { text: text.into() }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        })
    }

    fn message_text(item: &ResponseItemEnvelope) -> Option<&str> {
        let ResponseItem::Message { content, .. } = &item.item else {
            return None;
        };
        let [ContentItem::InputText { text }] = content.as_slice() else {
            return None;
        };
        Some(text)
    }

    #[test]
    fn limits_remote_compaction_to_a_stable_recent_suffix() {
        let original = (0..RESPONSES_INPUT_ITEM_LIMIT + 5)
            .map(|index| message("user", format!("message-{index}")))
            .collect::<Vec<_>>();
        let max_items = RESPONSES_INPUT_ITEM_LIMIT - 3;

        let first = limit_remote_compaction_prompt_history(original.clone(), max_items);
        let second = limit_remote_compaction_prompt_history(original, max_items);

        assert_eq!(first, second);
        assert_eq!(first.len(), max_items);
        assert_eq!(message_text(&first[0]), Some("message-8"));
        assert_eq!(
            message_text(first.last().expect("last item")),
            Some("message-16388")
        );
        assert!(first.len() + 3 <= RESPONSES_INPUT_ITEM_LIMIT);
    }

    #[test]
    fn does_not_split_an_attached_resize_notice_at_the_tail_boundary() {
        let bounded = limit_remote_compaction_prompt_history(
            vec![
                message("user", "image source"),
                message(
                    "developer",
                    "<image_resize_notice>generated</image_resize_notice>",
                ),
                message("user", "latest"),
            ],
            2,
        );

        assert_eq!(bounded.len(), 1);
        assert_eq!(message_text(&bounded[0]), Some("latest"));
    }

    #[test]
    fn removes_an_output_orphaned_by_the_tail_boundary() {
        let bounded = limit_remote_compaction_prompt_history(
            vec![
                ResponseItemEnvelope::new(ResponseItem::FunctionCall {
                    encrypted_content: None,
                    status: None,
                    id: None,
                    name: "shell".to_string(),
                    namespace: None,
                    arguments: "{}".to_string(),
                    call_id: "call-1".to_string(),
                    encrypted_function_args: None,
                    internal_chat_message_metadata_passthrough: None,
                }),
                ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some("call-1".to_string()),
                    name: Some("shell".to_string()),
                    namespace: None,
                    output: FunctionCallOutputPayload::from_text("done".to_string()),
                    internal_chat_message_metadata_passthrough: None,
                }),
                message("user", "latest"),
            ],
            2,
        );

        assert_eq!(bounded.len(), 1);
        assert_eq!(message_text(&bounded[0]), Some("latest"));
    }
}
