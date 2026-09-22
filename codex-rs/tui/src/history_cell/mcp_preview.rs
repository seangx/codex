//! Compact MCP and code-mode activity previews, independent of retained detail rendering.

use super::McpToolCallCell;
use super::NodeReplExecOutput;
use super::result::McpResultKind;
use crate::history_cell::activity_preview::DETAIL_PREVIEW_LINES;
use crate::history_cell::activity_preview::clipped_line;
use crate::live_wrap::take_prefix_by_width;
use crate::motion::MotionMode;
use crate::motion::ReducedMotionIndicator;
use crate::motion::activity_indicator;
use crate::style::accent_color;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::LogicalLineSource;
use ratatui::style::Stylize;
use ratatui::text::Line;
use std::borrow::Cow;
use std::collections::VecDeque;

impl McpToolCallCell {
    pub(super) fn compact_mcp_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        if width == 0 {
            return Vec::new();
        }
        let status = self.success();
        let (marker, verb) = match status {
            Some(true) => ("•".green().bold(), "Called"),
            Some(false) => ("•".red().bold(), "Failed"),
            None => (
                activity_indicator(
                    Some(self.start_time),
                    MotionMode::from_animations_enabled(self.animations_enabled),
                    ReducedMotionIndicator::StaticBullet,
                )
                .unwrap_or_else(|| "•".dim()),
                "Calling",
            ),
        };
        let title = self
            .invocation
            .arguments
            .as_ref()
            .filter(|_| self.result_kind() == McpResultKind::NodeRepl)
            .and_then(|arguments| arguments.get("title"))
            .and_then(serde_json::Value::as_str)
            .map(|title| title.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| format!("{}.{}", self.invocation.server, self.invocation.tool));
        let mut lines = vec![clipped_line(
            Line::from(vec![
                marker,
                " ".into(),
                verb.bold(),
                " ".into(),
                title.fg(accent_color()),
            ]),
            width,
        )];
        let mut details = VecDeque::with_capacity(DETAIL_PREVIEW_LINES);
        let push_tail = |details: &mut VecDeque<(String, LogicalLineSource)>,
                         text: &str,
                         first_prefix: &str| {
            for line in text
                .lines()
                .rev()
                .take(DETAIL_PREVIEW_LINES - details.len())
            {
                // Bound selected rows before allocating; retained output may be megabytes.
                let bounded = &line[..line.floor_char_boundary(16 * 1024)];
                let synthetic_prefix = if line.as_ptr() == text.as_ptr() {
                    first_prefix
                } else {
                    ""
                };
                let available = usize::from(width)
                    .saturating_sub(/*rhs*/ 4)
                    .saturating_sub(synthetic_prefix.len());
                let (visible_source, rest, _) = take_prefix_by_width(bounded, available);
                let truncated = !rest.is_empty() || bounded.len() < line.len();
                let visible_source = if truncated {
                    take_prefix_by_width(bounded, available.saturating_sub(/*rhs*/ 1)).0
                } else {
                    visible_source
                };
                let mut detail = synthetic_prefix.to_owned();
                detail.push_str(&visible_source);
                if truncated {
                    detail.push('…');
                }
                let mut source = LogicalLineSource::new(bounded.to_owned());
                source.range = 0..visible_source.len();
                source.prefix_bytes = synthetic_prefix.len();
                details.push_front((detail, source));
            }
        };
        if let Some(result) = &self.result {
            match result {
                Ok(result) => {
                    for block in result.content.iter().rev() {
                        let output = if self.result_kind() == McpResultKind::NodeRepl
                            && status == Some(true)
                            && let Some(text) = block.text()
                        {
                            if text.starts_with("Script completed\n")
                                && let Some((_, output)) = text.split_once("\nOutput:\n")
                            {
                                Cow::Borrowed(output)
                            } else if let Ok(output) =
                                serde_json::from_str::<NodeReplExecOutput>(text)
                                && output.exit_code == 0
                            {
                                Cow::Owned(output.output)
                            } else {
                                Cow::Borrowed(block.render_full())
                            }
                        } else {
                            Cow::Borrowed(block.render_full())
                        };
                        push_tail(&mut details, &output, "");
                        // A code-mode image can also carry text; its marker precedes that output.
                        if block.is_image
                            && block.text().is_some()
                            && details.len() < DETAIL_PREVIEW_LINES
                        {
                            push_tail(&mut details, "Returned image", "");
                        }
                        if details.len() == DETAIL_PREVIEW_LINES {
                            break;
                        }
                    }
                }
                Err(error) => {
                    let short_error = error.lines().take(DETAIL_PREVIEW_LINES + 1).count()
                        <= DETAIL_PREVIEW_LINES;
                    push_tail(
                        &mut details,
                        error,
                        if short_error { "Error: " } else { "" },
                    );
                    if short_error && details.is_empty() {
                        let mut source = LogicalLineSource::new(String::new());
                        source.range = 0..0;
                        source.prefix_bytes = "Error: ".len();
                        details.push_front(("Error: ".to_owned(), source));
                    }
                }
            }
        }
        for (index, (detail, mut source)) in details.into_iter().enumerate() {
            let gutter = if index == 0 { "  └ " } else { "    " };
            source.prefix_bytes += gutter.len();
            let line = Line::from(vec![gutter.dim(), detail.dim()]);
            if line.width() > usize::from(width) {
                lines.push(clipped_line(line, width));
            } else {
                lines.push(HyperlinkLine {
                    line,
                    hyperlinks: Vec::new(),
                    source: Some(source),
                });
            }
        }
        lines
    }
}
