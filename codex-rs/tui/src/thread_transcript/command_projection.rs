//! Project persisted command executions through the regular exec history cells.

use std::sync::Arc;
use std::time::Duration;

use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::ThreadItem;

use super::TranscriptCells;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::new_active_exec_command;
use crate::exec_command::split_command_string;
use crate::history_cell::ToolActivityGroupCell;

/// Accumulates only within one projected item slice, so a pagination boundary never rewrites or
/// implicitly joins persisted history from two independently loaded pages.
pub(super) struct CommandExecutionCellProjector {
    pending_group: Option<ToolActivityGroupCell>,
}

impl CommandExecutionCellProjector {
    pub(super) fn new() -> Self {
        Self {
            pending_group: None,
        }
    }

    /// Returns whether `item` was a command execution and was projected.
    pub(super) fn push_if_command(
        &mut self,
        item: &ThreadItem,
        cells: &mut TranscriptCells,
    ) -> bool {
        let ThreadItem::CommandExecution {
            id,
            command,
            source,
            status,
            command_actions,
            aggregated_output,
            exit_code,
            duration_ms,
            ..
        } = item
        else {
            return false;
        };

        let command = split_command_string(command);
        let parsed = command_actions
            .iter()
            .cloned()
            .map(codex_app_server_protocol::CommandAction::into_core)
            .collect();
        let mut cell = new_active_exec_command(
            id.clone(),
            command,
            parsed,
            *source,
            /*interaction_input*/ None,
            /*animations_enabled*/ false,
        );

        let completed_exit_code = match status {
            CommandExecutionStatus::InProgress => None,
            CommandExecutionStatus::Completed => Some(exit_code.unwrap_or_default()),
            CommandExecutionStatus::Failed | CommandExecutionStatus::Declined => {
                Some(exit_code.filter(|code| *code != 0).unwrap_or(1))
            }
        };
        if let Some(completed_exit_code) = completed_exit_code {
            let duration = Duration::from_millis(duration_ms.unwrap_or_default().max(0) as u64);
            let output = CommandOutput::new(
                completed_exit_code,
                aggregated_output.clone().unwrap_or_default(),
            );
            let completed = cell.complete_call(id, output, duration);
            debug_assert!(completed, "new projected exec cell should contain {id}");
        } else if let Some(output) = aggregated_output.as_deref() {
            let appended = cell.append_output(id, output);
            debug_assert!(
                appended || output.is_empty(),
                "new projected exec cell should contain {id}"
            );
        }
        cell.freeze_snapshot();

        let groupable = matches!(
            source,
            CommandExecutionSource::Agent | CommandExecutionSource::UnifiedExecStartup
        ) && completed_exit_code == Some(0);
        if groupable {
            if let Some(group) = self.pending_group.as_mut() {
                group.push(cell);
            } else {
                self.pending_group = Some(ToolActivityGroupCell::new(cell));
            }
        } else {
            self.flush(cells);
            cells.push(Arc::new(cell));
        }
        true
    }

    pub(super) fn flush(&mut self, cells: &mut TranscriptCells) {
        if let Some(group) = self.pending_group.take() {
            cells.push(Arc::new(group));
        }
    }
}
