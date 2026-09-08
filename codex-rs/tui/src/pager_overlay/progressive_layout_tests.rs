use super::LAYOUT_BATCH_CELLS;
use crate::history_cell::HistoryCell;
use crate::keymap::RuntimeKeymap;
use crate::pager_overlay::TranscriptOverlay;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

#[derive(Debug)]
struct CountingCell {
    label: String,
    measurements: Arc<AtomicUsize>,
}

impl HistoryCell for CountingCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        textwrap::wrap(&self.label, usize::from(width).max(1))
            .into_iter()
            .map(|line| Line::from(line.into_owned()))
            .collect()
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        vec![self.label.clone().into()]
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.measurements.fetch_add(1, Ordering::Relaxed);
        self.display_lines(width).len() as u16
    }
}

fn cells(count: usize, measurements: &Arc<AtomicUsize>) -> Vec<Arc<dyn HistoryCell>> {
    (0..count)
        .map(|index| {
            Arc::new(CountingCell {
                label: format!("cell-{index:04}"),
                measurements: Arc::clone(measurements),
            }) as Arc<dyn HistoryCell>
        })
        .collect()
}

#[test]
fn first_wheel_layout_measures_only_recent_cells_regardless_of_history_length() {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 5,
    );
    for count in [1_000, 100_000] {
        let measured = Arc::new(AtomicUsize::new(0));
        let mut overlay = TranscriptOverlay::new_embedded(
            cells(count, &measured),
            RuntimeKeymap::defaults().pager,
        );
        overlay.scroll_up(area, /*rows*/ 2);
        let mut buffer = Buffer::empty(area);
        overlay.render_embedded(area, &mut buffer);
        assert_eq!(measured.load(Ordering::Relaxed), LAYOUT_BATCH_CELLS);
        overlay.scroll_up(area, /*rows*/ 2);
        overlay.render_embedded(area, &mut buffer);
        assert_eq!(measured.load(Ordering::Relaxed), LAYOUT_BATCH_CELLS);
    }
}

#[test]
fn revealing_older_cells_preserves_screen_and_existing_height_caches() {
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 5,
    );
    let measured = Arc::new(AtomicUsize::new(0));
    let mut overlay =
        TranscriptOverlay::new_embedded(cells(1_000, &measured), RuntimeKeymap::defaults().pager);
    overlay.scroll_up(area, /*rows*/ 2);
    let mut before = Buffer::empty(area);
    overlay.render_embedded(area, &mut before);
    // Prefetch more than the current suffix without moving the viewport.
    overlay.prepare_upward_scroll(area, /*rows*/ 150);
    let mut after = Buffer::empty(area);
    overlay.render_embedded(area, &mut after);
    assert_eq!(after, before);
    assert_eq!(measured.load(Ordering::Relaxed), 2 * LAYOUT_BATCH_CELLS);
    let screen = before
        .content
        .chunks(usize::from(area.width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(screen, @"
    cell-0996

    cell-0997

    cell-0998
    ");
}

#[test]
fn progressive_navigation_matches_eager_layout_through_entire_history_and_resize() {
    let measured = Arc::new(AtomicUsize::new(0));
    let source = cells(300, &measured);
    let mut lazy = TranscriptOverlay::new_embedded(source.clone(), RuntimeKeymap::defaults().pager);
    let mut eager = TranscriptOverlay::new_embedded(source, RuntimeKeymap::defaults().pager);
    eager.materialize_to(/*start*/ 0);
    for width in [12, 20] {
        let area = Rect::new(/*x*/ 0, /*y*/ 0, width, /*height*/ 5);
        for _ in 0..110 {
            lazy.scroll_up(area, /*rows*/ 7);
            eager.scroll_up(area, /*rows*/ 7);
            let mut actual = Buffer::empty(area);
            let mut expected = Buffer::empty(area);
            lazy.render_embedded(area, &mut actual);
            eager.render_embedded(area, &mut expected);
            assert_eq!(actual, expected);
        }
    }
    assert_eq!(lazy.materialized_start, 0);
}

#[test]
fn committed_and_consolidated_cells_keep_deferred_source_indices_valid() {
    let measured = Arc::new(AtomicUsize::new(0));
    let mut overlay =
        TranscriptOverlay::new_embedded(cells(300, &measured), RuntimeKeymap::defaults().pager);
    let cell = cells(1, &measured).pop().unwrap();
    overlay.insert_cell(Arc::clone(&cell));
    overlay.consolidate_cells(0..10, Arc::clone(&cell));
    assert_eq!(overlay.materialized_start, 227);
    overlay.consolidate_cells(220..230, cell);
    assert_eq!(overlay.materialized_start, 220);
    overlay.set_highlight_cell(Some(10));
    assert_eq!(overlay.materialized_start, 10);
    assert_eq!(overlay.view.renderables.len(), overlay.cells.len());
}

#[derive(Debug)]
struct EmptyCountingCell {
    measurements: Arc<AtomicUsize>,
}

impl HistoryCell for EmptyCountingCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        Vec::new()
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        Vec::new()
    }

    fn desired_height(&self, _width: u16) -> u16 {
        self.measurements.fetch_add(1, Ordering::Relaxed);
        0
    }

    fn is_stream_continuation(&self) -> bool {
        true
    }
}

