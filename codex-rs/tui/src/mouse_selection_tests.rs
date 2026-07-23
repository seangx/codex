use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

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

#[test]
fn drag_selects_multiline_text_in_both_directions() {
    let buffer = buffer_with_lines(&["alpha", "bravo", "charl"]);

    for (start, end) in [((2, 0), (2, 2)), ((2, 2), (2, 0))] {
        let mut state = MouseSelectionState::default();
        assert_eq!(
            state.handle_event(
                mouse(MouseEventKind::Down(MouseButton::Left), start.0, start.1),
                &buffer,
            ),
            MouseSelectionAction::None
        );
        assert_eq!(
            state.handle_event(
                mouse(MouseEventKind::Drag(MouseButton::Left), end.0, end.1),
                &buffer,
            ),
            MouseSelectionAction::None
        );
        assert_eq!(
            state.handle_event(
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
    buffer[(0, 0)].set_symbol("\x1b]8;;https://example.com\x07x\x1b]8;;\x07");
    buffer[(1, 0)].set_symbol("y");
    let mut state = MouseSelectionState::default();

    state.handle_event(
        mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
        &buffer,
    );
    assert_eq!(
        state.handle_event(mouse(MouseEventKind::Up(MouseButton::Left), 1, 0), &buffer,),
        MouseSelectionAction::Copy("xy".to_string())
    );
}

#[test]
fn copied_text_skips_wide_character_continuation_cells() {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
    buffer.set_string(0, 0, "猫咪", ratatui::style::Style::default());
    let mut state = MouseSelectionState::default();

    state.handle_event(
        mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
        &buffer,
    );
    assert_eq!(
        state.handle_event(mouse(MouseEventKind::Up(MouseButton::Left), 3, 0), &buffer,),
        MouseSelectionAction::Copy("猫咪".to_string())
    );
}

#[test]
fn click_is_distinct_from_drag_selection() {
    let buffer = buffer_with_lines(&["link"]);
    let mut state = MouseSelectionState::default();
    let down = mouse(MouseEventKind::Down(MouseButton::Left), 1, 0);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 1, 0);

    assert_eq!(
        state.handle_event(down, &buffer),
        MouseSelectionAction::None
    );
    assert_eq!(
        state.handle_event(up, &buffer),
        MouseSelectionAction::Click(up)
    );
    assert_eq!(state.selection(), None);
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
