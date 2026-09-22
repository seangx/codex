//! Command execution lifecycle handlers for `ChatWidget`.
//!
//! This module owns command start/output/completion rendering, including active
//! exec-cell grouping and unified exec wait state.

use super::*;
use crate::exec_cell::CommandOutput;

impl ChatWidget {
    pub(super) fn flush_unified_exec_wait_streak(&mut self) {
        let Some(wait) = self.unified_exec_wait_streak.take() else {
            return;
        };
        let cell = history_cell::new_unified_exec_interaction(wait.command_display, String::new());
        if let Err(cell) = self.absorb_activity_detail(Box::new(cell)) {
            self.app_event_tx.send(AppEvent::InsertHistoryCell(cell));
        }
        self.restore_reasoning_status_header();
    }

    pub(super) fn on_command_execution_started(&mut self, item: ThreadItem) {
        let ThreadItem::CommandExecution {
            id,
            command,
            process_id,
            source,
            command_actions,
            ..
        } = &item
        else {
            return;
        };
        let (_command, parsed_cmd) = command_execution_command_and_parsed(command, command_actions);
        self.flush_answer_stream_with_separator();
        if is_unified_exec_source(*source) {
            if *source == ExecCommandSource::UnifiedExecStartup {
                self.track_unified_exec_process_begin(id, process_id.as_deref(), command);
            }
            if !self.bottom_pane.is_task_running() {
                return;
            }
            // Unified exec may be parsed as Unknown; keep the working indicator visible regardless.
            self.bottom_pane.ensure_status_indicator();
            if !is_standard_tool_call(&parsed_cmd) {
                return;
            }
        }
        self.defer_or_handle(
            item,
            InterruptManager::push_item_started,
            Self::handle_command_execution_started_now,
        );
    }

    pub(super) fn on_exec_command_output_delta(&mut self, call_id: &str, delta: &str) {
        self.track_unified_exec_output_chunk(call_id, delta.as_bytes());
        if !self.bottom_pane.is_task_running() {
            return;
        }

        let appended = if let Some(cell) = self
            .transcript
            .active_cell
            .as_mut()
            .and_then(|c| c.as_any_mut().downcast_mut::<ExecCell>())
        {
            cell.append_output(call_id, delta)
        } else if let Some(group) = self
            .transcript
            .active_cell
            .as_mut()
            .and_then(|c| c.as_any_mut().downcast_mut::<ToolActivityGroupCell>())
        {
            group
                .exec_for_call_mut(call_id)
                .is_some_and(|cell| cell.append_output(call_id, delta))
        } else {
            false
        };

        if appended {
            self.bump_active_cell_revision();
            self.request_redraw();
        }
    }

    pub(super) fn on_terminal_interaction(&mut self, process_id: String, stdin: String) {
        if !self.bottom_pane.is_task_running() {
            return;
        }
        let command_display = self
            .unified_exec_processes
            .iter()
            .find(|process| process.key == process_id)
            .map(|process| process.command_display.clone());
        if stdin.is_empty() && command_display.is_none() {
            return;
        }

        self.flush_answer_stream_with_separator();
        if stdin.is_empty() {
            // Empty stdin means we are polling for background output.
            // Surface this in the status indicator (single "waiting" surface) instead of
            // the transcript. Keep the header short so the interrupt hint remains visible.
            self.bottom_pane.ensure_status_indicator();
            self.bottom_pane
                .set_interrupt_hint_visible(/*visible*/ true);
            self.status_state.terminal_title_status_kind =
                TerminalTitleStatusKind::WaitingForBackgroundTerminal;
            self.set_status(
                "Waiting for background terminal".to_string(),
                command_display.clone(),
                StatusDetailsCapitalization::Preserve,
                /*details_max_lines*/ 1,
            );
            match &mut self.unified_exec_wait_streak {
                Some(wait) if wait.process_id == process_id => {
                    wait.update_command_display(command_display);
                }
                Some(_) => {
                    self.flush_unified_exec_wait_streak();
                    self.unified_exec_wait_streak =
                        Some(UnifiedExecWaitStreak::new(process_id, command_display));
                }
                None => {
                    self.unified_exec_wait_streak =
                        Some(UnifiedExecWaitStreak::new(process_id, command_display));
                }
            }
            self.request_redraw();
        } else {
            if self
                .unified_exec_wait_streak
                .as_ref()
                .is_some_and(|wait| wait.process_id == process_id)
            {
                self.flush_unified_exec_wait_streak();
            }
            self.add_to_history(history_cell::new_unified_exec_interaction(
                command_display,
                stdin,
            ));
        }
    }

