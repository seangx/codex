//! Viewport-aware transcript rendering and the fallback for generic pager content.

use std::sync::Arc;

use super::TranscriptContent;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::render::renderable::Renderable;
use crate::style::user_message_accent_style;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::mark_buffer_hyperlinks;
use crate::terminal_hyperlinks::visible_lines_ref;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;

/// Renders a committed history cell directly into the visible transcript viewport.
pub(super) struct CellRenderable {
    pub(super) cell: Arc<dyn HistoryCell>,
    pub(super) highlighted: bool,
    pub(super) content: TranscriptContent,
}

pub(super) fn user_history_overlay_style(highlighted: bool) -> Style {
    if highlighted {
        user_message_accent_style().reversed()
    } else {
        Style::default()
    }
}

impl Renderable for CellRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_scrolled(area, buf, /*scroll_offset*/ 0);
    }

    /// Scroll visible text and hyperlink metadata together without rendering hidden rows.
    fn render_scrolled(&self, area: Rect, buf: &mut Buffer, scroll_offset: u16) -> bool {
        let hyperlink_lines = match self.content {
            TranscriptContent::Full => self.cell.transcript_hyperlink_lines(area.width),
            TranscriptContent::Compact => self.cell.display_hyperlink_lines(area.width),
        };
        let style = if self.cell.as_any().is::<UserHistoryCell>() {
            user_history_overlay_style(self.highlighted)
        } else {
            Style::default()
        };
        let p = Paragraph::new(Text::from(visible_lines_ref(&hyperlink_lines)))
            .style(style)
            .wrap(Wrap { trim: false })
            .scroll((scroll_offset, 0));
        p.render(area, buf);
        mark_buffer_hyperlinks(buf, area, &hyperlink_lines, usize::from(scroll_offset));
        true
    }

    fn desired_height(&self, width: u16) -> u16 {
        match self.content {
            TranscriptContent::Full => self.cell.desired_transcript_height(width),
            TranscriptContent::Compact => self.cell.desired_height(width),
        }
    }
}

/// Renders the optional in-flight transcript tail without allocating hidden rows.
pub(super) struct HyperlinkLinesRenderable {
    pub(super) lines: Vec<HyperlinkLine>,
}

impl Renderable for HyperlinkLinesRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.render_scrolled(area, buf, /*scroll_offset*/ 0);
    }

    /// Keep live-tail hyperlinks aligned with the same visible rows as their text.
    fn render_scrolled(&self, area: Rect, buf: &mut Buffer, scroll_offset: u16) -> bool {
        Paragraph::new(Text::from(visible_lines_ref(&self.lines)))
            .wrap(Wrap { trim: false })
            .scroll((scroll_offset, 0))
            .render(area, buf);
        mark_buffer_hyperlinks(buf, area, &self.lines, usize::from(scroll_offset));
        true
    }

    fn desired_height(&self, width: u16) -> u16 {
        Paragraph::new(Text::from(visible_lines_ref(&self.lines)))
            .wrap(Wrap { trim: false })
            .line_count(width)
            .try_into()
            .unwrap_or(/*default*/ 0)
    }
}

/// Render visible rows directly when supported, preserving the legacy scratch-buffer fallback.
pub(super) fn render_offset_content(
    area: Rect,
    buf: &mut Buffer,
    renderable: &dyn Renderable,
    scroll_offset: u16,
) -> u16 {
    let height = renderable.desired_height(area.width);
    let copy_height = area.height.min(height.saturating_sub(scroll_offset));
    if copy_height == 0 {
        return 0;
    }

    let visible_area = Rect::new(area.x, area.y, area.width, copy_height);
    if renderable.render_scrolled(visible_area, buf, scroll_offset) {
        return copy_height;
    }

    let mut tall_buf = Buffer::empty(Rect::new(
        /*x*/ 0,
        /*y*/ 0,
        area.width,
        scroll_offset + copy_height,
    ));
    renderable.render(*tall_buf.area(), &mut tall_buf);
    for y in 0..copy_height {
        let src_y = y + scroll_offset;
        for x in 0..area.width {
            buf[(area.x + x, area.y + y)] = tall_buf[(x, src_y)].clone();
        }
    }

    copy_height
}

#[cfg(test)]
#[path = "scrolling_tests.rs"]
mod tests;
