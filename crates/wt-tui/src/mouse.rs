use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};

use crate::{
    Interaction, Model,
    render::{MousePane, mouse_pane, panes},
};

/// A left-button drag selection in screen cells. It is clamped to the
/// inside of the pane where it started, so borders and neighbouring panes
/// are never copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    anchor: (u16, u16),
    head: (u16, u16),
    bounds: Rect,
    dragged: bool,
    /// The button was released; the next frame copies and clears it.
    pub finished: bool,
}

impl Selection {
    fn clamp(&self, column: u16, row: u16) -> (u16, u16) {
        let right = self.bounds.x + self.bounds.width.saturating_sub(1);
        let bottom = self.bounds.y + self.bounds.height.saturating_sub(1);
        (
            column.clamp(self.bounds.x, right),
            row.clamp(self.bounds.y, bottom),
        )
    }

    /// Selected cells per screen row, in reading order.
    fn spans(&self) -> Vec<(u16, u16, u16)> {
        let (start, end) = {
            let a = (self.anchor.1, self.anchor.0);
            let b = (self.head.1, self.head.0);
            if a <= b { (a, b) } else { (b, a) }
        };
        let left = self.bounds.x;
        let right = self.bounds.x + self.bounds.width.saturating_sub(1);
        (start.0..=end.0)
            .map(|row| {
                let from = if row == start.0 { start.1 } else { left };
                let to = if row == end.0 { end.1 } else { right };
                (row, from, to)
            })
            .collect()
    }
}

/// Track a drag selection. Returns whether the screen must redraw.
pub(crate) fn select(model: &mut Model, event: MouseEvent, area: Rect) -> bool {
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let bounds = selection_bounds(model, area, event.column, event.row);
            let redraw = model.mouse_selection.is_some();
            model.mouse_selection = bounds.map(|bounds| Selection {
                anchor: (event.column, event.row),
                head: (event.column, event.row),
                bounds,
                dragged: false,
                finished: false,
            });
            if let Some(selection) = &mut model.mouse_selection {
                selection.anchor = selection.clamp(event.column, event.row);
                selection.head = selection.anchor;
            }
            redraw
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(selection) = &mut model.mouse_selection else {
                return false;
            };
            selection.head = selection.clamp(event.column, event.row);
            selection.dragged = true;
            true
        }
        MouseEventKind::Up(MouseButton::Left) => match &mut model.mouse_selection {
            Some(selection) if selection.dragged => {
                selection.head = selection.clamp(event.column, event.row);
                selection.finished = true;
                true
            }
            Some(_) => {
                model.mouse_selection = None;
                false
            }
            None => false,
        },
        _ => false,
    }
}

/// The inside of the pane under the pointer; overlays select from the
/// whole screen.
fn selection_bounds(model: &Model, area: Rect, column: u16, row: u16) -> Option<Rect> {
    let overlay = model.help
        || model.show_perf
        || model.yank.is_some()
        || !matches!(model.interaction, Interaction::None);
    if overlay {
        return Some(area);
    }
    let panes = panes(area, model.board.full_width_activity);
    let pane = match mouse_pane(area, model.board.full_width_activity, column, row) {
        MousePane::List => panes.list,
        MousePane::Details => panes.details,
        MousePane::Output => panes.activity,
        MousePane::Other => return Some(area),
    };
    let inner = Rect::new(
        pane.x + 1,
        pane.y + 1,
        pane.width.saturating_sub(2),
        pane.height.saturating_sub(2),
    );
    (inner.width > 0 && inner.height > 0).then_some(inner)
}

/// Reverse-video the selected cells of a rendered frame.
pub(crate) fn highlight(buffer: &mut Buffer, selection: &Selection) {
    let area = buffer.area;
    for (row, from, to) in selection.spans() {
        for column in from..=to {
            if column < area.right()
                && row < area.bottom()
                && let Some(cell) = buffer.cell_mut((column, row))
            {
                cell.modifier.insert(Modifier::REVERSED);
            }
        }
    }
}