    pub(super) fn on_command_execution_completed(&mut self, item: ThreadItem) {
        let ThreadItem::CommandExecution {
            id,
            process_id,
            source,
            ..
        } = &item
        else {
            return;
        };
        if is_unified_exec_source(*source) {
            if let Some(process_id) = process_id.as_deref()
                && self
                    .unified_exec_wait_streak
                    .as_ref()
                    .is_some_and(|wait| wait.process_id == process_id)
            {
                self.flush_unified_exec_wait_streak();
            }
            self.track_unified_exec_process_end(id, process_id.as_deref());
            if !self.bottom_pane.is_task_running() {
                return;
            }
        }
        self.defer_or_handle(
            item,
            InterruptManager::push_item_completed,
            Self::handle_command_execution_completed_now,
        );
    }

    pub(super) fn track_unified_exec_process_begin(
        &mut self,
        call_id: &str,
        process_id: Option<&str>,
        command: &str,
    ) {
        let key = process_id.unwrap_or(call_id).to_string();
        let command = split_command_string(command);
        let command_display = strip_bash_lc_and_escape(&command);
        if let Some(existing) = self
            .unified_exec_processes
            .iter_mut()
            .find(|process| process.key == key)
        {
            existing.call_id = call_id.to_string();
            existing.command_display = command_display;
            existing.recent_chunks.clear();
        } else {
            self.unified_exec_processes.push(UnifiedExecProcessSummary {
                key,
                call_id: call_id.to_string(),
                command_display,
                recent_chunks: Vec::new(),
            });
        }
        self.sync_unified_exec_footer();
    }

    pub(super) fn track_unified_exec_process_end(
        &mut self,
        call_id: &str,
        process_id: Option<&str>,
    ) {
        let key = process_id.unwrap_or(call_id);
        let before = self.unified_exec_processes.len();
        self.unified_exec_processes
            .retain(|process| process.key != key);
        if self.unified_exec_processes.len() != before {
            self.sync_unified_exec_footer();
        }
    }

    pub(super) fn sync_unified_exec_footer(&mut self) {
        let processes = self
            .unified_exec_processes
            .iter()
            .map(|process| process.command_display.clone())
            .collect();
        self.bottom_pane.set_unified_exec_processes(processes);
    }

    /// Record recent stdout/stderr lines for the unified exec footer.
    pub(super) fn track_unified_exec_output_chunk(&mut self, call_id: &str, chunk: &[u8]) {
        let Some(process) = self
            .unified_exec_processes
            .iter_mut()
            .find(|process| process.call_id == call_id)
        else {
            return;
        };

        let text = String::from_utf8_lossy(chunk);
        for line in text
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
        {
            process.recent_chunks.push(line.to_string());
        }

        const MAX_RECENT_CHUNKS: usize = 3;
        if process.recent_chunks.len() > MAX_RECENT_CHUNKS {
            let drop_count = process.recent_chunks.len() - MAX_RECENT_CHUNKS;
            process.recent_chunks.drain(0..drop_count);
        }
    }

    pub(crate) fn handle_command_execution_started_now(&mut self, item: ThreadItem) {
        let ThreadItem::CommandExecution {
            id,
            command,
            source,
            command_actions,
            ..
        } = item
        else {
            return;
        };
        let (command, parsed_cmd) =
            command_execution_command_and_parsed(&command, &command_actions);
        // Ensure the status indicator is visible while the command runs.
        self.bottom_pane.ensure_status_indicator();
        let parsed_cmd = self.annotate_skill_reads_in_parsed_cmd(parsed_cmd);
        // Code-mode dynamic tools execute their nested shell calls through the same app-server
        // UserShell source used by explicit `!` commands. While an `exec` wrapper is active,
        // normalize those nested events into regular agent tool activity so they stay in the
        // compact group instead of flushing a full command cell into the main transcript.
        let source = if source == ExecCommandSource::UserShell
            && self
                .transcript
                .active_cell
                .as_ref()
                .and_then(|cell| cell.as_any().downcast_ref::<ToolActivityGroupCell>())
                .is_some_and(ToolActivityGroupCell::has_active_dynamic_exec)
        {
            ExecCommandSource::Agent
        } else {
            source
        };
        self.running_commands.insert(
            id.clone(),
            RunningCommand {
                command: command.clone(),
                parsed_cmd: parsed_cmd.clone(),
                source,
            },
        );
        let is_wait_interaction = matches!(source, ExecCommandSource::UnifiedExecInteraction);
        let command_display = command.join(" ");
        let should_suppress_unified_wait = is_wait_interaction
            && self
                .last_unified_wait
                .as_ref()
                .is_some_and(|wait| wait.is_duplicate(&command_display));
        if is_wait_interaction {
            self.last_unified_wait = Some(UnifiedExecWaitState::new(command_display));
        } else {
            self.last_unified_wait = None;
        }
        if should_suppress_unified_wait {
            self.suppressed_exec_calls.insert(id);
            return;
        }
        if source == ExecCommandSource::UserShell {
            self.flush_active_cell();
            self.transcript.active_cell = Some(Box::new(new_active_exec_command(
                id,
                command,
                parsed_cmd,
                source,
                /*interaction_input*/ None,
                self.local_settings.tui.animations && self.local_settings.tui.effects.progress,
            )));
            self.bump_active_cell_revision();
        } else {
            let added_to_last_exec = self
                .transcript
                .active_cell
                .as_mut()
                .and_then(|c| c.as_any_mut().downcast_mut::<ToolActivityGroupCell>())
                .and_then(ToolActivityGroupCell::last_exec_mut)
                .is_some_and(|cell| {
                    cell.add_call(
                        id.clone(),
                        command.clone(),
                        parsed_cmd.clone(),
                        source,
                        /*interaction_input*/ None,
                    )
                });
            if added_to_last_exec {
                self.bump_active_cell_revision();
            } else {
                self.push_tool_activity(new_active_exec_command(
                    id,
                    command,
                    parsed_cmd,
                    source,
                    /*interaction_input*/ None,
                    self.local_settings.tui.animations,
                ));
            }
        }

        self.request_redraw();
    }

