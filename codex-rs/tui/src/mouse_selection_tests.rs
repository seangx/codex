use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use ratatui::buffer::Buffer;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use std::num::NonZeroU16;
use std::time::Duration;
use std::time::Instant;

use crate::mouse_selection::MouseSelectionAction;
use crate::mouse_selection::MouseSelectionState;
use crate::mouse_selection::SelectionRange;

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn buffer_with_lines(lines: &[&str]) -> Buffer {
    let width = lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0) as u16;
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, lines.len() as u16));
    for (y, line) in lines.iter().enumerate() {
        for (x, ch) in line.chars().enumerate() {
            buffer[(x as u16, y as u16)].set_char(ch);
        }
    }
    buffer
}

fn handle(
    state: &mut MouseSelectionState,
    event: MouseEvent,
    buffer: &Buffer,
) -> MouseSelectionAction {
    state.handle_event(event, buffer, Instant::now())
}

fn handle_at(
    state: &mut MouseSelectionState,
    event: MouseEvent,
    buffer: &Buffer,
    occurred_at: Instant,
) -> MouseSelectionAction {
    state.handle_event(event, buffer, occurred_at)
}

#[test]
fn drag_selects_multiline_text_in_both_directions() {
    let buffer = buffer_with_lines(&["alpha", "bravo", "charl"]);

    for (start, end) in [((2, 0), (2, 2)), ((2, 2), (2, 0))] {
        let mut state = MouseSelectionState::default();
        assert_eq!(
            handle(
                &mut state,
                mouse(MouseEventKind::Down(MouseButton::Left), start.0, start.1),
                &buffer,
            ),
            MouseSelectionAction::None
        );
        assert_eq!(
            handle(
                &mut state,
                mouse(MouseEventKind::Drag(MouseButton::Left), end.0, end.1),
                &buffer,
            ),
            MouseSelectionAction::None
        );
        assert_eq!(
            handle(
                &mut state,
                mouse(MouseEventKind::Up(MouseButton::Left), end.0, end.1),
                &buffer,
            ),
            MouseSelectionAction::Copy("pha\nbravo\ncha".to_string())
        );
    }
}

#[test]
fn copied_text_strips_osc8_annotations() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
    buffer[(0, 0)]
        .set_symbol("\x1b]8;;https://example.com\x07x\x1b]8;;\x07")
        .set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::MIN));
    buffer[(1, 0)].set_symbol("y");
    let mut state = MouseSelectionState::default();

    handle(
        &mut state,
        mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
        &buffer,
    );
    assert_eq!(
        handle(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 1, 0),
            &buffer
        ),
        MouseSelectionAction::Copy("xy".to_string())
    );
}

#[test]
fn copied_text_honors_ratatui_forced_cell_width() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
    buffer[(0, 0)]
        .set_symbol("ｶﾞ")
        .set_diff_option(CellDiffOption::ForcedWidth(
            NonZeroU16::new(2).unwrap_or(NonZeroU16::MIN),
        ));
    buffer[(2, 0)].set_symbol("x");
    let mut state = MouseSelectionState::default();

    handle(
        &mut state,
        mouse(MouseEventKind::Down(MouseButton::Left), 1, 0),
        &buffer,
    );
    assert_eq!(
        handle(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 2, 0),
            &buffer
        ),
        MouseSelectionAction::Copy("ｶﾞx".to_string())
    );
}

#[test]
fn copied_text_skips_wide_character_continuation_cells() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
    buffer.set_string(0, 0, "猫咪", ratatui::style::Style::default());
    let mut state = MouseSelectionState::default();

    handle(
        &mut state,
        mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
        &buffer,
    );
    assert_eq!(
        handle(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 3, 0),
            &buffer
        ),
        MouseSelectionAction::Copy("猫咪".to_string())
    );
}

#[test]
fn selection_endpoints_on_wide_character_continuations_snap_to_the_character() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
    buffer.set_string(0, 0, "猫咪", ratatui::style::Style::default());
    let mut state = MouseSelectionState::default();

    handle(
        &mut state,
        mouse(MouseEventKind::Down(MouseButton::Left), 1, 0),
        &buffer,
    );
    assert_eq!(
        handle(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 3, 0),
            &buffer
        ),
        MouseSelectionAction::Copy("猫咪".to_string())
    );
}

