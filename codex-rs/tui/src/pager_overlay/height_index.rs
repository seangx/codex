use std::cell::RefCell;

use crate::render::renderable::Renderable;

/// Cumulative renderable heights for the current pager width.
///
/// The index turns total-height and visible-start lookups into constant/logarithmic work after
/// the first measurement pass. Pager mutations explicitly invalidate it.
#[derive(Default)]
pub(super) struct HeightIndex {
    state: RefCell<HeightIndexState>,
}

#[derive(Default)]
struct HeightIndexState {
    width: Option<u16>,
    cumulative_heights: Vec<usize>,
}

impl HeightIndex {
    /// Preserve a source-cell anchor when a compact viewport changes width, without remeasuring
    /// the old layout. A bottom sentinel remains bottom-pinned instead.
    pub(super) fn resize_anchor(&self, width: u16, offset: usize) -> Option<(usize, usize)> {
        let state = self.state.borrow();
        if state.width == Some(width) || offset == usize::MAX {
            return None;
        }
        let total = *state.cumulative_heights.last()?;
        if offset >= total {
            return None;
        }
        let index = state
            .cumulative_heights
            .partition_point(|top| *top <= offset)
            - 1;
        Some((index, offset - state.cumulative_heights[index]))
    }

    pub(super) fn invalidate(&mut self) {
        *self.state.get_mut() = HeightIndexState::default();
    }

    pub(super) fn total_height(&self, renderables: &[Box<dyn Renderable>], width: u16) -> usize {
        self.ensure(renderables, width);
        self.state
            .borrow()
            .cumulative_heights
            .last()
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn item_bounds(
        &self,
        renderables: &[Box<dyn Renderable>],
        width: u16,
        index: usize,
    ) -> Option<(usize, usize)> {
        self.ensure(renderables, width);
        let state = self.state.borrow();
        Some((
            *state.cumulative_heights.get(index)?,
            *state.cumulative_heights.get(index.saturating_add(1))?,
        ))
    }

    pub(super) fn first_visible(
        &self,
        renderables: &[Box<dyn Renderable>],
        width: u16,
        scroll_offset: usize,
        area_y: u16,
    ) -> (usize, isize) {
        self.ensure(renderables, width);
        let state = self.state.borrow();
        let threshold = scroll_offset.saturating_add(area_y as usize);
        let start = state.cumulative_heights[1..].partition_point(|bottom| *bottom < threshold);
        let top = state
            .cumulative_heights
            .get(start)
            .copied()
            .unwrap_or_default() as isize
            - scroll_offset as isize;
        (start, top)
    }

    fn ensure(&self, renderables: &[Box<dyn Renderable>], width: u16) {
        let mut state = self.state.borrow_mut();
        if state.width == Some(width)
            && state.cumulative_heights.len() == renderables.len().saturating_add(1)
        {
            return;
        }

        state.width = Some(width);
        state.cumulative_heights.clear();
        state
            .cumulative_heights
            .reserve(renderables.len().saturating_add(1));
        state.cumulative_heights.push(0);
        for renderable in renderables {
            let next = state
                .cumulative_heights
                .last()
                .copied()
                .unwrap_or_default()
                .saturating_add(renderable.desired_height(width) as usize);
            state.cumulative_heights.push(next);
        }
    }
}
