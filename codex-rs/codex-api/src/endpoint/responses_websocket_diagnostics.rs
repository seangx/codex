//! Bounded, content-free diagnostics for one outbound WS request. Byte counts describe the
//! serialized JSON before negotiated WebSocket compression, not bytes received by the server.
//! Unknown close text is deliberately not logged: servers may echo request content in it.

use super::ResponsesWebsocketTimingLogContext;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub(super) struct RequestDiagnostics {
    request_id: u64,
    connection_id: u64,
    started: Instant,
    request_bytes: usize,
    input_items: usize,
    has_previous_response_id: bool,
    warmup: bool,
    pub(super) connection_reused: bool,
    pub(super) outcome: &'static str,
    pub(super) response_created: bool,
    pub(super) os_error: Option<i32>,
    send_ms: Option<u64>,
    // Consumer observation time, not socket arrival time: the existing transport waits for the
    // upload to finish before consuming queued messages.
    first_message_ms: Option<u64>,
    close_code: Option<u16>,
    close_reason_category: &'static str,
    close_reason_bytes: usize,
}

impl RequestDiagnostics {
    pub(super) fn new(
        context: &ResponsesWebsocketTimingLogContext,
        request_bytes: usize,
        connection_id: u64,
    ) -> Self {
        let diagnostics = Self {
            request_id: NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
            connection_id,
            started: Instant::now(),
            request_bytes,
            input_items: context.input_items,
            has_previous_response_id: context.previous_response_id.is_some(),
            warmup: context.warmup,
            connection_reused: context.connection_reused,
            outcome: "cancelled",
            response_created: false,
            os_error: None,
            send_ms: None,
            first_message_ms: None,
            close_code: None,
            close_reason_category: "none",
            close_reason_bytes: 0,
        };
        tracing::info!(
            target: "codex_api::ws_diagnostics",
            ws_request_id = diagnostics.request_id,
            ws_connection_id = diagnostics.connection_id,
            request_bytes,
            input_items = diagnostics.input_items,
            has_previous_response_id = diagnostics.has_previous_response_id,
            connection_reused = diagnostics.connection_reused,
            warmup = diagnostics.warmup,
            "websocket request started"
        );
        diagnostics
    }

    pub(super) fn observe_send(&mut self, elapsed: Duration, success: bool) {
        self.send_ms = Some(elapsed.as_millis() as u64);
        tracing::info!(
            target: "codex_api::ws_diagnostics",
            ws_request_id = self.request_id,
            ws_connection_id = self.connection_id,
            send_ms = self.send_ms,
            success,
            "websocket request send finished"
        );
    }

    pub(super) fn observe_message(&mut self) {
        self.first_message_ms
            .get_or_insert_with(|| self.started.elapsed().as_millis() as u64);
    }

    pub(super) fn observe_close(&mut self, frame: Option<&CloseFrame>) {
        self.outcome = "server_close";
        if let Some(frame) = frame {
            self.close_code = Some(frame.code.into());
            self.close_reason_bytes = frame.reason.len();
            self.close_reason_category = close_reason_category(&frame.reason);
        }
    }
}

impl Drop for RequestDiagnostics {
    fn drop(&mut self) {
        // Repeat request metadata on the terminal event so a bounded log window can still explain
        // a slow failure even if its start event has already been evicted. Drop also covers cancel.
        tracing::info!(
            target: "codex_api::ws_diagnostics",
            ws_request_id = self.request_id,
            ws_connection_id = self.connection_id,
            request_bytes = self.request_bytes,
            input_items = self.input_items,
            has_previous_response_id = self.has_previous_response_id,
            connection_reused = self.connection_reused,
            warmup = self.warmup,
            outcome = self.outcome,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            send_ms = self.send_ms,
            first_message_ms = self.first_message_ms,
            response_created = self.response_created,
            os_error = self.os_error,
            close_code = self.close_code,
            close_reason_category = self.close_reason_category,
            close_reason_bytes = self.close_reason_bytes,
            "websocket request finished"
        );
    }
}

pub(super) fn close_reason_category(reason: &str) -> &'static str {
    // Close frames are bounded control frames. Never expose arbitrary peer text.
    let reason = reason.to_ascii_lowercase();
    if reason.is_empty() {
        "empty"
    } else if reason.contains("too large") || reason.contains("size limit") {
        "size_limit"
    } else if reason.contains("connection limit") || reason.contains("60 minutes") {
        "connection_lifetime_limit"
    } else if reason.contains("timeout") || reason.contains("timed out") {
        "timeout"
    } else if reason.contains("rate limit") {
        "rate_limit"
    } else if reason.contains("overload") || reason.contains("try again") {
        "overload"
    } else {
        "other_redacted"
    }
}

#[cfg(test)]
#[path = "responses_websocket_diagnostics_tests.rs"]
mod tests;
