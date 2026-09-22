//! Last-observed picker activity, not a count of execution or residency slots.
//!
//! A missing/closed thread is labeled unloaded, not completed: unloading preserves history and
//! the picker cannot infer the outcome or the backend's resource ownership from that alone.

use super::App;
use super::agent_navigation::AgentNavigationState;
use super::agent_picker::AGENT_HISTORY_PICKER_VIEW_ID;
use super::agent_picker::AGENT_PICKER_VIEW_ID;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::multi_agents::AgentPickerThreadEntry;
use crate::multi_agents::format_agent_picker_item_name;
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
    pub(super) fn agent_picker_selection_view_params(
        &self,
        selected: Option<usize>,
    ) -> SelectionViewParams {
        let mut initial_selected_idx = selected;
        let mut items = Vec::new();
        let mut history_count = 0;
        for (thread_id, entry) in self.agent_navigation.ordered_threads() {
            let status = self.agent_picker_status(thread_id, entry);
            if status == AgentPickerStatus::NotLoaded {
                history_count += 1;
                continue;
            }
            let idx = items.len();
            if initial_selected_idx.is_none() && self.active_thread_id == Some(thread_id) {
                initial_selected_idx = Some(idx);
            }
            items.push(self.agent_picker_thread_item(thread_id, entry, status));
        }
        if history_count > 0 {
            items.push(SelectionItem {
                name: "more...".to_string(),
                description: Some(format!("{history_count} unloaded history")),
                actions: vec![Box::new(|tx| tx.send(AppEvent::OpenAgentHistory))],
                dismiss_on_select: false,
                dismiss_parent_on_child_accept: true,
                search_value: Some("more history unloaded".to_string()),
                ..Default::default()
            });
        }

        SelectionViewParams {
            view_id: Some(AGENT_PICKER_VIEW_ID),
            title: Some("Subagents".to_string()),
            subtitle: Some(AgentNavigationState::picker_subtitle()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            initial_selected_idx,
            ..SelectionViewParams::picker()
        }
    }

    pub(super) fn agent_history_selection_view_params(
        &self,
        selected: Option<usize>,
    ) -> SelectionViewParams {
        let items = self
            .agent_navigation
            .ordered_threads()
            .into_iter()
            .filter_map(|(thread_id, entry)| {
                let status = self.agent_picker_status(thread_id, entry);
                (status == AgentPickerStatus::NotLoaded)
                    .then(|| self.agent_picker_thread_item(thread_id, entry, status))
            })
            .collect::<Vec<_>>();
        SelectionViewParams {
            view_id: Some(AGENT_HISTORY_PICKER_VIEW_ID),
            title: Some(format!("Subagent history ({})", items.len())),
            subtitle: Some("Unloaded agents; history remains available.".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            initial_selected_idx: selected,
            ..SelectionViewParams::picker()
        }
    }

    fn agent_picker_thread_item(
        &self,
        thread_id: ThreadId,
        entry: &AgentPickerThreadEntry,
        status: AgentPickerStatus,
    ) -> SelectionItem {
        let is_primary = self.primary_thread_id == Some(thread_id);
        let name = entry
            .agent_path
            .as_deref()
            .map(str::trim)
            .filter(|agent_path| !is_primary && !agent_path.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                format_agent_picker_item_name(
                    entry.agent_nickname.as_deref(),
                    entry.agent_role.as_deref(),
                    is_primary,
                )
            });
        let uuid = thread_id.to_string();
        SelectionItem {
            name: name.clone(),
            name_prefix_spans: status.prefix(),
            description: Some(uuid.clone()),
            is_current: self.active_thread_id == Some(thread_id),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::SelectAgentThread(thread_id));
            })],
            dismiss_on_select: true,
            search_value: Some(format!("{name} {uuid}")),
            ..Default::default()
        }
    }

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
        if let Some(selected) = self
            .chat_widget
            .selected_index_for_present_view(AGENT_HISTORY_PICKER_VIEW_ID)
        {
            let params = self.agent_history_selection_view_params(Some(selected));
            self.chat_widget
                .replace_selection_view_if_present(AGENT_HISTORY_PICKER_VIEW_ID, params);
        }
    }
}
