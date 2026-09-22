//! Retain dynamic tool state in either renderer; the app emits only finished rows to terminal history.

use super::*;

impl ChatWidget {
    pub(super) fn on_dynamic_tool_item(&mut self, item: ThreadItem) {
        let started = matches!(
            &item,
            ThreadItem::DynamicToolCall {
                status: codex_app_server_protocol::DynamicToolCallStatus::InProgress,
                ..
            }
        );
        self.defer_or_handle(
            item,
            |interrupts, item| {
                if started {
                    interrupts.push_item_started(item);
                } else {
                    interrupts.push_item_completed(item);
                }
            },
            Self::handle_dynamic_tool_item_now,
        );
    }

    pub(crate) fn handle_dynamic_tool_item_now(&mut self, item: ThreadItem) {
        match item {
            item @ ThreadItem::DynamicToolCall {
                status: codex_app_server_protocol::DynamicToolCallStatus::InProgress,
                ..
            } => self.handle_dynamic_tool_call_started_now(item),
            item @ ThreadItem::DynamicToolCall { .. } => {
                self.handle_dynamic_tool_call_completed_now(item);
            }
            _ => {}
        }
    }

    pub(super) fn finish_dynamic_activity(&mut self) {
        for (_, cell) in self.transcript.dynamic_calls.drain() {
            cell.mark_interrupted();
        }
    }
}
