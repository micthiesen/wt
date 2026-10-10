//! The details pane for the selected row, review request, or folded section.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

use super::{MUTED, panel};
use crate::{BoardRow, Model};

pub(crate) fn render(frame: &mut Frame<'_>, model: &mut Model, details: Rect) {
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
                            Line::styled(&section.title, Style::new().add_modifier(Modifier::BOLD)),
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

fn work_state_color(state: wt_core::WorkState) -> Color {
    match state {
        wt_core::WorkState::Ready | wt_core::WorkState::Verified => Color::Green,
        wt_core::WorkState::NeedsHuman => Color::Red,
        wt_core::WorkState::NeedsTesting | wt_core::WorkState::Working => Color::Yellow,
        wt_core::WorkState::Review => Color::Cyan,
        wt_core::WorkState::Todo | wt_core::WorkState::Dropped => MUTED,
    }
}

pub(crate) fn work_status_lines(row: &BoardRow, show_verification: bool) -> Vec<Line<'static>> {
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

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

pub(crate) fn section_rollup_lines(section: &crate::BoardSection) -> Vec<Line<'static>> {
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
