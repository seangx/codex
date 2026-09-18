//! Patch summaries and image-tool transcript helpers.

use super::*;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::terminal_hyperlinks::terminal_name_for_link_rendering;
use codex_terminal_detection::TerminalName;
use codex_utils_path_uri::LegacyAppPathString;

#[cfg(test)]
#[path = "patches_tests.rs"]
mod tests;

#[derive(Debug)]
pub(crate) struct PatchHistoryCell {
    changes: HashMap<PathBuf, FileChange>,
    cwd: PathBuf,
}

impl HistoryCell for PatchHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        create_diff_summary(&self.changes, &self.cwd, width as usize)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(create_diff_summary(
            &self.changes,
            &self.cwd,
            RAW_DIFF_SUMMARY_WIDTH,
        ))
    }
}
/// Create a new `PendingPatch` cell that lists the file‑level summary of
/// a proposed patch. The summary lines should already be formatted (e.g.
/// "A path/to/file.rs").
pub(crate) fn new_patch_event(
    changes: HashMap<PathBuf, FileChange>,
    cwd: &Path,
) -> PatchHistoryCell {
    PatchHistoryCell {
        changes,
        cwd: cwd.to_path_buf(),
    }
}

pub(crate) fn new_patch_apply_failure(stderr: String) -> PlainHistoryCell {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Failure title
    lines.push(Line::from("✘ Failed to apply patch".magenta().bold()));

    if !stderr.trim().is_empty() {
        let output = output_lines(
            Some(&CommandOutput::new(/*exit_code*/ 1, stderr)),
            OutputLinesParams {
                line_limit: TOOL_CALL_MAX_LINES,
                only_err: true,
                include_angle_pipe: true,
                include_prefix: true,
            },
        );
        lines.extend(output.lines);
    }

    PlainHistoryCell { lines }
}

#[derive(Debug)]
pub(crate) struct ViewImageHistoryCell {
    filename: String,
    path_label: String,
}

impl HistoryCell for ViewImageHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let line = vec![
            "• ".dim(),
            "Viewed image ".bold(),
            self.filename.replace(['\n', '\r', '\t'], " ").dim(),
        ]
        .into();
        vec![truncate_line_with_ellipsis_if_overflow(
            line,
            width as usize,
        )]
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        PrefixedWrappedHistoryCell::new(
            Line::from(vec!["Viewed image ".bold(), self.path_label.clone().dim()]),
            vec!["• ".dim()],
            "  ",
        )
        .display_lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![Line::from(format!("Viewed image {}", self.path_label))]
    }
}

pub(crate) fn new_view_image_tool_call(path: LegacyAppPathString) -> ViewImageHistoryCell {
    let filename = path
        .to_inferred_path_uri()
        .and_then(|path| path.basename())
        .unwrap_or_else(|| path.render_for_ui());
    ViewImageHistoryCell {
        filename,
        path_label: path.into_string(),
    }
}

#[derive(Debug)]
pub(crate) struct ImageGenerationHistoryCell {
    lines: Vec<HyperlinkLine>,
}

impl HistoryCell for ImageGenerationHistoryCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        visible_lines(self.lines.clone())
    }

    fn display_hyperlink_lines(&self, _width: u16) -> Vec<HyperlinkLine> {
        self.lines.clone()
    }

    fn transcript_hyperlink_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        self.display_hyperlink_lines(width)
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        plain_lines(visible_lines(self.lines.clone()))
    }
}

pub(crate) fn new_image_generation_call(
    call_id: String,
    status: &str,
    revised_prompt: Option<String>,
    saved_path: Option<AbsolutePathBuf>,
) -> ImageGenerationHistoryCell {
    let detail = revised_prompt.unwrap_or(call_id);
    let heading = if status == "failed" {
        vec!["✗ ".red().bold(), "Image generation failed".bold()].into()
    } else {
        vec!["• ".dim(), "Generated Image:".bold()].into()
    };
    let mut lines = vec![
        HyperlinkLine::new(heading),
        HyperlinkLine::new(vec!["  └ ".dim(), detail.dim()].into()),
    ];
    if let Some(saved_path) = saved_path {
        let saved_path_url = Url::from_file_path(saved_path.as_path())
            .map(|url| url.to_string())
            .unwrap_or_else(|_| saved_path.display().to_string());
        let parent_url = saved_path
            .parent()
            .and_then(|parent| Url::from_directory_path(parent).ok());
        let mut saved_to = HyperlinkLine::new(Line::from(vec!["  └ ".dim(), "Saved to: ".dim()]));
        if let Some(parent_url) = parent_url.as_ref() {
            saved_to.push_trusted_file_span(saved_path_url.cyan().underlined(), parent_url);
        } else {
            saved_to.line.push_span(saved_path_url);
        }
        lines.push(saved_to);

        if let Some(parent_url) = parent_url {
            lines.push(open_folder_link_line(
                &parent_url,
                terminal_name_for_link_rendering(),
            ));
        }
    }

    ImageGenerationHistoryCell { lines }
}

pub(crate) fn open_folder_link_line(
    parent_url: &Url,
    terminal_name: TerminalName,
) -> HyperlinkLine {
    let mut line = HyperlinkLine::new(Line::from("  └ ".dim()));
    line.push_trusted_file_span("Open containing folder".cyan().underlined(), parent_url);
    if terminal_name == TerminalName::AppleTerminal {
        line.line.push_span(" (⌘-click: ".dim());
        line.push_trusted_file_span(parent_url.to_string().cyan().underlined(), parent_url);
        line.line.push_span(")".dim());
    }
    line
}