/// The selected text of a rendered frame, trailing blanks trimmed per line.
pub(crate) fn extract(buffer: &Buffer, selection: &Selection) -> String {
    let area = buffer.area;
    selection
        .spans()
        .into_iter()
        .filter(|(row, _, _)| *row < area.bottom())
        .map(|(row, from, to)| {
            let mut line = String::new();
            for column in from..=to.min(area.right().saturating_sub(1)) {
                if let Some(cell) = buffer.cell((column, row)) {
                    line.push_str(cell.symbol());
                }
            }
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn scroll(model: &mut Model, event: MouseEvent, area: Rect) -> bool {
    let up = match event.kind {
        MouseEventKind::ScrollUp => true,
        MouseEventKind::ScrollDown => false,
        _ => return false,
    };
    let key = KeyEvent::new(
        if up {
            KeyCode::Char('k')
        } else {
            KeyCode::Char('j')
        },
        KeyModifiers::NONE,
    );
    if model.help {
        let maximum = crate::help::filtered_lines(&model.help_query.text())
            .len()
            .saturating_sub(1)
            .max(model.help_max_scroll);
        model.help_scroll = if up {
            model.help_scroll.saturating_sub(3)
        } else {
            model.help_scroll.saturating_add(3).min(maximum)
        };
        return true;
    }
    if model.show_perf || !matches!(&model.interaction, Interaction::None) || model.yank.is_some() {
        if !matches!(&model.interaction, Interaction::Text(_)) && model.title_prompt.is_none() {
            let _ = model.input(key, area.height.saturating_sub(4) as usize);
        }
        return true;
    }
    let pane = mouse_pane(
        area,
        model.board.full_width_activity,
        event.column,
        event.row,
    );
    if model.history.active {
        match pane {
            MousePane::Details => {
                model.history.scroll = if up {
                    model.history.scroll.saturating_sub(3)
                } else {
                    model.history.scroll.saturating_add(3)
                };
            }
            MousePane::List => {
                let _ = model.history_input(key, area.height.saturating_sub(4) as usize);
            }
            MousePane::Output => model.output.scroll(up),
            _ => return false,
        }
        return true;
    }
    match pane {
        MousePane::List => {
            // The wheel scrolls the viewport; the cursor stays put (TS
            // `scroll-list.tsx`). Rendering clamps the offset.
            model.list_free_scroll = true;
            model.offset = if up {
                model.offset.saturating_sub(3)
            } else {
                model.offset.saturating_add(3)
            };
            true
        }
        MousePane::Details => {
            model.details_scroll = if up {
                model.details_scroll.saturating_sub(3)
            } else {
                model.details_scroll.saturating_add(3)
            };
            true
        }
        MousePane::Output => {
            model.output.scroll(up);
            true
        }
        MousePane::Other => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttentionLine, Board, BoardRow};
    use std::sync::Arc;

    #[test]
    fn wheel_scrolls_the_pane_under_the_pointer() {
        let board = Board {
            rows: (0..6)
                .map(|index| BoardRow {
                    key: index.to_string(),
                    slug: index.to_string(),
                    ..Default::default()
                })
                .collect(),
            sections: vec![crate::BoardSection {
                key: "inbox".into(),
                title: "Inbox".into(),
                rows: (0..6).collect(),
                ..Default::default()
            }],
            attention: (0..30)
                .map(|index| AttentionLine {
                    at_ms: index,
                    source: "test".into(),
                    text: index.to_string(),
                })
                .collect(),
            ..Default::default()
        };
        let mut model = Model::default();
        model.apply(wt_runtime::SourceSnapshot {
            data: Some(Arc::new(board)),
            state: wt_runtime::SourceState::Ready,
            updated_at: None,
            revision: 1,
        });
        let area = Rect::new(0, 0, 100, 30);
        let list = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        assert!(scroll(&mut model, list, area));
        assert_eq!(model.selected_row().unwrap().key, "0");
        assert_eq!(model.offset, 3);
        let details = MouseEvent {
            column: 80,
            row: 4,
            ..list
        };
        assert!(scroll(&mut model, details, area));
        assert_eq!(model.details_scroll, 3);
        model.output_view(3, 20);
        let output = MouseEvent {
            column: 80,
            row: 25,
            kind: MouseEventKind::ScrollUp,
            ..list
        };
        assert!(scroll(&mut model, output, area));
        assert!(model.output_view(3, 20).1.len() <= 3);
        assert!(format!("{:?}", model.output_view(3, 20).1).contains("26"));
    }

    #[test]
    fn output_wheel_keeps_working_while_history_view_is_open() {
        let board = Board {
            attention: (0..30)
                .map(|index| AttentionLine {
                    at_ms: index,
                    source: "test".into(),
                    text: index.to_string(),
                })
                .collect(),
            ..Default::default()
        };
        let mut model = Model::default();
        model.apply(wt_runtime::SourceSnapshot {
            data: Some(Arc::new(board)),
            state: wt_runtime::SourceState::Ready,
            updated_at: None,
            revision: 1,
        });
        model.history.active = true;
        model.output_view(3, 30);
        let output = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 80,
            row: 25,
            modifiers: KeyModifiers::NONE,
        };
        assert!(scroll(&mut model, output, Rect::new(0, 0, 100, 30)));
        assert!(format!("{:?}", model.output_view(3, 30).1).contains("25"));
    }
}