#[test]
fn zero_height_history_yields_after_one_batch_per_frame() {
    let measured = Arc::new(AtomicUsize::new(0));
    let source = (0..1_000)
        .map(|_| {
            Arc::new(EmptyCountingCell {
                measurements: Arc::clone(&measured),
            }) as Arc<dyn HistoryCell>
        })
        .collect();
    let mut overlay = TranscriptOverlay::new_embedded(source, RuntimeKeymap::defaults().pager);
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 5,
    );
    let mut buffer = Buffer::empty(area);

    for frame in 1..=3 {
        let previous_start = overlay.materialized_start;
        assert!(overlay.render_embedded(area, &mut buffer));
        assert_eq!(
            overlay.materialized_start,
            previous_start - LAYOUT_BATCH_CELLS
        );
        // The first frame measures the initial suffix as well as its one extra batch.
        assert_eq!(
            measured.load(Ordering::Relaxed),
            (frame + 1) * LAYOUT_BATCH_CELLS
        );
    }
}

#[test]
fn insertion_between_materialization_and_draw_does_not_follow_bottom() {
    let measured = Arc::new(AtomicUsize::new(0));
    let mut overlay =
        TranscriptOverlay::new_embedded(cells(1_000, &measured), RuntimeKeymap::defaults().pager);
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 5,
    );
    // Render close to the materialized prefix without the usual eager prefetch.
    overlay.view.scroll_offset = 2;
    let mut before = Buffer::empty(area);
    overlay.view.render_content_only(area, &mut before);

    overlay.prepare_upward_scroll(area, /*rows*/ 0);
    let anchored_offset = overlay.view.scroll_offset;
    assert_eq!(anchored_offset, 2 + 2 * LAYOUT_BATCH_CELLS);
    assert!(!overlay.is_scrolled_to_bottom());
    overlay.insert_cell(cells(1, &measured).pop().unwrap());
    assert_eq!(overlay.view.scroll_offset, anchored_offset);

    let mut after = Buffer::empty(area);
    overlay.render_embedded(area, &mut after);
    assert_eq!(after, before);
}

#[test]
fn resize_mid_history_preserves_visible_cell_before_deferred_prefix_is_measured() {
    let measured = Arc::new(AtomicUsize::new(0));
    let source = (0..300)
        .map(|index| {
            Arc::new(CountingCell {
                label: format!("cell-{index:04} alpha beta gamma delta epsilon"),
                measurements: Arc::clone(&measured),
            }) as Arc<dyn HistoryCell>
        })
        .collect();
    let mut overlay = TranscriptOverlay::new_embedded(source, RuntimeKeymap::defaults().pager);
    let wide = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 40, /*height*/ 5,
    );
    let narrow = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 12, /*height*/ 5,
    );
    let target = 280;
    let (old_top, old_bottom) = overlay
        .view
        .height_index
        .item_bounds(&overlay.view.renderables, wide.width, target)
        .unwrap();
    // Skip the inter-cell blank line and anchor the first actual text row.
    overlay.view.scroll_offset = old_top + 1;
    let mut before = Buffer::empty(wide);
    overlay.render_embedded(wide, &mut before);
    let deferred_start = overlay.materialized_start;
    assert!(deferred_start > 0);

    let mut after = Buffer::empty(narrow);
    overlay.render_embedded(narrow, &mut after);
    let (new_top, new_bottom) = overlay
        .view
        .height_index
        .item_bounds(&overlay.view.renderables, narrow.width, target)
        .unwrap();
    assert!(new_bottom - new_top > old_bottom - old_top);
    assert_eq!(overlay.view.scroll_offset, new_top + 1);
    assert_eq!(overlay.materialized_start, deferred_start);
    let first_row = after.content[..usize::from(narrow.width)]
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert_eq!(first_row.trim_end(), "cell-0280");
}
