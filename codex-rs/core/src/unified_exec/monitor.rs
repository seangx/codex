use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::MissedTickBehavior;

use super::process::UnifiedExecProcess;
use crate::codex_thread::TryStartTurnIfIdleRejectionReason;
use crate::context::ContextualUserFragment;
use crate::context::MonitorEvent;
use crate::session::input_queue::TurnInput;
use crate::session::session::Session;

const MONITOR_BATCH_INTERVAL: Duration = Duration::from_millis(500);
const MONITOR_DEDUP_WINDOW: Duration = Duration::from_secs(2);
const MONITOR_MAX_BATCH_BYTES: usize = 3 * 1024;
const MONITOR_MAX_BATCH_LINES: usize = 20;
const MONITOR_MAX_DELIVERY_BYTES: usize = 32 * 1024;
const MONITOR_MAX_LINE_BYTES: usize = 1024;

struct DecodedLine {
    text: String,
    truncated: bool,
}

#[derive(Default)]
struct LineDecoder {
    pending: Vec<u8>,
    discard_until_newline: bool,
}

impl LineDecoder {
    fn push_chunk(&mut self, chunk: &[u8]) -> Vec<DecodedLine> {
        let mut lines = Vec::new();
        for byte in chunk {
            if self.discard_until_newline {
                if *byte == b'\n' {
                    self.discard_until_newline = false;
                }
                continue;
            }
            if *byte == b'\n' {
                lines.push(self.finish_line(/*truncated*/ false));
                continue;
            }
            if self.pending.len() < MONITOR_MAX_LINE_BYTES {
                self.pending.push(*byte);
                continue;
            }
            lines.push(self.finish_line(/*truncated*/ true));
            self.discard_until_newline = true;
        }
        lines
    }

    fn finish_partial(&mut self) -> Option<DecodedLine> {
        (!self.pending.is_empty()).then(|| self.finish_line(/*truncated*/ false))
    }

    fn finish_line(&mut self, truncated: bool) -> DecodedLine {
        if self.pending.last() == Some(&b'\r') {
            self.pending.pop();
        }
        let mut text = String::from_utf8_lossy(&self.pending).to_string();
        self.pending.clear();
        if truncated {
            text.push_str(" …[line truncated]");
        }
        DecodedLine { text, truncated }
    }
}

#[derive(Default)]
struct MonitorBatch {
    lines: Vec<String>,
    bytes: usize,
    omitted_lines: usize,
}

impl MonitorBatch {
    fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.omitted_lines == 0
    }

    fn push(&mut self, line: DecodedLine) {
        let line_bytes = line.text.len();
        if self.lines.len() >= MONITOR_MAX_BATCH_LINES
            || self.bytes.saturating_add(line_bytes) > MONITOR_MAX_BATCH_BYTES
        {
            self.omitted_lines = self.omitted_lines.saturating_add(1);
            return;
        }
        if line.truncated {
            self.omitted_lines = self.omitted_lines.saturating_add(1);
        }
        self.bytes = self.bytes.saturating_add(line_bytes);
        self.lines.push(line.text);
    }

    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

struct RecentLines {
    entries: VecDeque<(String, Instant)>,
}

impl RecentLines {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    fn is_duplicate(&mut self, line: &str) -> bool {
        let now = Instant::now();
        while self.entries.front().is_some_and(|(_, seen_at)| {
            now.saturating_duration_since(*seen_at) > MONITOR_DEDUP_WINDOW
        }) {
            self.entries.pop_front();
        }
        if self.entries.iter().any(|(seen, _)| seen == line) {
            return true;
        }
        self.entries.push_back((line.to_string(), now));
        false
    }
}

struct DeliveryBudget {
    remaining_bytes: usize,
    limit_notified: bool,
}

impl DeliveryBudget {
    fn new() -> Self {
        Self {
            remaining_bytes: MONITOR_MAX_DELIVERY_BYTES,
            limit_notified: false,
        }
    }

