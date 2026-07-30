use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use ratatui::buffer::Buffer;
use ratatui::buffer::CellWidth;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use std::time::Duration;
use std::time::Instant;

use crate::terminal_hyperlinks::strip_osc8;

const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const WORD_CONNECTORS: &str = "_-./:@~?&=%+#";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SelectionRange {
    anchor: Position,
    focus: Position,
}

impl SelectionRange {
    pub(crate) fn new(anchor: Position, focus: Position) -> Self {
        Self { anchor, focus }
    }

    pub(crate) fn ordered(self) -> (Position, Position) {
        if (self.anchor.y, self.anchor.x) <= (self.focus.y, self.focus.x) {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    pub(crate) fn contains(self, position: Position) -> bool {
        let (start, end) = self.ordered();
        (start.y, start.x) <= (position.y, position.x) && (position.y, position.x) <= (end.y, end.x)
    }

    pub(crate) fn highlight(self, buffer: &mut Buffer) {
        let Some(selection) = self.intersection_with(buffer.area) else {
            return;
        };
        let (start, end) = selection.ordered();

        for y in start.y..=end.y {
            let row_start = if y == start.y { start.x } else { buffer.area.x };
            let selected_row_end = if y == end.y {
                end.x
            } else {
                buffer.area.right().saturating_sub(1)
            };
            let Some(content_end) = row_content_end(buffer, y) else {
                continue;
            };
            let row_end = selected_row_end.min(content_end);
            if row_start > row_end {
                continue;
            }
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
                let cell = &buffer[(x, y)];
                let symbol = strip_osc8(cell.symbol());
                let width = usize::from(cell.cell_width()).max(1);
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

    fn intersection_with(self, area: Rect) -> Option<Self> {
        if area.is_empty() {
            return None;
        }
        let (start, end) = self.ordered();
        let area_start = area.as_position();
        let area_end = Position::new(
            area.right().saturating_sub(1),
            area.bottom().saturating_sub(1),
        );
        let start = position_max(start, area_start);
        let end = position_min(end, area_end);
        ((start.y, start.x) <= (end.y, end.x)).then(|| Self::new(start, end))
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
    word_anchor: Option<SelectionRange>,
    last_click: Option<CompletedClick>,
}

#[derive(Debug)]
struct CompletedClick {
    position: Position,
    completed_at: Instant,
}

impl MouseSelectionState {
    pub(crate) fn selection(&self) -> Option<SelectionRange> {
        self.selection
    }

    pub(crate) fn clear(&mut self) -> bool {
        self.word_anchor = None;
        self.last_click = None;
        let had_pressed_position = self.pressed_at.take().is_some();
        let had_selection = self.selection.take().is_some();
        had_pressed_position || had_selection
    }

    pub(crate) fn clear_click_sequence(&mut self) {
        self.last_click = None;
    }

    pub(crate) fn handle_event(
        &mut self,
        event: MouseEvent,
        displayed_buffer: &Buffer,
        occurred_at: Instant,
    ) -> MouseSelectionAction {
        let position = symbol_start_at(displayed_buffer, Position::new(event.column, event.row));
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pressed_at = Some(position);
                let is_double_click = self.last_click.take().is_some_and(|click| {
                    click.position == position
                        && occurred_at.saturating_duration_since(click.completed_at)
                            <= DOUBLE_CLICK_INTERVAL
                });
                self.word_anchor = is_double_click
                    .then(|| word_range_at(displayed_buffer, position))
                    .flatten();
                self.selection = self
                    .word_anchor
                    .or_else(|| Some(SelectionRange::new(position, position)));
                MouseSelectionAction::None
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(anchor) = self.pressed_at {
                    self.selection = Some(self.word_anchor.map_or_else(
                        || SelectionRange::new(anchor, position),
                        |word| extend_word_selection(word, position),
                    ));
                }
                MouseSelectionAction::None
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(anchor) = self.pressed_at.take() else {
                    return MouseSelectionAction::None;
                };
                if self.word_anchor.take().is_some() {
                    let action = self
                        .selection
                        .and_then(|selection| selection.text(displayed_buffer))
                        .map_or(MouseSelectionAction::None, MouseSelectionAction::Copy);
                    self.selection = None;
                    self.last_click = None;
                    return action;
                }
                if anchor == position {
                    self.selection = None;
                    self.last_click = Some(CompletedClick {
                        position,
                        completed_at: occurred_at,
                    });
                    return MouseSelectionAction::Click(event);
                }

                let selection = SelectionRange::new(anchor, position);
                let action = selection
                    .text(displayed_buffer)
                    .map_or(MouseSelectionAction::None, MouseSelectionAction::Copy);
                self.selection = None;
                self.last_click = None;
                action
            }
            _ => MouseSelectionAction::None,
        }
    }
}

fn word_range_at(buffer: &Buffer, position: Position) -> Option<SelectionRange> {
    if !buffer.area.contains(position) {
        return None;
    }

    let symbols = row_symbols(buffer, position.y);
    let selected = symbols
        .iter()
        .position(|symbol| symbol.start <= position.x && position.x <= symbol.end)?;
    if !symbols[selected].is_word {
        return None;
    }

    let mut first = selected;
    while first > 0 && symbols[first - 1].is_word {
        first -= 1;
    }
    let mut last = selected;
    while last + 1 < symbols.len() && symbols[last + 1].is_word {
        last += 1;
    }

    Some(SelectionRange::new(
        Position::new(symbols[first].start, position.y),
        Position::new(symbols[last].end, position.y),
    ))
}

fn extend_word_selection(word: SelectionRange, position: Position) -> SelectionRange {
    let (start, end) = word.ordered();
    if (position.y, position.x) < (start.y, start.x) {
        SelectionRange::new(end, position)
    } else if (position.y, position.x) > (end.y, end.x) {
        SelectionRange::new(start, position)
    } else {
        word
    }
}

#[derive(Debug)]
struct RowSymbol {
    start: u16,
    end: u16,
    is_word: bool,
}

fn row_symbols(buffer: &Buffer, y: u16) -> Vec<RowSymbol> {
    let mut symbols = Vec::new();
    let mut x = buffer.area.x;
    while x < buffer.area.right() {
        let cell = &buffer[(x, y)];
        let symbol = strip_osc8(cell.symbol());
        let width = cell.cell_width().max(1);
        let end = x
            .saturating_add(width)
            .saturating_sub(1)
            .min(buffer.area.right().saturating_sub(1));
        symbols.push(RowSymbol {
            start: x,
            end,
            is_word: symbol
                .chars()
                .any(|ch| ch.is_alphanumeric() || WORD_CONNECTORS.contains(ch)),
        });
        x = x.saturating_add(width);
    }
    symbols
}

fn clamp_position(position: Position, area: Rect) -> Position {
    Position::new(
        position.x.clamp(area.x, area.right().saturating_sub(1)),
        position.y.clamp(area.y, area.bottom().saturating_sub(1)),
    )
}

fn symbol_start_at(buffer: &Buffer, position: Position) -> Position {
    if !buffer.area.contains(position) {
        return position;
    }
    let mut x = buffer.area.x;
    while x < buffer.area.right() {
        let cell = &buffer[(x, position.y)];
        let width = cell.cell_width().max(1);
        if position.x < x.saturating_add(width) {
            return Position::new(x, position.y);
        }
        x = x.saturating_add(width);
    }
    position
}

fn row_content_end(buffer: &Buffer, y: u16) -> Option<u16> {
    let mut content_end = None;
    let mut x = buffer.area.x;
    while x < buffer.area.right() {
        let cell = &buffer[(x, y)];
        let symbol = strip_osc8(cell.symbol());
        let width = cell.cell_width().max(1);
        if symbol.chars().any(|ch| !ch.is_whitespace()) {
            content_end = Some(
                x.saturating_add(width)
                    .saturating_sub(1)
                    .min(buffer.area.right().saturating_sub(1)),
            );
        }
        x = x.saturating_add(width);
    }
    content_end
}

fn position_max(left: Position, right: Position) -> Position {
    if (left.y, left.x) >= (right.y, right.x) {
        left
    } else {
        right
    }
}

fn position_min(left: Position, right: Position) -> Position {
    if (left.y, left.x) <= (right.y, right.x) {
        left
    } else {
        right
    }
}
