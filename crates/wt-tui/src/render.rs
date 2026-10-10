use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;
use wt_runtime::SourceState;

use crate::{BoardRow, Interaction, Model};

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
                lines.extend(work_status_lines(row, model.show_verification));
                if row.work.is_some() {
                    lines.push(Line::default());
                }
                for group in &row.detail_groups {
                    for line in &group.lines {
                        lines.push(Line::from(vec![
                            Span::styled(format!("{}: ", group.label), Style::new().fg(MUTED)),
                            Span::raw(line.clone()),
                        ]));
                    }
                    if let Some(error) = &group.error {
                        lines.push(Line::styled(
                            format!("{}: {}", group.label, error),
                            Style::new().fg(Color::Red),
                        ));
                    }
                }
                lines.extend(row.details.iter().map(|line| Line::from(line.as_str())));
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
                            lines.extend(section_rollup_lines(section));
                            lines.push(Line::default());
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
        let footer_text =
            if model.toast.is_none() && !matches!(&model.source_state, SourceState::Failed(_)) {
                let status = slot_status_summary(&model.board);
                if status.is_empty() {
                    footer_text.to_owned()
                } else {
                    format!("{status}  {footer_text}")
                }
            } else {
                footer_text.to_owned()
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

fn slot_status_summary(board: &crate::Board) -> String {
    ["manager", "main", "wt", "dotfiles"]
        .into_iter()
        .filter_map(|key| {
            let sessions = board.slot_sessions.get(key)?;
            let session = sessions.iter().find(|session| session.live)?;
            let label = match key {
                "manager" => "M",
                "main" => "main",
                "wt" => "wt",
                "dotfiles" => "dotfiles",
                _ => return None,
            };
            let context = if key == "manager" && session.harness.eq_ignore_ascii_case("claude") {
                session
                    .context_percent
                    .map(|percent| format!(" {percent}%"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            Some(format!("{label}:{}{context}", session.state))
        })
        .collect::<Vec<_>>()
        .join(" · ")
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

fn work_marker(row: &BoardRow) -> Span<'static> {
    let Some(work) = row.work.as_ref() else {
        return Span::styled("○ ", Style::new().fg(MUTED));
    };
    let Some(state) = work.effective_state else {
        return Span::styled("○ ", Style::new().fg(MUTED));
    };
    let (glyph, color) = if work.verification_overdue {
        ("●", Color::Red)
    } else if work.blocked {
        ("⊘", Color::Yellow)
    } else {
        let color = work_state_color(state);
        let glyph = match state {
            wt_core::WorkState::Todo => "○",
            wt_core::WorkState::Verified => "✓",
            wt_core::WorkState::Dropped => "⊘",
            _ => "●",
        };
        if work.stale == Some(true) && !work.derived {
            ("○", color)
        } else {
            (glyph, color)
        }
    };
    Span::styled(format!("{glyph} "), Style::new().fg(color))
}

fn work_state_color(state: wt_core::WorkState) -> Color {
    match state {
        wt_core::WorkState::Ready | wt_core::WorkState::Verified => Color::Green,
        wt_core::WorkState::NeedsHuman => Color::Red,
        wt_core::WorkState::NeedsTesting | wt_core::WorkState::Working => Color::Yellow,
        wt_core::WorkState::Review => Color::Cyan,
        wt_core::WorkState::Todo | wt_core::WorkState::Dropped => MUTED,
    }
}

fn work_status_lines(row: &BoardRow, show_verification: bool) -> Vec<Line<'static>> {
    let Some(work) = row.work.as_ref() else {
        return Vec::new();
    };
    let Some(state) = work.effective_state else {
        return Vec::new();
    };
    let state_text = if work.verification_overdue {
        format!("unverified · {} · overdue", state.as_str())
    } else if work.verification_owed {
        format!("unverified · {}", state.as_str())
    } else if work.blocked {
        format!("blocked · {}", state.as_str())
    } else if work.derived {
        format!("{} · live", state.as_str())
    } else {
        state.as_str().to_owned()
    };
    let status_color = if work.verification_overdue {
        Color::Red
    } else if work.blocked {
        Color::Yellow
    } else {
        work_state_color(state)
    };
    let mut spans = vec![Span::styled(
        state_text,
        Style::new().fg(status_color).add_modifier(Modifier::BOLD),
    )];
    if let Some(risk) = work.record.as_ref().and_then(|record| record.risk) {
        spans.push(Span::styled(" · risk ", Style::new().fg(MUTED)));
        spans.push(Span::styled(
            risk.as_str(),
            Style::new().fg(match risk {
                wt_core::WorkRisk::Low => Color::Green,
                wt_core::WorkRisk::Medium => Color::Yellow,
                wt_core::WorkRisk::High => Color::Red,
            }),
        ));
    }
    if let Some(age) = work.age.as_ref().filter(|_| !work.derived) {
        spans.push(Span::styled(
            format!(" · {age} ago"),
            Style::new().fg(MUTED),
        ));
    }
    if work.stale == Some(true) && !work.derived {
        spans.push(Span::styled(
            " · commits since",
            Style::new().fg(Color::Yellow),
        ));
    }
    let mut lines = vec![Line::from(spans)];
    if let Some(blocked_on) = work
        .record
        .as_ref()
        .and_then(|record| record.blocked_on.as_deref())
    {
        lines.push(Line::styled(
            format!("blocked on: {blocked_on}"),
            Style::new().fg(Color::Yellow),
        ));
    }
    if let Some(note) = work
        .record
        .as_ref()
        .and_then(|record| record.note.as_deref())
    {
        lines.extend(note.lines().map(|line| {
            Line::from(vec![
                Span::styled("  ", Style::new()),
                Span::raw(line.to_owned()),
            ])
        }));
    }
    if let Some(steps) = work
        .record
        .as_ref()
        .and_then(|record| record.verify_after_merge.as_deref())
    {
        if work.verification_owed {
            lines.push(Line::styled(
                "Post-merge verification is owed · V shows steps",
                Style::new().fg(if work.verification_overdue {
                    Color::Red
                } else {
                    Color::Yellow
                }),
            ));
        }
        if show_verification {
            lines.extend(steps.lines().map(|line| Line::from(line.to_owned())));
        }
    }
    lines
}

fn folded_summary(rollup: &crate::SectionRollup) -> String {
    let mut parts = Vec::new();
    for entry in &rollup.states {
        let name = entry.state.map_or("unset", wt_core::WorkState::as_str);
        parts.push(format!("{} {name}", entry.count));
    }
    for entry in &rollup.risks {
        parts.push(format!("{} {} risk", entry.count, entry.risk.as_str()));
    }
    if rollup.stale_statuses > 0 {
        parts.push(format!("{} stale status", rollup.stale_statuses));
    }
    if rollup.verification_owed > 0 {
        parts.push(format!("{} verify owed", rollup.verification_owed));
    }
    if rollup.verification_overdue > 0 {
        parts.push(format!("{} overdue verify", rollup.verification_overdue));
    }
    if rollup.dirty_worktrees.is_some_and(|count| count > 0) {
        parts.push(format!(
            "{} dirty",
            rollup.dirty_worktrees.unwrap_or_default()
        ));
    }
    if rollup.unknown_git > 0 {
        parts.push(format!("{} Git unknown", rollup.unknown_git));
    }
    if rollup.upstream_ahead > 0 {
        parts.push(format!("{} ahead upstream", rollup.upstream_ahead));
    }
    if rollup.upstream_behind > 0 {
        parts.push(format!("{} behind upstream", rollup.upstream_behind));
    }
    if rollup.rebasing > 0 {
        parts.push(format!("{} rebasing", rollup.rebasing));
    }
    if rollup.conflicted > 0 {
        parts.push(format!("{} conflicts", rollup.conflicted));
    }
    if rollup.open_prs > 0 {
        parts.push(format!("{} open PR", rollup.open_prs));
    }
    if rollup.draft_prs > 0 {
        parts.push(format!("{} draft", rollup.draft_prs));
    }
    if rollup.queued_prs > 0 {
        parts.push(format!("{} queued", rollup.queued_prs));
    }
    if rollup.failing_checks > 0 {
        parts.push(format!("{} red CI", rollup.failing_checks));
    }
    if rollup.paused_automations > 0 {
        parts.push(format!("{} paused", rollup.paused_automations));
    }
    if rollup.needs_attention > 0 {
        parts.push(format!("{} attention", rollup.needs_attention));
    }
    for note in rollup.blocked_notes.iter().take(2) {
        parts.push(format!("blocked {}", truncate(note, 48)));
    }
    if rollup.blocked_notes.len() > 2 {
        parts.push(format!("+{} blockers", rollup.blocked_notes.len() - 2));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(" · "))
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn section_rollup_lines(section: &crate::BoardSection) -> Vec<Line<'static>> {
    let rollup = &section.rollup;
    let states = rollup
        .states
        .iter()
        .map(|entry| {
            format!(
                "{} {}",
                entry.count,
                entry.state.map_or("unset", wt_core::WorkState::as_str)
            )
        })
        .collect::<Vec<_>>();
    let risks = rollup
        .risks
        .iter()
        .map(|entry| format!("{} {}", entry.count, entry.risk.as_str()))
        .collect::<Vec<_>>();
    let mut lines = vec![Line::from(format!("Work: {}", states.join(" · ")))];
    if !risks.is_empty() {
        lines.push(Line::from(format!("Risk: {}", risks.join(" · "))));
    }
    let mut mechanics = Vec::new();
    if rollup.stale_statuses > 0 {
        mechanics.push(format!("{} stale status", rollup.stale_statuses));
    }
    if rollup.verification_owed > 0 {
        mechanics.push(format!("{} verify owed", rollup.verification_owed));
    }
    if rollup.verification_overdue > 0 {
        mechanics.push(format!("{} overdue verify", rollup.verification_overdue));
    }
    if let Some(dirty) = rollup.dirty_worktrees.filter(|count| *count > 0) {
        mechanics.push(format!("{dirty} dirty"));
    }
    if rollup.unknown_git > 0 {
        mechanics.push(format!("{} Git unknown", rollup.unknown_git));
    }
    if rollup.upstream_ahead > 0 {
        mechanics.push(format!("{} ahead upstream", rollup.upstream_ahead));
    }
    if rollup.upstream_behind > 0 {
        mechanics.push(format!("{} behind upstream", rollup.upstream_behind));
    }
    if rollup.rebasing > 0 {
        mechanics.push(format!("{} rebasing", rollup.rebasing));
    }
    if rollup.conflicted > 0 {
        mechanics.push(format!("{} conflicts", rollup.conflicted));
    }
    if rollup.open_prs > 0 {
        mechanics.push(format!("{} open PR", rollup.open_prs));
    }
    if rollup.draft_prs > 0 {
        mechanics.push(format!("{} draft", rollup.draft_prs));
    }
    if rollup.queued_prs > 0 {
        mechanics.push(format!("{} queued", rollup.queued_prs));
    }
    if rollup.failing_checks > 0 {
        mechanics.push(format!("{} failing CI", rollup.failing_checks));
    }
    if rollup.paused_automations > 0 {
        mechanics.push(format!("{} automation-paused", rollup.paused_automations));
    }
    if !mechanics.is_empty() {
        lines.push(Line::from(format!("Mechanics: {}", mechanics.join(" · "))));
    }
    if rollup.needs_attention > 0 {
        lines.push(Line::styled(
            format!("{} need attention", rollup.needs_attention),
            Style::new().fg(Color::Yellow),
        ));
    }
    lines.extend(rollup.blocked_notes.iter().map(|note| {
        Line::styled(
            format!("Blocked on: {note}"),
            Style::new().fg(Color::Yellow),
        )
    }));
    lines
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
                        work_marker(row),
                        Span::raw(&row.title),
                        Span::styled(
                            row.issue_status
                                .as_ref()
                                .map(|status| format!("  {status}"))
                                .unwrap_or_default(),
                            Style::new().fg(Color::Blue),
                        ),
                        Span::styled(
                            if row.badge.is_empty() {
                                String::new()
                            } else {
                                format!("  {}", row.badge)
                            },
                            Style::new().fg(Color::Cyan),
                        ),
                    ])
                    .style(style)
                }
                Some(crate::model::VisualItem::Section(index)) => {
                    let section = &model.board.sections[index];
                    let summary = if section.folded {
                        folded_summary(&section.rollup)
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
    fn footer_slot_status_uses_only_live_special_sessions() {
        let board = Board {
            slot_sessions: [
                (
                    "manager".into(),
                    vec![crate::SessionView {
                        live: true,
                        harness: "Claude".into(),
                        state: "asking".into(),
                        context_percent: Some(87),
                        ..Default::default()
                    }],
                ),
                (
                    "main".into(),
                    vec![crate::SessionView {
                        live: false,
                        state: "working".into(),
                        ..Default::default()
                    }],
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        assert_eq!(slot_status_summary(&board), "M:asking 87%");
    }

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
    fn narrow_worktree_titles_keep_state_and_stale_marker_visible() {
        let mut model = Model {
            board: Arc::new(Board {
                rows: vec![BoardRow {
                    key: "one".into(),
                    title: "A deliberately narrow title".into(),
                    work: Some(crate::WorkPresentation {
                        effective_state: Some(wt_core::WorkState::Ready),
                        stale: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            selected: Some(0),
            ..Model::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal.draw(|frame| render(frame, &mut model)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("○ A deliberately narrow title"));
        let details = work_status_lines(&model.board.rows[0], false)
            .into_iter()
            .flat_map(|line| line.spans)
            .map(|span| span.content.to_string())
            .collect::<String>();
        assert!(details.contains("ready"));
        assert!(details.contains("commits since"));
    }

    #[test]
    fn folded_section_summary_exposes_states_risk_and_blocker_notes() {
        let rollup = crate::SectionRollup {
            states: vec![crate::WorkStateCount {
                state: Some(wt_core::WorkState::NeedsHuman),
                count: 2,
            }],
            risks: vec![crate::WorkRiskCount {
                risk: wt_core::WorkRisk::High,
                count: 1,
            }],
            blocked_notes: vec!["api: waiting for review".into()],
            ..Default::default()
        };
        let summary = folded_summary(&rollup);
        assert!(summary.contains("2 needs-human"));
        assert!(summary.contains("1 high risk"));
        assert!(summary.contains("blocked api: waiting for review"));
        let section = crate::BoardSection {
            rollup,
            ..Default::default()
        };
        let details = section_rollup_lines(&section)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(details.contains("api: waiting for review"));
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
