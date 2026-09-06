//! Shared retry and transport fallback decisions for Responses requests.

use std::time::Duration;

use crate::client::ModelClientSession;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::util::backoff;
use codex_client::RetryOperation;
use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputQuestion;
use codex_protocol::request_user_input::RequestUserInputQuestionOption;
use tracing::warn;

const INITIAL_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_CONNECTION_RETRY_DELAY: Duration = Duration::from_secs(60);
const HTTP_FALLBACK_STREAM_RETRY_MULTIPLIER: u64 = 2;
const MAX_HTTP_FALLBACK_STREAM_RETRIES: u64 = 100;
const WEBSOCKET_FALLBACK_CALL_ID: &str = "websocket_fallback_choice";
const WEBSOCKET_FALLBACK_QUESTION_ID: &str = "transport";
const KEEP_RECONNECTING_LABEL: &str = "Keep reconnecting (Recommended)";
const USE_HTTPS_FALLBACK_LABEL: &str = "Use HTTPS fallback";

#[derive(Debug, Clone, Copy)]
pub(crate) enum ResponsesStreamRequest {
    Sampling,
    RemoteCompactionV2,
}

pub(crate) struct ResponsesStreamRetryState {
    retries: u64,
    connection_retries: u64,
    connection_retry_delay: Duration,
}

impl Default for ResponsesStreamRetryState {
    fn default() -> Self {
        Self {
            retries: 0,
            connection_retries: 0,
            connection_retry_delay: INITIAL_CONNECTION_RETRY_DELAY,
        }
    }
}

