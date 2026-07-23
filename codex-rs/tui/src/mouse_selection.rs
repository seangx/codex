use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthStr;

use crate::terminal_hyperlinks::strip_osc8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SelectionRange {
    anchor: Position,
    focus: Position,
}

impl SelectionRange {
    pub(crate) fn new(anchor: Position, focus: Position) -> Self {
        Self { anchor, focus }
    }

    fn ordered(self) -> (Position, Position) {
        if (self.anchor.y, self.anchor.x) <= (self.focus.y, self.focus.x) {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    pub(crate) fn highlight(self, buffer: &mut Buffer) {
        let Some(selection) = self.clamped_to(buffer.area) else {
            return;
        };
        let (start, end) = selection.ordered();

        for y in start.y..=end.y {
            let row_start = if y == start.y { start.x } else { buffer.area.x };
            let row_end = if y == end.y {
                end.x
            } else {
                buffer.area.right().saturating_sub(1)
            };
            for x in row_start..=row_end {
                buffer[(x, y)].modifier.insert(Modifier::REVERSED);
            }
        }
    }

    fn text(self, buffer: &Buffer) -> Option<String> {
        let selection = self.clamped_to(buffer.area)?;
        let (start, end) = selection.ordered();
        let mut rows = Vec::with_capacity(usize::from(end.y - start.y) + 1);

        for y in start.y..=end.y {
            let row_start = if y == start.y { start.x } else { buffer.area.x };
            let row_end = if y == end.y {
                end.x
            } else {
                buffer.area.right().saturating_sub(1)
            };
            let mut row = String::new();
            let mut x = row_start;
            while x <= row_end {
                let symbol = strip_osc8(buffer[(x, y)].symbol());
                let width = UnicodeWidthStr::width(symbol.as_str()).max(1);
                row.push_str(&symbol);
                x = x.saturating_add(u16::try_from(width).unwrap_or(u16::MAX));
            }
            rows.push(row.trim_end().to_string());
        }

        let text = rows.join("\n");
        (!text.is_empty()).then_some(text)
    }

    fn clamped_to(self, area: Rect) -> Option<Self> {
        if area.is_empty() {
            return None;
        }
        Some(Self {
            anchor: clamp_position(self.anchor, area),
            focus: clamp_position(self.focus, area),
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum MouseSelectionAction {
    None,
    Click(MouseEvent),
    Copy(String),
}

#[derive(Debug, Default)]
pub(crate) struct MouseSelectionState {
    pressed_at: Option<Position>,
    selection: Option<SelectionRange>,
}

impl MouseSelectionState {
    pub(crate) fn selection(&self) -> Option<SelectionRange> {
        self.selection
    }

    pub(crate) fn clear(&mut self) -> bool {
        self.pressed_at.take().is_some() || self.selection.take().is_some()
    }

    pub(crate) fn handle_event(
        &mut self,
        event: MouseEvent,
        displayed_buffer: &Buffer,
    ) -> MouseSelectionAction {
        let position = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pressed_at = Some(position);
                self.selection = Some(SelectionRange::new(position, position));
                MouseSelectionAction::None
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(anchor) = self.pressed_at {
                    self.selection = Some(SelectionRange::new(anchor, position));
                }
                MouseSelectionAction::None
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(anchor) = self.pressed_at.take() else {
                    return MouseSelectionAction::None;
                };
                if anchor == position {
                    self.selection = None;
                    return MouseSelectionAction::Click(event);
                }

                let selection = SelectionRange::new(anchor, position);
                self.selection = Some(selection);
                selection
                    .text(displayed_buffer)
                    .map_or(MouseSelectionAction::None, MouseSelectionAction::Copy)
            }
            _ => MouseSelectionAction::None,
        }
    }
}

fn clamp_position(position: Position, area: Rect) -> Position {
    Position::new(
        position.x.clamp(area.x, area.right().saturating_sub(1)),
        position.y.clamp(area.y, area.bottom().saturating_sub(1)),
    )
}
