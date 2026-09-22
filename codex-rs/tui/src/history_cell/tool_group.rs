//! Compact transcript cell for consecutive tool activity.

use super::*;
use crate::exec_cell::ExecCell;
use codex_protocol::parse_command::ParsedCommand;

const TOOL_SUMMARY_ORANGE: Color = Color::LightYellow;

#[derive(Debug)]
pub(crate) struct ToolActivityGroupCell {
    cells: Vec<Box<dyn HistoryCell>>,
    start_time: Instant,
}

impl ToolActivityGroupCell {
    pub(crate) fn new(cell: impl HistoryCell + 'static) -> Self {
        Self {
            cells: vec![Box::new(cell)],
            start_time: Instant::now(),
        }
    }

    pub(crate) fn push(&mut self, cell: impl HistoryCell + 'static) {
        self.cells.push(Box::new(cell));
    }

    pub(crate) fn last_exec_mut(&mut self) -> Option<&mut ExecCell> {
        self.cells
            .last_mut()
            .and_then(|cell| cell.as_any_mut().downcast_mut::<ExecCell>())
    }

    pub(crate) fn exec_for_call(&self, call_id: &str) -> Option<&ExecCell> {
        self.cells.iter().rev().find_map(|cell| {
            let exec = cell.as_any().downcast_ref::<ExecCell>()?;
            let contains_call = exec.iter_calls().any(|call| call.call_id == call_id);
            contains_call.then_some(exec)
        })
    }

    pub(crate) fn exec_for_call_mut(&mut self, call_id: &str) -> Option<&mut ExecCell> {
        self.cells.iter_mut().rev().find_map(|cell| {
            let exec = cell.as_any_mut().downcast_mut::<ExecCell>()?;
            let contains_call = exec.iter_calls().any(|call| call.call_id == call_id);
            contains_call.then_some(exec)
        })
    }

    pub(crate) fn mcp_for_call_mut(&mut self, call_id: &str) -> Option<&mut McpToolCallCell> {
        self.cells.iter_mut().rev().find_map(|cell| {
            let tool = cell.as_any_mut().downcast_mut::<McpToolCallCell>()?;
            (tool.call_id() == call_id).then_some(tool)
        })
    }

    pub(crate) fn web_search_for_call_mut(&mut self, call_id: &str) -> Option<&mut WebSearchCell> {
        self.cells.iter_mut().rev().find_map(|cell| {
            let search = cell.as_any_mut().downcast_mut::<WebSearchCell>()?;
            (search.call_id() == call_id).then_some(search)
        })
    }

    pub(crate) fn dynamic_for_call_mut(
        &mut self,
        call_id: &str,
    ) -> Option<&mut DynamicToolCallCell> {
        self.cells.iter_mut().rev().find_map(|cell| {
            let tool = cell.as_any_mut().downcast_mut::<DynamicToolCallCell>()?;
            (tool.call_id() == call_id).then_some(tool)
        })
    }

    pub(crate) fn has_active_dynamic_exec(&self) -> bool {
        self.cells.iter().any(|cell| {
            cell.as_any()
                .downcast_ref::<DynamicToolCallCell>()
                .is_some_and(|tool| tool.is_exec() && tool.is_active())
        })
    }

    pub(crate) fn is_active(&self) -> bool {
        self.summary().active
    }

    pub(crate) fn freeze_snapshot(&mut self) {
        for cell in &mut self.cells {
            if let Some(exec) = cell.as_any_mut().downcast_mut::<ExecCell>() {
                exec.freeze_snapshot();
            } else if let Some(tool) = cell.as_any_mut().downcast_mut::<McpToolCallCell>() {
                tool.freeze_snapshot();
            }
        }
    }

    pub(crate) fn should_flush(&self) -> bool {
        !self.is_active()
            && self.cells.iter().any(|cell| {
                cell.as_any()
                    .downcast_ref::<ExecCell>()
                    .is_some_and(ExecCell::should_flush)
                    || cell
                        .as_any()
                        .downcast_ref::<McpToolCallCell>()
                        .is_some_and(|tool| tool.success() == Some(false))
                    || cell
                        .as_any()
                        .downcast_ref::<DynamicToolCallCell>()
                        .is_some_and(DynamicToolCallCell::failed)
            })
    }

    pub(crate) fn mark_failed(&mut self) {
        for cell in &mut self.cells {
            if let Some(exec) = cell.as_any_mut().downcast_mut::<ExecCell>() {
                exec.mark_failed();
            } else if let Some(tool) = cell.as_any_mut().downcast_mut::<McpToolCallCell>() {
                tool.mark_failed();
            } else if let Some(search) = cell.as_any_mut().downcast_mut::<WebSearchCell>() {
                search.complete();
            } else if let Some(tool) = cell.as_any_mut().downcast_mut::<DynamicToolCallCell>() {
                tool.mark_interrupted();
            }
        }
    }

