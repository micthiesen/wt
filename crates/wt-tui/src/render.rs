use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use wt_runtime::SourceState;

use crate::{Interaction, Model};

const MUTED: Color = Color::DarkGray;

pub(crate) fn render(frame: &mut Frame<'_>, model: &mut Model) {
    let area = frame.area();
    if area.width < 20 || area.height < 5 {
        frame.render_widget(Paragraph::new("Resize terminal to view wt"), area);
        return;
    }
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let state = match &model.source_state {
        SourceState::Empty => "loading",
        SourceState::Refreshing => "refreshing",
        SourceState::Ready => "",
        SourceState::Failed(_) => "refresh failed",
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " wt ",
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw(&model.board.name),
            Span::styled(
                format!("  {} worktrees  {state}", model.board.rows.len()),
                Style::new().fg(MUTED),
            ),
        ])),
        header,
    );
    let [list, right] = if area.width >= 80 {
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(body)
    } else {
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body)
    };
    let [details, activity] =
        Layout::vertical([Constraint::Percentage(70), Constraint::Percentage(30)]).areas(right);
    render_list(frame, model, list);
    let lines = model
        .selected_row()
        .map(|row| {
            let mut lines = vec![
                Line::styled(&row.title, Style::new().add_modifier(Modifier::BOLD)),
                Line::from(row.branch.as_str()),
                Line::styled(&row.path, Style::new().fg(MUTED)),
                Line::default(),
            ];
            lines.extend(row.details.iter().map(|line| Line::from(line.as_str())));
            lines
        })
        .unwrap_or_else(|| vec![Line::from("No worktrees")]);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel("Details"))
            .wrap(Wrap { trim: false })
            .scroll((model.details_scroll, 0)),
        details,
    );
    let available = activity.height.saturating_sub(2) as usize;
    let activity_lines: Vec<_> = model
        .board
        .activity
        .iter()
        .rev()
        .take(available)
        .rev()
        .map(|line| Line::from(line.as_str()))
        .collect();
    frame.render_widget(
        Paragraph::new(activity_lines).block(panel("Activity")),
        activity,
    );
    if let Interaction::Text(prompt) = &model.interaction {
        let label = prompt.prompt.as_str();
        let room = footer.width.saturating_sub(label.len() as u16) as usize;
        let (text, cursor) = prompt.editor.viewport(room);
        frame.render_widget(Paragraph::new(format!("{label}{text}")), footer);
        if room > 0 {
            frame.set_cursor_position((footer.x + label.len() as u16 + cursor, footer.y));
        }
    } else {
        let (footer_text, failed) = if let Some((message, failed)) = &model.toast {
            (message.as_str(), *failed)
        } else {
            match &model.source_state {
                SourceState::Failed(error) => (error.as_ref(), true),
                _ => (
                    " j/k navigate   t title   y copy   r refresh   ? help   q quit",
                    false,
                ),
            }
        };
        frame.render_widget(
            Paragraph::new(footer_text).style(Style::new().fg(if failed {
                Color::Red
            } else {
                MUTED
            })),
            footer,
        );
    }
    if let Some(prompt) = &model.title_prompt {
        let label = "Title: ";
        let room = footer.width.saturating_sub(label.len() as u16) as usize;
        let (text, cursor) = prompt.editor.viewport(room);
        frame.render_widget(Paragraph::new(format!("{label}{text}")), footer);
        if room > 0 {
            frame.set_cursor_position((footer.x + label.len() as u16 + cursor, footer.y));
        }
    }
    if let Some(selected) = model.yank {
        let choices = model.yank_choices();
        let overlay = centered(area, 60, (choices.len() + 3) as u16);
        frame.render_widget(Clear, overlay);
        let lines = choices
            .iter()
            .enumerate()
            .map(|(index, (key, label, value))| {
                Line::from(format!(
                    "{} {key}  {label}: {value}",
                    if index == selected { "›" } else { " " }
                ))
                .style(if index == selected {
                    Style::new().fg(Color::Cyan)
                } else {
                    Style::new()
                })
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(lines).block(panel("Copy · Enter/y confirms · Esc cancels")),
            overlay,
        );
    }
    if model.show_perf {
        let overlay = centered(area, 56, 7);
        frame.render_widget(Clear, overlay);
        frame.render_widget(Paragraph::new(format!(
            "Frames: {}\nLast draw: {} µs\nDraws occur only after input or source changes.\nP closes this view.",
            model.frame_count, model.last_frame_micros
        )).block(panel("Rendering")), overlay);
    }
    if model.help {
        let overlay = centered(area, 62, 18);
        frame.render_widget(Clear, overlay);
        frame.render_widget(Paragraph::new(
            "j / k, ↑ / ↓    Move cursor\ng / G            First / last worktree\nPgUp / PgDn      Half-page navigation\nSpace            Next row needing attention\nCtrl+J / Ctrl+K  Scroll details\nt                Edit title\ny                Copy branch, path or slug\nr                Refresh sources\nP                Rendering metrics\n?                Help\nq / Ctrl+C       Quit\n\nEsc / q / ? closes help"
        ).block(panel("wt keymap")).wrap(Wrap { trim: false }), overlay);
    }
    match &model.interaction {
        Interaction::Confirm(confirm) => {
            let max_lines = area.height.saturating_sub(4) as usize;
            let visible = confirm
                .lines
                .iter()
                .enumerate()
                .skip(confirm.selected.saturating_sub(max_lines.saturating_sub(1)))
                .take(max_lines);
            let lines = visible
                .map(|(index, line)| {
                    Line::from(format!(
                        "{} {line}",
                        if index == confirm.selected {
                            "›"
                        } else {
                            " "
                        }
                    ))
                    .style(if index == confirm.selected {
                        Style::new().fg(Color::Cyan)
                    } else {
                        Style::new()
                    })
                })
                .collect::<Vec<_>>();
            let height = (lines.len() as u16 + 2).min(area.height);
            let overlay = centered(area, area.width.saturating_sub(4).min(72), height);
            frame.render_widget(Clear, overlay);
            frame.render_widget(
                Paragraph::new(lines)
                    .block(panel_owned(format!(
                        "{} · Enter/y confirms · Esc cancels",
                        confirm.title
                    )))
                    .wrap(Wrap { trim: false }),
                overlay,
            );
        }
        Interaction::Picker(picker) => {
            let height = (picker.options.len() as u16 + 2).min(area.height.saturating_sub(2));
            let overlay = centered(area, area.width.saturating_sub(4).min(72), height);
            frame.render_widget(Clear, overlay);
            let lines = picker
                .options
                .iter()
                .enumerate()
                .take(height.saturating_sub(2) as usize)
                .map(|(index, option)| {
                    let chord = option.chord.map(|c| format!("{c} ")).unwrap_or_default();
                    Line::from(format!(
                        "{} {chord}{}",
                        if index == picker.selected { "›" } else { " " },
                        option.label
                    ))
                    .style(if index == picker.selected {
                        Style::new().fg(Color::Cyan)
                    } else {
                        Style::new()
                    })
                })
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(lines)
                    .block(panel_owned(format!(
                        "{} · Enter confirms · Esc cancels",
                        picker.title
                    )))
                    .wrap(Wrap { trim: false }),
                overlay,
            );
        }
        Interaction::Text(_) | Interaction::None => {}
    }
}

