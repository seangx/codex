use std::sync::Arc;

use super::RemoteCompactionV2Output;
use super::run_remote_compaction_request_v2;
use crate::Prompt;
use crate::client::ModelClientSession;
use crate::compact::CompactionAnalyticsDetails;
use crate::compact_remote::trim_function_call_history_to_fit_context_window;
use crate::compact_remote_history::retain_recent_history_item_groups;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::CompactionTurnMetadata;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use codex_rollout_trace::CompactionTraceContext;
use std::collections::HashSet;
use tracing::info;

// The Responses API accepts at most 16,384 top-level input items.
const RESPONSES_INPUT_ITEM_LIMIT: usize = 16_384;
const COMPACTION_TRIGGER_INPUT_ITEM_COUNT: usize = 1;

pub(super) struct RemoteCompactV2Attempt {
    pub(super) trace_input_history: Option<Vec<ResponseItem>>,
    pub(super) prompt_input: Vec<ResponseItem>,
    pub(super) prompt_input_metadata: Vec<Option<CodexHarnessMetadata>>,
    pub(super) compaction_output: ResponseItem,
    pub(super) compaction_response_id: String,
    pub(super) token_usage: Option<TokenUsage>,
    /// Keeps a session created for standalone compaction alive through lifecycle completion.
    pub(super) owned_client_session: Option<ModelClientSession>,
}

pub(super) async fn run_remote_compact_v2_attempt(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    client_session: Option<&mut ModelClientSession>,
    compaction_trace: &CompactionTraceContext,
    compaction_metadata: CompactionTurnMetadata,
    analytics_details: &mut CompactionAnalyticsDetails,
) -> CodexResult<RemoteCompactV2Attempt> {
    let turn_context = &step_context.turn;
    let mut history = sess.clone_history().await;
    let base_instructions = sess.get_prompt_base_instructions().await;
    let (rewritten_outputs, estimated_deleted_tokens) =
        trim_function_call_history_to_fit_context_window(
            &mut history,
            turn_context.as_ref(),
            &base_instructions,
        );
    if rewritten_outputs > 0 {
        info!(
            turn_id = %turn_context.sub_id,
            rewritten_outputs,
            "rewrote history outputs before remote compaction v2"
        );
    }
    if estimated_deleted_tokens > 0 {
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

    // Keep the normalized canonical history for the post-compaction retention pass. The
    // compaction request itself uses a media-free copy so large inline payloads are never
    // serialized over the wire. `ContextManager::clone` only clones the backing Arc here;
    // once the request copy is consumed, normalization can unwrap the retained history
    // without cloning its media strings.
    let retained_history = history.clone();
    let prompt_history =
        history.for_compaction_prompt_annotated(&turn_context.model_info().input_modalities);
    let original_prompt_item_count = prompt_history.len();
    // Remote compaction appends one trigger. Responses Lite additionally prepends the tool
    // inventory and, when present, the base instructions while constructing the wire request.
    // Reserve every generated item before bounding history so the serialized `input` stays within
    // the API limit rather than bounding only the pre-format Prompt.
    let responses_lite_prefix_item_count = if turn_context.model_info().use_responses_lite {
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
    let tool_router = &step_context.tool_router;
    input.push(ResponseItem::CompactionTrigger {});
    let prompt = Prompt {
        input,
        tools: tool_router.model_visible_specs(),
        parallel_tool_calls: true,
        base_instructions,
        output_schema: None,
        output_schema_strict: true,
        cyber_access_program: turn_context.cyber_access_program,
    };

    let responses_metadata = sess
        .responses_metadata(
            turn_context.as_ref(),
            CodexResponsesRequestKind::Compaction(compaction_metadata),
        )
        .await;
    let trace_attempt = compaction_trace.start_attempt(&serde_json::json!({
        "model": turn_context.model_info().slug.as_str(),
        "instructions": prompt.base_instructions.text.as_str(),
        "input": &prompt.input,
        "parallel_tool_calls": prompt.parallel_tool_calls,
    }));
    let mut owned_client_session = None;
    let client_session = match client_session {
        Some(client_session) => client_session,
        None => owned_client_session.insert(sess.services.model_client.new_session()),
    };
    let compaction_output_result = run_remote_compaction_request_v2(
        sess,
        step_context,
        client_session,
        &prompt,
        &responses_metadata,
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
        retained_history.for_prompt_annotated(&turn_context.model_info().input_modalities);
    let (prompt_input, prompt_input_metadata): (Vec<_>, Vec<_>) = retained_prompt_history
        .into_iter()
        .map(|envelope| (envelope.item, envelope.metadata))
        .unzip();
    Ok(RemoteCompactV2Attempt {
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

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use pretty_assertions::assert_eq;

    fn message(role: &str, text: impl Into<String>) -> ResponseItemEnvelope {
        ResponseItemEnvelope::new(ResponseItem::Message {
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