    fn summary(&self) -> ToolActivitySummary {
        let mut summary = ToolActivitySummary::default();
        let has_exec_cells = self.cells.iter().any(|cell| cell.as_any().is::<ExecCell>());
        for cell in &self.cells {
            if let Some(exec) = cell.as_any().downcast_ref::<ExecCell>() {
                summary.add_exec(exec);
            } else if let Some(tool) = cell.as_any().downcast_ref::<McpToolCallCell>() {
                summary.mcp_calls.push(tool.compact_summary());
                summary.active |= tool.is_active();
                summary.failed |= tool.success() == Some(false);
            } else if let Some(search) = cell.as_any().downcast_ref::<WebSearchCell>() {
                summary.web_searches += 1;
                summary.active |= !search.is_completed();
            } else if let Some(tool) = cell.as_any().downcast_ref::<DynamicToolCallCell>() {
                summary.add_dynamic_tool(tool, !has_exec_cells);
            }
        }
        summary
    }
}

impl HistoryCell for ToolActivityGroupCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if width == 0 {
            return Vec::new();
        }

        let summary = self.summary();
        let bullet = if summary.active {
            activity_indicator(
                Some(self.start_time),
                MotionMode::from_animations_enabled(self.transcript_animation_tick().is_some()),
                ReducedMotionIndicator::StaticBullet,
            )
            .unwrap_or_else(|| "•".dim())
        } else if summary.failed {
            "•".red().bold()
        } else {
            "•".dim()
        };
        let text = summary.render();
        let mut line = Line::from(vec![bullet, " ".into()]);
        let available = usize::from(width).saturating_sub(2);
        let (prefix, remainder, _) = take_prefix_by_width(&text, available);
        line.push_span(Span::styled(
            prefix,
            Style::default().fg(TOOL_SUMMARY_ORANGE),
        ));
        if !remainder.is_empty() && available > 0 {
            let (prefix, _, _) = take_prefix_by_width(&text, available.saturating_sub(1));
            line.spans.pop();
            line.push_span(Span::styled(
                prefix,
                Style::default().fg(TOOL_SUMMARY_ORANGE),
            ));
            line.push_span("…".dim());
        }
        vec![line]
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for cell in &self.cells {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(cell.raw_lines());
        }
        lines
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for cell in &self.cells {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(cell.transcript_lines(width));
        }
        lines
    }

    fn transcript_animation_tick(&self) -> Option<u64> {
        self.cells
            .iter()
            .filter_map(|cell| cell.transcript_animation_tick())
            .max()
    }
}

#[derive(Debug, Default)]
struct ToolActivitySummary {
    reads: usize,
    lists: usize,
    searches: usize,
    web_searches: usize,
    shell_commands: usize,
    mcp_calls: Vec<String>,
    dynamic_calls: Vec<String>,
    active: bool,
    failed: bool,
}

impl ToolActivitySummary {
    fn add_exec(&mut self, exec: &ExecCell) {
        self.active |= exec.is_active();
        self.failed |= exec.has_failed_call();
        for call in exec.iter_calls() {
            if call.parsed.is_empty()
                || call
                    .parsed
                    .iter()
                    .any(|parsed| matches!(parsed, ParsedCommand::Unknown { .. }))
            {
                self.shell_commands += 1;
                continue;
            }
            for parsed in &call.parsed {
                match parsed {
                    ParsedCommand::Read { .. } => self.reads += 1,
                    ParsedCommand::ListFiles { .. } => self.lists += 1,
                    ParsedCommand::Search { .. } => self.searches += 1,
                    ParsedCommand::Unknown { .. } => unreachable!("handled above"),
                }
            }
        }
    }

    fn add_dynamic_tool(&mut self, tool: &DynamicToolCallCell, count_nested_shell: bool) {
        self.active |= tool.is_active();
        self.failed |= tool.failed();
        let (display_name, nested_shell_commands) = tool.compact_summary();
        if tool.is_exec() && !count_nested_shell {
            return;
        }
        if nested_shell_commands > 0 {
            self.shell_commands += nested_shell_commands;
        } else {
            self.dynamic_calls.push(display_name);
        }
    }