    async fn deliver_running(
        &mut self,
        session: &Arc<Session>,
        monitor_id: i32,
        batch: MonitorBatch,
    ) {
        if batch.is_empty() || self.limit_notified {
            return;
        }
        let event = MonitorEvent::running(monitor_id, batch.lines, batch.omitted_lines);
        let rendered_bytes = event.render().len();
        if rendered_bytes > self.remaining_bytes {
            self.limit_notified = true;
            deliver_monitor_event(session, MonitorEvent::delivery_limit_reached(monitor_id)).await;
            return;
        }
        self.remaining_bytes = self.remaining_bytes.saturating_sub(rendered_bytes);
        deliver_monitor_event(session, event).await;
    }
}

pub(crate) fn start_monitoring_output(
    process: Arc<UnifiedExecProcess>,
    session: Arc<Session>,
    monitor_id: i32,
) {
    let mut receiver = process.output_receiver();
    let exit_token = process.cancellation_token();
    tokio::spawn(async move {
        let mut decoder = LineDecoder::default();
        let mut batch = MonitorBatch::default();
        let mut recent_lines = RecentLines::new();
        let mut delivery_budget = DeliveryBudget::new();
        let mut flush_interval = tokio::time::interval(MONITOR_BATCH_INTERVAL);
        flush_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        flush_interval.tick().await;

        for chunk in process.snapshot_output().await {
            let decoded = decoder.push_chunk(&chunk);
            push_decoded_lines(&mut batch, &mut recent_lines, decoded);
        }

        loop {
            tokio::select! {
                _ = flush_interval.tick() => {
                    delivery_budget
                        .deliver_running(&session, monitor_id, batch.take())
                        .await;
                }
                received = receiver.recv() => {
                    let chunk = match received {
                        Ok(chunk) => chunk,
                        Err(RecvError::Lagged(skipped)) => {
                            batch.omitted_lines = batch
                                .omitted_lines
                                .saturating_add(usize::try_from(skipped).unwrap_or(usize::MAX));
                            continue;
                        }
                        Err(RecvError::Closed) => break,
                    };
                    let decoded = decoder.push_chunk(&chunk);
                    push_decoded_lines(&mut batch, &mut recent_lines, decoded);
                }
                _ = exit_token.cancelled() => {
                    tokio::time::sleep(super::async_watcher::TRAILING_OUTPUT_GRACE).await;
                    break;
                }
            }
        }

        loop {
            match receiver.try_recv() {
                Ok(chunk) => {
                    let decoded = decoder.push_chunk(&chunk);
                    push_decoded_lines(&mut batch, &mut recent_lines, decoded);
                }
                Err(
                    tokio::sync::broadcast::error::TryRecvError::Empty
                    | tokio::sync::broadcast::error::TryRecvError::Closed,
                ) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    batch.omitted_lines = batch
                        .omitted_lines
                        .saturating_add(usize::try_from(skipped).unwrap_or(usize::MAX));
                }
            }
        }

        if let Some(line) = decoder.finish_partial()
            && !recent_lines.is_duplicate(&line.text)
        {
            batch.push(line);
        }
        let batch = batch.take();
        deliver_monitor_event(
            &session,
            MonitorEvent::exited(
                monitor_id,
                process.exit_code(),
                batch.lines,
                batch.omitted_lines,
            ),
        )
        .await;
    });
}

fn push_decoded_lines(
    batch: &mut MonitorBatch,
    recent_lines: &mut RecentLines,
    lines: Vec<DecodedLine>,
) {
    for line in lines {
        if recent_lines.is_duplicate(&line.text) {
            batch.omitted_lines = batch.omitted_lines.saturating_add(1);
        } else {
            batch.push(line);
        }
    }
}

async fn deliver_monitor_event(session: &Arc<Session>, event: MonitorEvent) {
    let items = vec![ContextualUserFragment::into(event)];
    let items = match session.inject_if_running(items).await {
        Ok(()) => return,
        Err(items) => items,
    };
    let turn_input = items.iter().cloned().map(TurnInput::ResponseItem).collect();
    match session.try_start_turn_if_idle(turn_input).await {
        Ok(()) => {}
        Err(err) => {
            let reason = err.reason();
            if reason == TryStartTurnIfIdleRejectionReason::Busy {
                match session.inject_if_running(items).await {
                    Ok(()) => (),
                    Err(items) => session.inject_no_new_turn(items, None).await,
                }
            } else {
                session.inject_no_new_turn(items, None).await;
            }
        }
    }
}

#[cfg(test)]
#[path = "monitor_tests.rs"]
mod tests;