    /// Finalizes an exec call while preserving the active exec cell grouping contract.
    ///
    /// Exec begin/end events usually pair through `running_commands`, but unified exec can emit an
    /// end event for a call that was never materialized as the current active `ExecCell` (for
    /// example, when another exploring group is still active). In that case we render the end as a
    /// standalone history entry instead of replacing or flushing the unrelated active exploring
    /// cell. If this method treated every unknown end as "complete the active cell", the UI could
    /// merge unrelated commands and hide still-running exploring work.
    pub(crate) fn handle_command_execution_completed_now(&mut self, item: ThreadItem) {
        enum ExecEndTarget {
            // Normal case: the active exec cell already tracks this call id.
            DirectTracked,
            // The active compact tool group tracks this call id.
            GroupTracked,
            // We have an active exec group, but it does not contain this call id. Render the end
            // as a standalone finalized history cell so the active group remains intact.
            OrphanHistoryWhileDirectExec,
            // Append an unmatched end to the active compact tool group.
            AppendToGroup,
            // No active exec cell can safely own this end; build a new cell from the end payload.
            NewCell,
        }

        let ThreadItem::CommandExecution {
            id,
            command,
            process_id: _,
            source,
            status,
            command_actions,
            aggregated_output,
            exit_code,
            duration_ms,
            ..
        } = item
        else {
            return;
        };
        let event_command = split_command_string(&command);
        let event_parsed = command_actions
            .into_iter()
            .map(codex_app_server_protocol::CommandAction::into_core)
            .collect();
        let duration = Duration::from_millis(duration_ms.unwrap_or_default().max(0) as u64);
        let exit_code = if status == codex_app_server_protocol::CommandExecutionStatus::Completed {
            exit_code.unwrap_or_default()
        } else {
            exit_code.filter(|code| *code != 0).unwrap_or(1)
        };
        let aggregated_output = aggregated_output.unwrap_or_default();

        let running = self.running_commands.remove(&id);
        let was_running = running.is_some();
        if self.suppressed_exec_calls.remove(&id) {
            return;
        }
        let (command, parsed, source) = match running {
            Some(rc) => (rc.command, rc.parsed_cmd, rc.source),
            None => (event_command, event_parsed, source),
        };
        let parsed = self.annotate_skill_reads_in_parsed_cmd(parsed);
        let is_unified_exec_interaction =
            matches!(source, ExecCommandSource::UnifiedExecInteraction);
        let is_user_shell = source == ExecCommandSource::UserShell;
        // Completion-only replay has no begin event to join adjacent exploration. Extend only
        // a finished, compatible group; an unrelated running group must retain orphan routing.
        if !was_running
            && let Some(cell) = self
                .transcript
                .active_cell
                .as_mut()
                .and_then(|cell| cell.as_any_mut().downcast_mut::<ExecCell>())
            && !cell.is_active()
            && !cell.should_flush()
            && !cell.iter_calls().any(|call| call.call_id == id)
        {
            cell.add_call(
                id.clone(),
                command.clone(),
                parsed.clone(),
                source,
                /*interaction_input*/ None,
            );
        }
        let end_target = match self.transcript.active_cell.as_ref() {
            Some(cell) => {
                if let Some(exec_cell) = cell.as_any().downcast_ref::<ExecCell>() {
                    if exec_cell.iter_calls().any(|call| call.call_id == id) {
                        ExecEndTarget::DirectTracked
                    } else if exec_cell.is_active() {
                        ExecEndTarget::OrphanHistoryWhileDirectExec
                    } else {
                        ExecEndTarget::NewCell
                    }
                } else if let Some(group) = cell.as_any().downcast_ref::<ToolActivityGroupCell>() {
                    if group.exec_for_call(&id).is_some() {
                        ExecEndTarget::GroupTracked
                    } else if group.is_active() && source != ExecCommandSource::UnifiedExecStartup {
                        ExecEndTarget::OrphanHistoryWhileDirectExec
                    } else {
                        ExecEndTarget::AppendToGroup
                    }
                } else if cell.as_any().is::<McpToolCallCell>()
                    || cell
                        .as_any()
                        .downcast_ref::<history_cell::ComputerActivityCell>()
                        .is_some_and(history_cell::ComputerActivityCell::is_active)
                {
                    ExecEndTarget::OrphanHistoryWhileDirectExec
                } else {
                    ExecEndTarget::NewCell
                }
            }
            None => ExecEndTarget::NewCell,
        };

        let output = if is_unified_exec_interaction {
            CommandOutput::new(exit_code, String::new())
        } else {
            CommandOutput::new(exit_code, aggregated_output)
        };

        match end_target {
            ExecEndTarget::DirectTracked => {
                let has_active_hook = self
                    .active_hook_cell
                    .as_ref()
                    .is_some_and(HookCell::has_visible_running_run);
                if let Some(cell) = self
                    .transcript
                    .active_cell
                    .as_mut()
                    .and_then(|c| c.as_any_mut().downcast_mut::<ExecCell>())
                {
                    let completed = cell.complete_call(&id, output, duration);
                    debug_assert!(completed, "active exec cell should contain {id}");
                    if cell.should_flush() || (has_active_hook && !cell.is_active()) {
                        self.flush_active_cell();
                    } else {
                        self.bump_active_cell_revision();
                        self.request_redraw();
                    }
                }
            }
            ExecEndTarget::GroupTracked => {
                let has_active_hook = self
                    .active_hook_cell
                    .as_ref()
                    .is_some_and(HookCell::has_visible_running_run);
                let should_flush = if let Some(group) = self
                    .transcript
                    .active_cell
                    .as_mut()
                    .and_then(|c| c.as_any_mut().downcast_mut::<ToolActivityGroupCell>())
                {
                    let completed = group
                        .exec_for_call_mut(&id)
                        .is_some_and(|cell| cell.complete_call(&id, output, duration));
                    debug_assert!(completed, "active tool group should contain {id}");
                    group.should_flush() || (has_active_hook && !group.is_active())
                } else {
                    false
                };
                if should_flush {
                    self.flush_active_cell();
                } else {
                    self.bump_active_cell_revision();
                    self.request_redraw();
                }
            }
            ExecEndTarget::OrphanHistoryWhileDirectExec => {
                let mut orphan = new_active_exec_command(
                    id.clone(),
                    command,
                    parsed,
                    source,
                    /*interaction_input*/ None,
                    self.local_settings.tui.animations && self.local_settings.tui.effects.progress,
                );
                let completed = orphan.complete_call(&id, output, duration);
                debug_assert!(completed, "new orphan exec cell should contain {id}");
                self.app_event_tx
                    .send(AppEvent::InsertHistoryCell(Box::new(orphan)));
                self.request_redraw();
            }
            ExecEndTarget::AppendToGroup => {
                let mut cell = new_active_exec_command(
                    id.clone(),
                    command,
                    parsed,
                    source,
                    /*interaction_input*/ None,
                    self.local_settings.tui.animations && self.local_settings.tui.effects.progress,
                );
                let completed = cell.complete_call(&id, output, duration);
                debug_assert!(completed, "new grouped exec cell should contain {id}");
                if cell.should_flush() {
                    self.flush_active_cell();
                    self.add_to_history(cell);
                } else {
                    self.push_tool_activity(cell);
                    self.request_redraw();
                }
            }
            ExecEndTarget::NewCell => {
                let mut cell = new_active_exec_command(
                    id.clone(),
                    command,
                    parsed,
                    source,
                    /*interaction_input*/ None,
                    self.local_settings.tui.animations && self.local_settings.tui.effects.progress,
                );
                let completed = cell.complete_call(&id, output, duration);
                debug_assert!(completed, "new exec cell should contain {id}");
                if is_user_shell || cell.should_flush() {
                    self.add_to_history(cell);
                } else {
                    self.push_tool_activity(cell);
                    self.request_redraw();
                }
            }
        }
        if is_user_shell {
            self.maybe_send_next_queued_input();
        }
    }
}