#[test]
fn mouse_up_copies_text_and_clears_the_selection_highlight() {
    let buffer = buffer_with_lines(&["select me"]);
    let mut state = MouseSelectionState::default();

    handle(
        &mut state,
        mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
        &buffer,
    );
    handle(
        &mut state,
        mouse(MouseEventKind::Drag(MouseButton::Left), 5, 0),
        &buffer,
    );
    assert!(state.selection().is_some());
    assert_eq!(
        handle(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 5, 0),
            &buffer
        ),
        MouseSelectionAction::Copy("select".to_string())
    );
    assert_eq!(state.selection(), None);
}

#[test]
fn clear_removes_an_active_mouse_selection() {
    let buffer = buffer_with_lines(&["select me"]);
    let mut state = MouseSelectionState::default();

    handle(
        &mut state,
        mouse(MouseEventKind::Down(MouseButton::Left), 2, 0),
        &buffer,
    );

    assert!(state.clear());
    assert_eq!(state.selection(), None);
}

#[test]
fn click_is_distinct_from_drag_selection() {
    let buffer = buffer_with_lines(&["link"]);
    let mut state = MouseSelectionState::default();
    let down = mouse(MouseEventKind::Down(MouseButton::Left), 1, 0);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 1, 0);

    assert_eq!(
        handle(&mut state, down, &buffer),
        MouseSelectionAction::None
    );
    assert_eq!(
        handle(&mut state, up, &buffer),
        MouseSelectionAction::Click(up)
    );
    assert_eq!(state.selection(), None);
}

#[test]
fn double_click_selects_and_copies_the_contiguous_word() {
    let buffer = buffer_with_lines(&["run codex-rs/tui now"]);
    let mut state = MouseSelectionState::default();
    let started_at = Instant::now();

    assert_eq!(
        handle_at(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 8, 0),
            &buffer,
            started_at,
        ),
        MouseSelectionAction::None
    );
    assert_eq!(
        handle_at(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 8, 0),
            &buffer,
            started_at + Duration::from_millis(10),
        ),
        MouseSelectionAction::Click(mouse(MouseEventKind::Up(MouseButton::Left), 8, 0))
    );
    assert_eq!(
        handle_at(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 8, 0),
            &buffer,
            started_at + Duration::from_millis(100),
        ),
        MouseSelectionAction::None
    );
    assert_eq!(
        state.selection(),
        Some(SelectionRange::new(
            Position::new(4, 0),
            Position::new(15, 0)
        ))
    );
    assert_eq!(
        handle_at(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 8, 0),
            &buffer,
            started_at + Duration::from_millis(110),
        ),
        MouseSelectionAction::Copy("codex-rs/tui".to_string())
    );
    assert_eq!(state.selection(), None);
}

#[test]
fn double_click_selects_a_contiguous_chinese_phrase() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
    buffer.set_string(0, 0, "选择词组 好", ratatui::style::Style::default());
    let mut state = MouseSelectionState::default();
    let started_at = Instant::now();

    handle_at(
        &mut state,
        mouse(MouseEventKind::Down(MouseButton::Left), 4, 0),
        &buffer,
        started_at,
    );
    handle_at(
        &mut state,
        mouse(MouseEventKind::Up(MouseButton::Left), 4, 0),
        &buffer,
        started_at + Duration::from_millis(10),
    );
    handle_at(
        &mut state,
        mouse(MouseEventKind::Down(MouseButton::Left), 4, 0),
        &buffer,
        started_at + Duration::from_millis(100),
    );

    assert_eq!(
        handle_at(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 4, 0),
            &buffer,
            started_at + Duration::from_millis(110),
        ),
        MouseSelectionAction::Copy("选择词组".to_string())
    );
}

