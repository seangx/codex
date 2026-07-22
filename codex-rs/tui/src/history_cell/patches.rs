//! Patch summaries and image-tool transcript helpers.

use super::*;
use crate::terminal_hyperlinks::terminal_name_for_link_rendering;
use codex_terminal_detection::TerminalName;
use codex_utils_path_uri::LegacyAppPathString;

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

pub(crate) fn new_view_image_tool_call(path: LegacyAppPathString, cwd: &Path) -> PlainHistoryCell {
    let display_path = path
        .to_inferred_path_uri()
        .and_then(|path| path.to_abs_path().ok())
        .map(|path| display_path_for(path.as_path(), cwd))
        .unwrap_or_else(|| path.into_string());

    let lines: Vec<Line<'static>> = vec![
        vec!["• ".dim(), "Viewed Image".bold()].into(),
        vec!["  └ ".dim(), display_path.dim()].into(),
    ];

    PlainHistoryCell { lines }
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
        lines.push(HyperlinkLine::new(
            vec!["  └ ".dim(), "Saved to: ".dim(), saved_path_url.into()].into(),
        ));

        if let Some(parent) = saved_path.parent()
            && let Ok(parent_url) = Url::from_directory_path(parent)
        {
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
