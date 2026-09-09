//! Last-observed picker activity, not a count of execution or residency slots.
//!
//! A missing/closed thread is labeled unloaded, not completed: unloading preserves history and
//! the picker cannot infer the outcome or the backend's resource ownership from that alone.

use super::App;
use super::agent_picker::AGENT_PICKER_VIEW_ID;
use crate::multi_agents::AgentPickerThreadEntry;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::ThreadId;
use ratatui::style::Stylize;
use ratatui::text::Span;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AgentPickerStatus {
    Running,
    Completed,
    Interrupted,
    Failed,
    Idle,
    NotLoaded,
    Unknown,
}

impl AgentPickerStatus {
    pub(super) fn prefix(self) -> Vec<Span<'static>> {
        let label = match self {
            Self::Running => "[running]".cyan(),
            Self::Completed => "[done]".green(),
            Self::Interrupted => "[interrupted]".dim(),
            Self::Failed => "[error]".red(),
            Self::Idle => "[idle]".dim(),
            Self::NotLoaded => "[not loaded]".dim(),
            Self::Unknown => "[unknown]".dim(),
        };
        let padding = " ".repeat(14 - label.content.len());
        vec![label, padding.into()]
    }
}

impl App {
    pub(super) fn agent_picker_status(
        &self,
        thread_id: ThreadId,
        entry: &AgentPickerThreadEntry,
    ) -> AgentPickerStatus {
        if entry.is_closed {
            return AgentPickerStatus::NotLoaded;
        }
        // Do not wait for a busy store or fetch full history just to paint the picker.
        let store = self
            .thread_event_channels
            .get(&thread_id)
            .and_then(|channel| channel.store.try_lock().ok());
        if store.is_none() && entry.is_running {
            return AgentPickerStatus::Running;
        }
        let Some(store) = store else {
            return AgentPickerStatus::Unknown;
        };
        match store.latest_thread_status.as_ref() {
            Some(ThreadStatus::NotLoaded) => return AgentPickerStatus::NotLoaded,
            Some(ThreadStatus::SystemError) => return AgentPickerStatus::Failed,
            Some(ThreadStatus::Active { .. }) => return AgentPickerStatus::Running,
            Some(ThreadStatus::Idle) | None => {}
        }
        if store.latest_thread_status.is_none()
            && (store.active_turn_id().is_some() || entry.is_running)
        {
            return AgentPickerStatus::Running;
        }
        if store.thread_status_supersedes_turn {
            return AgentPickerStatus::Idle;
        }
        match store.latest_turn_status.as_ref() {
            Some(TurnStatus::Completed) => AgentPickerStatus::Completed,
            Some(TurnStatus::Interrupted) => AgentPickerStatus::Interrupted,
            Some(TurnStatus::Failed) => AgentPickerStatus::Failed,
            Some(TurnStatus::InProgress) | None => AgentPickerStatus::Idle,
        }
    }

    pub(super) fn repaint_agent_picker(&mut self) {
        if let Some(selected) = self
            .chat_widget
            .selected_index_for_present_view(AGENT_PICKER_VIEW_ID)
        {
            let params = self.agent_picker_selection_view_params(Some(selected));
            self.chat_widget
                .replace_selection_view_if_present(AGENT_PICKER_VIEW_ID, params);
        }
    }
}
