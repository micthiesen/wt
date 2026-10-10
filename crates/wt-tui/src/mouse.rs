use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::{
    Interaction, Model,
    render::{MousePane, mouse_pane},
};

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
            .saturating_sub(1);
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
            let _ = model.input(key, area.height.saturating_sub(4) as usize);
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
        assert_eq!(model.selected_row().unwrap().key, "1");
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