    fn render(&self) -> String {
        let mut parts = Vec::new();
        if self.reads > 0 {
            parts.push(format!(
                "Read {} {}",
                self.reads,
                plural(self.reads, "file", "files")
            ));
        }
        if self.lists > 0 {
            parts.push(format!(
                "Listed {} {}",
                self.lists,
                plural(self.lists, "path", "paths")
            ));
        }
        if self.searches > 0 {
            parts.push(format!(
                "Searched for {} {}",
                self.searches,
                plural(self.searches, "pattern", "patterns")
            ));
        }
        if self.web_searches > 0 {
            parts.push(format!(
                "Searched the web {} {}",
                self.web_searches,
                plural(self.web_searches, "time", "times")
            ));
        }
        if !self.mcp_calls.is_empty() {
            let first = &self.mcp_calls[0];
            if self.mcp_calls.iter().all(|call| call == first) {
                let suffix = if self.mcp_calls.len() > 1 {
                    format!(" {} times", self.mcp_calls.len())
                } else {
                    Default::default()
                };
                parts.push(format!("Called {first}{suffix}"));
            } else {
                parts.push(format!("Called {} tools", self.mcp_calls.len()));
            }
        }
        if !self.dynamic_calls.is_empty() {
            let first = &self.dynamic_calls[0];
            if self.dynamic_calls.iter().all(|call| call == first) {
                let suffix = if self.dynamic_calls.len() > 1 {
                    format!(" {} times", self.dynamic_calls.len())
                } else {
                    Default::default()
                };
                parts.push(format!("Called {first}{suffix}"));
            } else {
                parts.push(format!("Called {} tools", self.dynamic_calls.len()));
            }
        }
        if self.shell_commands > 0 {
            parts.push(format!(
                "Ran {} shell {}",
                self.shell_commands,
                plural(self.shell_commands, "command", "commands")
            ));
        }
        if parts.is_empty() {
            return "Used tools".to_string();
        }
        parts
            .into_iter()
            .enumerate()
            .map(|(index, part)| {
                if index == 0 {
                    part
                } else {
                    lowercase_first(part)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn lowercase_first(value: String) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return value;
    };
    first.to_lowercase().chain(chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec_cell::new_active_exec_command;
    use codex_app_server_protocol::CommandExecutionSource as ExecCommandSource;
    use codex_app_server_protocol::ThreadItem;
    use pretty_assertions::assert_eq;

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn groups_tool_activity_into_one_summary_line() {
        let exec = new_active_exec_command(
            "shell".to_string(),
            vec!["make".to_string()],
            vec![ParsedCommand::Unknown {
                cmd: "make".to_string(),
            }],
            ExecCommandSource::Agent,
            None,
            false,
        );
        let mut group = ToolActivityGroupCell::new(exec);
        group.push(McpToolCallCell::new(
            "mcp".to_string(),
            McpInvocation {
                server: "plugin".to_string(),
                tool: "playwright".to_string(),
                arguments: None,
            },
            false,
        ));

        let lines = group.display_lines(120);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            line_text(&lines[0]),
            "• Called plugin.playwright, ran 1 shell command"
        );
        assert_eq!(lines[0].spans[2].style.fg, Some(TOOL_SUMMARY_ORANGE));
    }

    #[test]
    fn raw_lines_keep_each_tool_detail() {
        let exec = new_active_exec_command(
            "shell".to_string(),
            vec!["pwd".to_string()],
            vec![ParsedCommand::Unknown {
                cmd: "pwd".to_string(),
            }],
            ExecCommandSource::Agent,
            None,
            false,
        );
        let mut group = ToolActivityGroupCell::new(exec);
        group.push(McpToolCallCell::new(
            "mcp".to_string(),
            McpInvocation {
                server: "plugin".to_string(),
                tool: "playwright".to_string(),
                arguments: None,
            },
            false,
        ));

        let raw = group
            .raw_lines()
            .into_iter()
            .map(|line| line_text(&line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(raw.contains("$ pwd"));
        assert!(raw.contains("Calling plugin.playwright"));
    }

    #[test]
    fn dynamic_exec_counts_nested_shell_commands_in_summary() {
        let arguments = serde_json::json!(
            "const results = await Promise.all([tools.exec_command({}), \
             tools.exec_command({})]);"
        );
        let item = |status, content_items, success| ThreadItem::DynamicToolCall {
            id: "dynamic-1".to_string(),
            namespace: Some("functions".to_string()),
            tool: "exec".to_string(),
            arguments: arguments.clone(),
            status,
            content_items,
            success,
            duration_ms: None,
        };
        let tool = DynamicToolCallCell::from_item(item(
            codex_app_server_protocol::DynamicToolCallStatus::InProgress,
            None,
            None,
        ))
        .expect("dynamic tool item");
        let mut group = ToolActivityGroupCell::new(tool);
        let tool = group
            .dynamic_for_call_mut("dynamic-1")
            .expect("dynamic tool should exist");
        assert!(tool.update_from_item(item(
            codex_app_server_protocol::DynamicToolCallStatus::Completed,
            Some(vec![
                codex_app_server_protocol::DynamicToolCallOutputContentItem::InputText {
                    text: "large command output".to_string(),
                },
            ]),
            Some(true),
        )));

        let lines = group.display_lines(120);
        assert_eq!(line_text(&lines[0]), "• Ran 2 shell commands");
        let raw = group
            .raw_lines()
            .into_iter()
            .map(|line| line_text(&line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(raw.contains("tools.exec_command"));
        assert!(raw.contains("large command output"));
    }
}
