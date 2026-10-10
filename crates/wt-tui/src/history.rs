//! Removed-history interaction reads prepared snapshots only.
use crate::{Model, UiAction, model::InputResult};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
};

#[derive(Default)]
pub(crate) struct HistoryView {
    pub active: bool,
    pub selected_key: Option<String>,
    pub selected: usize,
    pub offset: usize,
    pub scroll: u16,
}

/// These operate on the application or an explicit special slot, never on
/// the live row hidden behind the history view.
pub(crate) fn is_global_key(key: KeyEvent) -> bool {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    if control {
        return matches!(key.code, KeyCode::Char('c' | 'r' | 'R' | 'n'))
            || (shift && matches!(key.code, KeyCode::Char('a' | 'A')));
    }
    matches!(
        key.code,
        KeyCode::Char(
            '?' | 'P'
                | 'q'
                | 'r'
                | 'n'
                | 'c'
                | 'A'
                | 'm'
                | 'M'
                | ','
                | '.'
                | '/'
                | '<'
                | '>'
                | '\\'
        ) | KeyCode::BackTab
    ) && !(shift && key.code == KeyCode::Char('n'))
}

impl Model {
    pub(crate) fn reconcile_history(&mut self) {
        let rows = &self.board.removed_history.rows;
        self.history.selected = self
            .history
            .selected_key
            .as_ref()
            .and_then(|key| rows.iter().position(|row| &row.key == key))
            .unwrap_or(self.history.selected)
            .min(rows.len().saturating_sub(1));
        self.history.selected_key = rows.get(self.history.selected).map(|row| row.key.clone());
    }

    pub(crate) fn history_input(&mut self, key: KeyEvent, height: usize) -> InputResult {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('h')) {
            self.history.active = false;
            return InputResult::Action(UiAction::SetHistoryActive { active: false });
        }
        if key.code == KeyCode::Char('q') || (control && key.code == KeyCode::Char('c')) {
            return InputResult::Quit;
        }
        if key.code == KeyCode::Char('?') {
            self.help = true;
            return InputResult::Draw;
        }
        if key.code == KeyCode::Char('r') {
            return InputResult::Refresh;
        }
        self.reconcile_history();
        let count = self.board.removed_history.rows.len();
        let next = match key.code {
            KeyCode::Char('j') if control => {
                self.history.scroll = self.history.scroll.saturating_add(3);
                return InputResult::Draw;
            }
            KeyCode::Char('k') if control => {
                self.history.scroll = self.history.scroll.saturating_sub(3);
                return InputResult::Draw;
            }
            KeyCode::Down | KeyCode::Char('j') => Some(self.history.selected.saturating_add(1)),
            KeyCode::Up | KeyCode::Char('k') => Some(self.history.selected.saturating_sub(1)),
            KeyCode::Home | KeyCode::Char('g') => Some(0),
            KeyCode::End | KeyCode::Char('G') => Some(count.saturating_sub(1)),
            KeyCode::PageDown => Some(self.history.selected.saturating_add(height / 2)),
            KeyCode::PageUp => Some(self.history.selected.saturating_sub(height / 2)),
            _ => None,
        };
        if let Some(next) = next {
            self.history.selected = next.min(count.saturating_sub(1));
            self.history.selected_key = self
                .board
                .removed_history
                .rows
                .get(self.history.selected)
                .map(|row| row.key.clone());
            self.history.scroll = 0;
            return InputResult::Draw;
        }
        let Some(row) = self.board.removed_history.rows.get(self.history.selected) else {
            return InputResult::Unchanged;
        };
        let action = match key.code {
            KeyCode::Enter => UiAction::PrepareRestoreRemoved {
                key: row.key.clone(),
            },
            KeyCode::Char('a' | 'A') if control => UiAction::ToggleRemovedAutomations {
                key: row.key.clone(),
            },
            KeyCode::Char('p') => match &row.pr_url {
                Some(url) => UiAction::OpenLink { url: url.clone() },
                None => return InputResult::Unchanged,
            },
            KeyCode::Char('i') => match &row.issue_url {
                Some(url) => UiAction::OpenLink { url: url.clone() },
                None => return InputResult::Unchanged,
            },
            KeyCode::Char('y') => UiAction::Copy {
                value: row.branch.clone(),
                label: "branch".into(),
            },
            _ => return InputResult::Unchanged,
        };
        InputResult::Action(action)
    }
}

pub(crate) fn render(frame: &mut Frame<'_>, model: &mut Model, list: Rect, details: Rect) {
    model.reconcile_history();
    let rows = &model.board.removed_history.rows;
    let height = list.height.saturating_sub(2) as usize;
    if model.history.selected < model.history.offset {
        model.history.offset = model.history.selected;
    }
    if model.history.selected >= model.history.offset + height {
        model.history.offset = model
            .history
            .selected
            .saturating_add(1)
            .saturating_sub(height);
    }
    let lines = rows
        .iter()
        .enumerate()
        .skip(model.history.offset)
        .take(height)
        .map(|(index, row)| {
            Line::from(format!(
                "{} {}  {}{}",
                if index == model.history.selected {
                    "›"
                } else {
                    " "
                },
                row.slug,
                row.title,
                if row.automations_paused {
                    " [auto paused]"
                } else {
                    ""
                }
            ))
            .style(if index == model.history.selected {
                Style::new().fg(Color::Cyan)
            } else {
                Style::new()
            })
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Removed · {} · h returns", rows.len())),
        ),
        list,
    );
    let lines = rows
        .get(model.history.selected)
        .map(|row| {
            let mut lines = vec![
                Line::from(row.title.clone()),
                Line::from(row.branch.clone()),
                Line::from(format!("Removed: {}", row.removed_at)),
            ];
            if let Some(host) = &row.host {
                lines.push(Line::from(format!("Host: {host}")));
            }
            lines.extend(row.details.iter().cloned().map(Line::from));
            lines.push(Line::from("Enter restores · p PR · i issue · y branch"));
            lines
        })
        .unwrap_or_else(|| vec![Line::from("No recently removed worktrees")]);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((model.history.scroll, 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Removal record"),
            ),
        details,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Board, RemovedHistoryRow};
    use std::sync::Arc;

    #[test]
    fn history_keys_never_mutate_hidden_live_selection_and_keep_remote_identity() {
        let key = "@remote/builder/same".to_owned();
        let mut model = Model {
            board: Arc::new(Board {
                removed_history: crate::RemovedHistorySnapshot {
                    rows: vec![RemovedHistoryRow {
                        key: key.clone(),
                        slug: "same".into(),
                        branch: "feature/same".into(),
                        ..Default::default()
                    }],
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        model.history.active = true;
        assert_eq!(
            model.history_input(KeyEvent::from(KeyCode::Char('d')), 20),
            InputResult::Unchanged
        );
        assert_eq!(
            model.history_input(KeyEvent::from(KeyCode::Enter), 20),
            InputResult::Action(UiAction::PrepareRestoreRemoved { key: key.clone() })
        );
        assert_eq!(
            model.history_input(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), 20),
            InputResult::Action(UiAction::ToggleRemovedAutomations { key })
        );
        assert_eq!(
            model.history_input(KeyEvent::from(KeyCode::Esc), 20),
            InputResult::Action(UiAction::SetHistoryActive { active: false })
        );
        assert!(!model.history.active);
    }
}
