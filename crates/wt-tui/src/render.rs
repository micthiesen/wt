use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;
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
                if model.board.automations_paused {
                    "  auto paused"
                } else {
                    ""
                },
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {} worktrees  {state}", model.board.rows.len()),
                Style::new().fg(MUTED),
            ),
            Span::styled(
                format!("  {}", model.board.usage.join(" · ")),
                Style::new().fg(MUTED),
            ),
        ])),
        header,
    );
    let (list, details, activity) = if model.board.full_width_activity && area.width >= 80 {
        let [top, activity] =
            Layout::vertical([Constraint::Max(22), Constraint::Min(4)]).areas(body);
        let [list, details] =
            Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(top);
        (list, details, activity)
    } else {
        let [list, right] = if area.width >= 80 {
            Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(body)
        } else {
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body)
        };
        let [details, activity] =
            Layout::vertical([Constraint::Percentage(70), Constraint::Percentage(30)]).areas(right);
        (list, details, activity)
    };
    if model.history.active {
        crate::history::render(frame, model, list, details);
    } else {
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
                if let Some(status) = &row.issue_status {
                    lines.push(Line::from(format!("Tracker: {status}")));
                }
                lines.extend(row.details.iter().map(|line| Line::from(line.as_str())));
                if model.show_verification
                    && let Some(steps) = &row.verify_steps
                {
                    lines.push(Line::default());
                    lines.extend(steps.lines().map(Line::from));
                }
                for session in &row.sessions {
                    lines.push(Line::default());
                    lines.push(Line::styled(
                        format!(
                            "{} / {}: {}{}",
                            session.harness,
                            session.name,
                            session.state,
                            if session.queued > 0 {
                                format!(" · {} queued", session.queued)
                            } else {
                                String::new()
                            }
                        ),
                        Style::new().fg(Color::Cyan),
                    ));
                }
                lines
            })
            .unwrap_or_else(|| {
                if let Some(review) = model.selected_review() {
                    let mut lines = vec![
                        Line::styled(&review.title, Style::new().add_modifier(Modifier::BOLD)),
                        Line::from(format!(
                            "#{} · {} · {}",
                            review.number, review.author, review.branch
                        )),
                        Line::from(review.url.as_str()),
                        Line::default(),
                    ];
                    lines.extend(review.details.iter().map(|line| Line::from(line.as_str())));
                    lines.push(Line::from("w checkout · p open PR · d dismiss"));
                    lines
                } else {
                    match model.selected_section() {
                        Some(section) => {
                            let mut lines = vec![
                                Line::styled(
                                    &section.title,
                                    Style::new().add_modifier(Modifier::BOLD),
                                ),
                                Line::from(format!(
                                    "{} worktrees · Tab to {}",
                                    section.rows.len(),
                                    if section.folded { "expand" } else { "fold" }
                                )),
                                Line::default(),
                            ];
                            lines.extend(
                                section
                                    .rows
                                    .iter()
                                    .filter_map(|&index| model.board.rows.get(index))
                                    .map(|row| {
                                        Line::from(format!(
                                            "{}: {}  {}",
                                            row.slug, row.title, row.badge
                                        ))
                                    }),
                            );
                            lines
                        }
                        None => vec![Line::from(if model.board.review_requests.is_empty() {
                            "No worktrees"
                        } else {
                            "Requested reviews · Tab to fold or expand"
                        })],
                    }
                }
            });
        frame.render_widget(
            Paragraph::new(lines)
                .block(panel("Details"))
                .wrap(Wrap { trim: false })
                .scroll((model.details_scroll, 0)),
            details,
        );
    }
    let available = activity.height.saturating_sub(2) as usize;
    let available_width = activity.width.saturating_sub(2) as usize;
    let (title, activity_lines) = model.output_view(available, available_width);
    frame.render_widget(
        Paragraph::new(activity_lines).block(panel(&title)),
        activity,
    );
    if let Interaction::Text(prompt) = &model.interaction {
        let label = prompt.prompt.as_str();
        let label_width = label.width().min(u16::MAX as usize) as u16;
        let room = footer.width.saturating_sub(label_width) as usize;
        let (text, cursor) = prompt.editor.viewport(room);
        frame.render_widget(Paragraph::new(format!("{label}{text}")), footer);
        if room > 0 {
            frame.set_cursor_position((footer.x + label_width + cursor, footer.y));
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
        let overlay = centered(
            area,
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, overlay);
        let mut lines = vec![Line::from(format!(
            "Frames: {} · last draw: {} µs · continuous: {}",
            model.frame_count,
            model.last_frame_micros,
            if model.perf_continuous { "on" } else { "off" }
        ))];
        if model.board.perf.is_empty() {
            lines.push(Line::from("Sampling…"));
        }
        lines.extend(
            model
                .board
                .perf
                .iter()
                .skip(model.perf_scroll)
                .take(overlay.height.saturating_sub(3) as usize)
                .map(|line| Line::from(line.as_str())),
        );
        frame.render_widget(
            Paragraph::new(lines).block(panel(
                "Performance · r sample · i continuous · j/k scroll · P closes",
            )),
            overlay,
        );
    }
    if model.help {
        let overlay = centered(area, 74, area.height.saturating_sub(2));
        frame.render_widget(Clear, overlay);
        let query = model.help_query.text();
        let filtered = crate::help::filtered_lines(&query);
        let visible_lines = overlay.height.saturating_sub(3) as usize;
        model.help_scroll = model
            .help_scroll
            .min(filtered.len().saturating_sub(visible_lines));
        let mut lines = filtered
            .iter()
            .skip(model.help_scroll)
            .take(visible_lines)
            .map(|line| Line::from(*line))
            .collect::<Vec<_>>();
        if filtered.is_empty() {
            lines.push(Line::from("No matching help entries"));
        }
        let title = if model.help_searching {
            "wt keymap · / filter · Enter done · Esc clear"
        } else if query.is_empty() {
            "wt keymap · / search · j/k scroll · Esc closes"
        } else {
            "wt keymap · / search · Esc clear · q closes"
        };
        frame.render_widget(Paragraph::new(lines).block(panel(title)), overlay);
        if model.help_searching && overlay.width > 4 {
            let label = "/";
            let (text, cursor) = model
                .help_query
                .viewport(overlay.width.saturating_sub(4) as usize);
            frame.set_cursor_position((overlay.x + 2 + cursor, overlay.y + overlay.height - 2));
            frame.render_widget(
                Paragraph::new(format!("{label}{text}")),
                Rect::new(
                    overlay.x + 1,
                    overlay.y + overlay.height - 2,
                    overlay.width - 2,
                    1,
                ),
            );
        }
    }
    match &model.interaction {
        Interaction::Reviewers(picker) => {
            let height = (picker.candidates.len() as u16 + 2).min(area.height.saturating_sub(2));
            let visible = height.saturating_sub(2) as usize;
            let offset = picker
                .selected
                .saturating_sub(visible / 2)
                .min(picker.candidates.len().saturating_sub(visible));
            let overlay = centered(area, area.width.saturating_sub(4).min(80), height);
            frame.render_widget(Clear, overlay);
            let lines = picker
                .candidates
                .iter()
                .enumerate()
                .skip(offset)
                .take(visible)
                .map(|(index, option)| {
                    Line::from(format!(
                        "{} [{}] {}",
                        if index == picker.selected { "›" } else { " " },
                        if option.selected { "x" } else { " " },
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
                Paragraph::new(lines).block(panel(
                    "Reviewers · Space toggles · v/Enter submits · Esc cancels",
                )),
                overlay,
            );
        }
        Interaction::Log {
            title,
            lines,
            scroll,
        } => {
            let overlay = centered(
                area,
                area.width.saturating_sub(4),
                area.height.saturating_sub(4),
            );
            frame.render_widget(Clear, overlay);
            let lines = lines
                .iter()
                .skip(*scroll)
                .take(overlay.height.saturating_sub(2) as usize)
                .map(|line| Line::from(line.as_str()))
                .collect::<Vec<_>>();
            frame.render_widget(
                Paragraph::new(lines)
                    .block(panel_owned(format!("{title} · j/k scroll · Esc closes"))),
                overlay,
            );
        }
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
            let visible = height.saturating_sub(2) as usize;
            let offset = picker
                .selected
                .saturating_sub(visible / 2)
                .min(picker.options.len().saturating_sub(visible));
            let overlay = centered(area, area.width.saturating_sub(4).min(72), height);
            frame.render_widget(Clear, overlay);
            let lines = picker
                .options
                .iter()
                .enumerate()
                .skip(offset)
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
                Paragraph::new(lines).block(panel_owned(format!(
                    "{} · Enter confirms · Esc cancels",
                    picker.title
                ))),
                overlay,
            );
        }
        Interaction::Text(_) | Interaction::None => {}
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MousePane {
    List,
    Details,
    Output,
    Other,
}

pub(crate) fn mouse_pane(area: Rect, full_width_activity: bool, x: u16, y: u16) -> MousePane {
    let [_, body, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let (list, details, output) = if full_width_activity && area.width >= 80 {
        let [top, output] = Layout::vertical([Constraint::Max(22), Constraint::Min(4)]).areas(body);
        let [list, details] =
            Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(top);
        (list, details, output)
    } else {
        let [list, right] = if area.width >= 80 {
            Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(body)
        } else {
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body)
        };
        let [details, output] =
            Layout::vertical([Constraint::Percentage(70), Constraint::Percentage(30)]).areas(right);
        (list, details, output)
    };
    let contains = |rect: Rect| {
        x >= rect.x
            && x < rect.x.saturating_add(rect.width)
            && y >= rect.y
            && y < rect.y.saturating_add(rect.height)
    };
    if contains(list) {
        MousePane::List
    } else if contains(details) {
        MousePane::Details
    } else if contains(output) {
        MousePane::Output
    } else {
        MousePane::Other
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
    let lines: Vec<_> = (0..model.item_count())
        .skip(model.offset)
        .take(inner.height as usize)
        .map(|index| {
            let selected = model.selected == Some(index);
            let style = if selected {
                Style::new().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::new()
            };
            match model.item(index) {
                Some(crate::model::VisualItem::Row(index)) => {
                    let row = &model.board.rows[index];
                    Line::from(vec![
                        Span::raw(if selected { "› " } else { "  " }),
                        Span::styled(&row.stack_prefix, Style::new().fg(MUTED)),
                        Span::raw(&row.title),
                        Span::styled(
                            row.issue_status
                                .as_ref()
                                .map(|status| format!("  {status}"))
                                .unwrap_or_default(),
                            Style::new().fg(Color::Blue),
                        ),
                        Span::styled(format!("  {}", row.badge), Style::new().fg(Color::Cyan)),
                    ])
                    .style(style)
                }
                Some(crate::model::VisualItem::Section(index)) => {
                    let section = &model.board.sections[index];
                    let attention = section
                        .rows
                        .iter()
                        .filter(|&&index| model.board.rows[index].needs_attention)
                        .count();
                    let summary = if section.folded && attention > 0 {
                        format!(" · {attention} need attention")
                    } else {
                        String::new()
                    };
                    Line::from(format!(
                        "{} {} {} ({}){summary}",
                        if selected { "›" } else { " " },
                        if section.folded { "▸" } else { "▾" },
                        section.title,
                        section.rows.len()
                    ))
                    .style(style.fg(Color::Cyan).add_modifier(Modifier::BOLD))
                }
                Some(crate::model::VisualItem::ReviewHeader) => Line::from(format!(
                    "{} {} Requested reviews ({})",
                    if selected { "›" } else { " " },
                    if model.reviews_folded { "▸" } else { "▾" },
                    model.board.review_requests.len(),
                ))
                .style(style.fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Some(crate::model::VisualItem::ReviewRequest(index)) => {
                    let row = &model.board.review_requests[index];
                    Line::from(format!(
                        "{} #{} {} · {}",
                        if selected { "›" } else { " " },
                        row.number,
                        row.title,
                        row.author
                    ))
                    .style(style)
                }
                None => Line::default(),
            }
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
    fn picker_keeps_late_selection_visible_after_terminal_shrinks() {
        let mut model = Model {
            interaction: Interaction::Picker(crate::model::PickerPrompt {
                action: PickerAction::Base { key: "one".into() },
                title: "Fork base".into(),
                options: (0..30)
                    .map(|index| PickerOption {
                        value: Some(index.to_string()),
                        label: format!("branch-{index:02} {}", "wide label ".repeat(10)),
                        chord: None,
                        note: None,
                        verify_after_merge: None,
                    })
                    .collect(),
                selected: 27,
            }),
            ..Default::default()
        };
        for (width, height) in [(100, 30), (30, 7), (20, 5)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render(frame, &mut model)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                text.contains("› branch-27"),
                "selected option missing at {width}x{height}: {text}"
            );
        }
    }

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
                        published_base: None,
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
