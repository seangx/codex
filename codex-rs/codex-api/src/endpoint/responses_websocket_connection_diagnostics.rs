//! Connection-scoped diagnostics remain active between response consumers. All causes are
//! bounded categories: never log peer text, frame payloads, or formatted transport errors.

use super::diagnostics::close_reason_category;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tracing::Span;

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

pub(super) struct ConnectionDiagnostics {
    pub(super) connection_id: u64,
    span: Span,
    started: Instant,
    last_received: Instant,
    keepalive_interval: Duration,
    pings_sent: u64,
    pongs_received: u64,
    outcome: &'static str,
    error_category: &'static str,
    io_error_kind: Option<std::io::ErrorKind>,
    os_error: Option<i32>,
    close_code: Option<u16>,
    close_reason_category: &'static str,
    close_reason_bytes: usize,
}

impl ConnectionDiagnostics {
    pub(super) fn new(keepalive_interval: Duration) -> Self {
        let diagnostics = Self {
            connection_id: NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed),
            // The pump outlives individual requests. Retain the connection owner's span so
            // idle closes still carry the thread attribution, including when the task aborts.
            span: Span::current(),
            started: Instant::now(),
            last_received: Instant::now(),
            keepalive_interval,
            pings_sent: 0,
            pongs_received: 0,
            outcome: "local_drop",
            error_category: "none",
            io_error_kind: None,
            os_error: None,
            close_code: None,
            close_reason_category: "none",
            close_reason_bytes: 0,
        };
        tracing::info!(
            target: "codex_api::ws_diagnostics",
            ws_connection_id = diagnostics.connection_id,
            keepalive_interval_ms = keepalive_interval.as_millis() as u64,
            "websocket connection established"
        );
        diagnostics
    }

    pub(super) fn finish(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }

    pub(super) fn received(&mut self) {
        self.last_received = Instant::now();
    }

    pub(super) fn ping_sent(&mut self) {
        self.pings_sent += 1;
    }

    pub(super) fn pong_received(&mut self) {
        self.pongs_received += 1;
    }

    pub(super) fn observe_error(&mut self, outcome: &'static str, error: &WsError) {
        self.outcome = outcome;
        self.error_category = match error {
            WsError::Io(error) => {
                self.io_error_kind = Some(error.kind());
                self.os_error = error.raw_os_error();
                "io"
            }
            WsError::ConnectionClosed => "connection_closed",
            WsError::AlreadyClosed => "already_closed",
            WsError::Protocol(_) => "protocol",
            WsError::Capacity(_) => "capacity",
            WsError::Tls(_) => "tls",
            WsError::WriteBufferFull(_) => "write_buffer_full",
            WsError::Utf8(_) => "utf8",
            WsError::AttackAttempt => "attack_attempt",
            WsError::Url(_) => "url",
            WsError::Http(_) => "http",
            WsError::HttpFormat(_) => "http_format",
        };
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

impl Drop for ConnectionDiagnostics {
    fn drop(&mut self) {
        self.span.in_scope(|| {
            tracing::info!(
                target: "codex_api::ws_diagnostics",
                ws_connection_id = self.connection_id,
                lifetime_ms = self.started.elapsed().as_millis() as u64,
                last_received_ms = self.last_received.elapsed().as_millis() as u64,
                keepalive_interval_ms = self.keepalive_interval.as_millis() as u64,
                pings_sent = self.pings_sent,
                pongs_received = self.pongs_received,
                outcome = self.outcome,
                error_category = self.error_category,
                io_error_kind = ?self.io_error_kind,
                os_error = self.os_error,
                close_code = self.close_code,
                close_reason_category = self.close_reason_category,
                close_reason_bytes = self.close_reason_bytes,
                "websocket connection finished"
            );
        });
    }
}