/// Handles a retryable stream error and returns `Ok(())` when the caller should
/// retry the request loop.
pub(crate) async fn handle_retryable_response_stream_error(
    retry_state: &mut ResponsesStreamRetryState,
    max_retries: u64,
    err: CodexErr,
    client_session: &mut ModelClientSession,
    sess: &Session,
    turn_context: &TurnContext,
    request: ResponsesStreamRequest,
) -> Result<(), CodexErr> {
    let operation = match request {
        ResponsesStreamRequest::Sampling => RetryOperation::Sampling,
        ResponsesStreamRequest::RemoteCompactionV2 => RetryOperation::RemoteCompactionV2,
    };
    // A fallback request has already spent the WebSocket retry budget. Give the HTTP transport a
    // separate, larger bounded budget so a transient disconnect does not immediately terminate the
    // turn. Preserve an explicit zero-retry policy and keep the provider-level hard cap.
    let max_retries = if max_retries > 0 && client_session.responses_http_fallback_active() {
        max_retries
            .saturating_mul(HTTP_FALLBACK_STREAM_RETRY_MULTIPLIER)
            .min(MAX_HTTP_FALLBACK_STREAM_RETRIES)
    } else {
        max_retries
    };

    if turn_context
        .config
        .features
        .enabled(Feature::UnboundedConnectionRetries)
        && matches!(request, ResponsesStreamRequest::Sampling)
        && matches!(err.details(), CodexErrorDetails::ConnectionFailed(_))
        && !turn_context.session_source.is_internal()
        && !turn_context.provider.info().is_amazon_bedrock()
    {
        let retry_delay = retry_state.connection_retry_delay;
        warn!(
            turn_id = %turn_context.sub_id,
            error = %err,
            ?retry_delay,
            "stream connection failed; waiting to retry"
        );
        sess.notify_stream_error(turn_context, "Reconnecting... waiting for network", err)
            .await;
        retry_state.connection_retries = retry_state.connection_retries.saturating_add(1);
        codex_client::record_retry!(retry_state.connection_retries, retry_delay, operation);
        tokio::time::sleep(retry_delay).await;
        retry_state.connection_retry_delay = retry_delay
            .saturating_mul(2)
            .min(MAX_CONNECTION_RETRY_DELAY);
        return Ok(());
    }

    if retry_state.retries >= max_retries
        && sess.services.model_client.responses_websocket_enabled()
    {
        let keep_reconnecting = if matches!(request, ResponsesStreamRequest::Sampling)
            && matches!(
                &turn_context.session_source,
                SessionSource::Cli | SessionSource::Mcp
            ) {
            let response = sess
                .request_user_input(
                    turn_context,
                    WEBSOCKET_FALLBACK_CALL_ID.to_string(),
                    RequestUserInputArgs {
                        questions: vec![RequestUserInputQuestion {
                            id: WEBSOCKET_FALLBACK_QUESTION_ID.to_string(),
                            header: "Connection".to_string(),
                            question:
                                "WebSocket retries were exhausted. How should Codex continue?"
                                    .to_string(),
                            is_other: false,
                            is_secret: false,
                            options: Some(vec![
                                RequestUserInputQuestionOption {
                                    label: KEEP_RECONNECTING_LABEL.to_string(),
                                    description: "Retry WebSocket after a longer delay and keep HTTPS disabled."
                                        .to_string(),
                                },
                                RequestUserInputQuestionOption {
                                    label: USE_HTTPS_FALLBACK_LABEL.to_string(),
                                    description: "Replay this turn over HTTPS now; long sessions may resend a large request."
                                        .to_string(),
                                },
                            ]),
                        }],
                        is_blocking: true,
                        auto_resolution_ms: None,
                    },
                )
                .await;
            let Some(response) = response else {
                return Err(err);
            };
            response
                .response
                .answers
                .get(WEBSOCKET_FALLBACK_QUESTION_ID)
                .and_then(|answer| answer.answers.first())
                .is_some_and(|answer| answer == KEEP_RECONNECTING_LABEL)
        } else {
            false
        };

        if keep_reconnecting {
            let retry_delay = retry_state.connection_retry_delay;
            let retry_deadline = tokio::time::Instant::now() + retry_delay;
            warn!(
                turn_id = %turn_context.sub_id,
                error = %err,
                ?retry_delay,
                "WebSocket retries exhausted; user chose to keep reconnecting"
            );
            sess.notify_stream_error(
                turn_context,
                format!("Reconnecting WebSocket in {}s", retry_delay.as_secs()),
                err,
            )
            .await;
            retry_state.connection_retries = retry_state.connection_retries.saturating_add(1);
            codex_client::record_retry!(retry_state.connection_retries, retry_delay, operation);
            tokio::time::sleep_until(retry_deadline).await;
            retry_state.connection_retry_delay = retry_delay
                .saturating_mul(2)
                .min(MAX_CONNECTION_RETRY_DELAY);
            retry_state.retries = 0;
            return Ok(());
        }

        if client_session.try_switch_fallback_transport(
            &turn_context.session_telemetry,
            turn_context.model_info(),
        ) {
            sess.send_event(
                turn_context,
                EventMsg::Warning(WarningEvent {
                    message: format!("Falling back from WebSockets to HTTPS transport. {err:#}"),
                }),
            )
            .await;
            retry_state.retries = 0;
            return Ok(());
        }
    }

    if retry_state.retries < max_retries {
        retry_state.retries += 1;
        let retry_count = retry_state.retries;
        let delay = err.retry_delay().unwrap_or_else(|| backoff(retry_count));
        log_retry(request, turn_context, &err, retry_count, max_retries, delay);

        // In release builds, hide the first websocket retry notification to reduce noisy
        // transient reconnect messages. In debug builds, keep full visibility for diagnosis.
        let report_error = retry_count > 1
            || cfg!(debug_assertions)
            || !sess.services.model_client.responses_websocket_enabled();
        if report_error {
            // Surface retry information to any UI/front-end so the user understands what is
            // happening instead of staring at a seemingly frozen screen.
            sess.notify_stream_error(
                turn_context,
                format!("Reconnecting... {retry_count}/{max_retries}"),
                err,
            )
            .await;
        }
        codex_client::record_retry!(retry_count, delay, operation);
        tokio::time::sleep(delay).await;
        return Ok(());
    }

    Err(err)
}

fn log_retry(
    request: ResponsesStreamRequest,
    turn_context: &TurnContext,
    err: &CodexErr,
    retries: u64,
    max_retries: u64,
    delay: Duration,
) {
    match request {
        ResponsesStreamRequest::Sampling => {
            warn!(
                turn_id = %turn_context.sub_id,
                retries,
                max_retries,
                sampling_error = %err,
                "stream disconnected - retrying sampling request ({retries}/{max_retries} in {delay:?})...",
            );
        }
        ResponsesStreamRequest::RemoteCompactionV2 => {
            warn!(
                turn_id = %turn_context.sub_id,
                retries,
                max_retries,
                compact_error = %err,
                "remote compaction v2 stream failed; retrying request after delay"
            );
        }
    }
}

#[cfg(test)]
#[path = "responses_retry_tests.rs"]
mod tests;
