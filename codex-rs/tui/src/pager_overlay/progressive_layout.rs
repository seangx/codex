//! Delay compact-history layout until the viewport approaches older cells.
//!
//! Keep source indices intact using zero-height placeholders. Opening the mouse-wheel viewer
//! measures a recent batch, not every message accumulated during the session. Revealing an older
//! batch preserves existing renderables (and their height caches) and anchors the same visible row.
//! Full transcript/export behavior is unchanged; explicit navigation to the beginning may still
//! require the complete layout. This bounds cells measured initially, not the size of one cell.

use super::TranscriptOverlay;
use crate::render::renderable::Renderable;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

pub(super) const LAYOUT_BATCH_CELLS: usize = 64;

pub(super) struct DeferredCell;

impl Renderable for DeferredCell {
    fn render(&self, _area: Rect, _buf: &mut Buffer) {}

    fn desired_height(&self, _width: u16) -> u16 {
        0
    }
}

impl TranscriptOverlay {
    pub(super) fn materialize_to(&mut self, start: usize) {
        if start >= self.materialized_start {
            return;
        }
        for index in start..self.materialized_start {
            self.view.renderables[index] = Self::render_cell(
                &self.cells[index],
                index,
                self.highlight_cell,
                self.content,
                self.history_state,
            );
        }
        self.materialized_start = start;
        self.view.invalidate_height_index();
    }

    pub(super) fn prepare_upward_scroll(&mut self, area: Rect, rows: usize) -> bool {
        if area.is_empty() {
            return false;
        }
        let anchor = self
            .view
            .height_index
            .resize_anchor(area.width, self.view.scroll_offset);
        let mut height = self.view.content_height(area.width);
        if let Some((index, offset)) = anchor
            && let Some((top, bottom)) =
                self.view
                    .height_index
                    .item_bounds(&self.view.renderables, area.width, index)
        {
            self.view.scroll_offset = top + offset.min(bottom.saturating_sub(top + 1));
        }
        // Resolve the initial bottom sentinel before adding any prefix height.
        self.view.scroll_offset = self
            .view
            .scroll_offset
            .min(height.saturating_sub(usize::from(area.height)));
        let needed = rows.saturating_add(usize::from(area.height));
        // At most one batch per event/frame: long runs of invisible cells must not monopolize
        // the UI thread. The caller requests another frame if more layout is still needed.
        if self.materialized_start > 0 && self.view.scroll_offset < needed {
            let start = self.materialized_start.saturating_sub(LAYOUT_BATCH_CELLS);
            self.materialize_to(start);
            let next_height = self.view.content_height(area.width);
            self.view.scroll_offset = self
                .view
                .scroll_offset
                .saturating_add(next_height.saturating_sub(height));
            height = next_height;
        }
        // Background insertions can arrive before the next draw. They must compare the anchored
        // offset against the new height, not mistake it for the bottom of the previous suffix.
        self.view.last_rendered_height = Some(height);
        self.view.last_content_height = Some(usize::from(area.height));
        self.materialized_start > 0 && self.view.scroll_offset < needed
    }
}

#[cfg(test)]
#[path = "progressive_layout_tests.rs"]
mod tests;