fn panel(title: &str) -> Block<'_> {
    Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(MUTED))
        .title(title)
}

fn panel_owned(title: String) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(MUTED))
        .title(title)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn render_list(frame: &mut Frame<'_>, model: &mut Model, area: Rect) {
    let block = panel("Worktrees");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    model.keep_selection_visible(inner.height as usize);
    let lines: Vec<_> = model
        .board
        .rows
        .iter()
        .enumerate()
        .skip(model.offset)
        .take(inner.height as usize)
        .map(|(index, row)| {
            let selected = model.selected == Some(index);
            let style = if selected {
                Style::new().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::new()
            };
            Line::from(vec![
                Span::raw(if selected { "› " } else { "  " }),
                Span::raw(&row.title),
                Span::styled(format!("  {}", row.badge), Style::new().fg(Color::Cyan)),
            ])
            .style(style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ConfirmPrompt;
    use crate::{Board, BoardRow, ConfirmAction, Interaction, PickerAction, PickerOption};
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::Arc;

    #[test]
    fn unicode_and_long_details_render_at_narrow_and_tiny_sizes() {
        let mut model = Model {
            board: Arc::new(Board {
                name: "repository".into(),
                rows: vec![BoardRow {
                    key: "one".into(),
                    title: "界面 e\u{301} task".into(),
                    details: vec!["long detail ".repeat(100)],
                    ..BoardRow::default()
                }],
                ..Board::default()
            }),
            selected: Some(0),
            ..Model::default()
        };
        for (width, height) in [(180, 50), (79, 24), (30, 7), (5, 2), (0, 0)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut model)).unwrap();
            if width >= 20 && height >= 5 {
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(text.contains("repository"));
                if height >= 24 {
                    // Wide glyphs have a continuation cell in the buffer.
                    assert!(text.contains('界'));
                    assert!(text.contains('面'));
                }
            }
        }
    }

    #[test]
    fn confirmation_and_picker_modals_fit_narrow_terminals() {
        let mut model = Model {
            interaction: Interaction::Confirm(ConfirmPrompt {
                action: ConfirmAction::Remove {
                    key: "one".into(),
                    force: true,
                    revision: crate::RemovalRevision {
                        key: "one".into(),
                        path: "/worktrees/one".into(),
                        branch: "feature/one".into(),
                        head: "abc123".into(),
                        digest: "deadbeef".into(),
                        hazards: vec!["uncommitted changes".into()],
                    },
                },
                title: "remove 界面 worktree".into(),
                lines: vec![
                    "dirty changes will be lost".into(),
                    "post-merge verification still owed".into(),
                ],
                selected: 0,
                cancel_key: Some('d'),
            }),
            ..Model::default()
        };
        for (width, height) in [(79, 24), (30, 7), (20, 5), (5, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut model)).unwrap();
        }
        model.interaction = Interaction::Picker(crate::model::PickerPrompt {
            action: PickerAction::Base { key: "one".into() },
            title: "Record fork base".into(),
            options: vec![PickerOption {
                value: Some("feature/界面".into()),
                label: "feature/界面 (current)".into(),
                chord: Some('1'),
                note: None,
                verify_after_merge: None,
            }],
            selected: 0,
        });
        let mut terminal = Terminal::new(TestBackend::new(30, 7)).unwrap();
        terminal.draw(|frame| render(frame, &mut model)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("feature/"));
        assert!(text.contains('界'));
    }
}