#[test]
fn clicks_outside_the_double_click_interval_remain_single_clicks() {
    let buffer = buffer_with_lines(&["alpha"]);
    let mut state = MouseSelectionState::default();
    let started_at = Instant::now();
    let down = mouse(MouseEventKind::Down(MouseButton::Left), 2, 0);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 2, 0);

    handle_at(&mut state, down, &buffer, started_at);
    assert_eq!(
        handle_at(
            &mut state,
            up,
            &buffer,
            started_at + Duration::from_millis(10),
        ),
        MouseSelectionAction::Click(up)
    );
    assert_eq!(
        handle_at(
            &mut state,
            down,
            &buffer,
            started_at + Duration::from_millis(600),
        ),
        MouseSelectionAction::None
    );
    assert_eq!(
        state.selection(),
        Some(SelectionRange::new(
            Position::new(2, 0),
            Position::new(2, 0)
        ))
    );
}

#[test]
fn clearing_click_sequence_prevents_link_clicks_from_starting_a_double_click() {
    let buffer = buffer_with_lines(&["https://openai.com"]);
    let mut state = MouseSelectionState::default();
    let started_at = Instant::now();
    let down = mouse(MouseEventKind::Down(MouseButton::Left), 8, 0);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 8, 0);

    handle_at(&mut state, down, &buffer, started_at);
    handle_at(
        &mut state,
        up,
        &buffer,
        started_at + Duration::from_millis(10),
    );
    state.clear_click_sequence();
    handle_at(
        &mut state,
        down,
        &buffer,
        started_at + Duration::from_millis(100),
    );

    assert_eq!(
        state.selection(),
        Some(SelectionRange::new(
            Position::new(8, 0),
            Position::new(8, 0)
        ))
    );
}

#[test]
fn double_click_word_highlight_snapshot() {
    let buffer = buffer_with_lines(&["run codex-rs/tui now"]);
    let mut state = MouseSelectionState::default();
    let started_at = Instant::now();
    let down = mouse(MouseEventKind::Down(MouseButton::Left), 8, 0);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 8, 0);

    handle_at(&mut state, down, &buffer, started_at);
    handle_at(
        &mut state,
        up,
        &buffer,
        started_at + Duration::from_millis(10),
    );
    handle_at(
        &mut state,
        down,
        &buffer,
        started_at + Duration::from_millis(100),
    );

    let mut highlighted = buffer.clone();
    state
        .selection()
        .expect("double-click selection")
        .highlight(&mut highlighted);
    let selected_cells = highlighted
        .content
        .iter()
        .enumerate()
        .filter(|(_, cell)| cell.modifier.contains(Modifier::REVERSED))
        .map(|(index, cell)| {
            let position = highlighted.pos_of(index);
            (position, cell.symbol().to_string())
        })
        .collect::<Vec<_>>();

    insta::assert_debug_snapshot!(selected_cells);
}

#[test]
fn selection_highlight_snapshot() {
    let mut buffer = buffer_with_lines(&["alpha", "bravo", "charl"]);
    SelectionRange::new(Position::new(2, 0), Position::new(2, 2)).highlight(&mut buffer);
    let selected_cells = buffer
        .content
        .iter()
        .enumerate()
        .filter(|(_, cell)| cell.modifier.contains(Modifier::REVERSED))
        .map(|(index, cell)| {
            let position = buffer.pos_of(index);
            (position, cell.symbol().to_string())
        })
        .collect::<Vec<_>>();

    insta::assert_debug_snapshot!(selected_cells);
}

#[test]
fn selection_above_inline_viewport_does_not_highlight_viewport_edge() {
    let mut buffer = Buffer::empty(Rect::new(0, 10, 8, 3));

    SelectionRange::new(Position::new(2, 2), Position::new(5, 2)).highlight(&mut buffer);

    assert!(
        buffer
            .content
            .iter()
            .all(|cell| !cell.modifier.contains(Modifier::REVERSED))
    );
}

#[test]
fn selection_highlight_excludes_trailing_blank_terminal_columns() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 2));
    buffer.set_string(0, 0, "short", ratatui::style::Style::default());
    buffer.set_string(0, 1, "line", ratatui::style::Style::default());

    SelectionRange::new(Position::new(0, 0), Position::new(19, 1)).highlight(&mut buffer);

    for x in 0..20 {
        assert_eq!(
            buffer[(x, 0)].modifier.contains(Modifier::REVERSED),
            x < 5,
            "unexpected row 0 highlight at x={x}"
        );
        assert_eq!(
            buffer[(x, 1)].modifier.contains(Modifier::REVERSED),
            x < 4,
            "unexpected row 1 highlight at x={x}"
        );
    }
}
