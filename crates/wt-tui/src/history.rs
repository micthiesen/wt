//! Removed-history interaction reads prepared snapshots only.
use crate::{Model, UiAction, model::InputResult};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Margin, Rect},
    style::{Color, Modifier},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;
use wt_core::WorkState;

use crate::{
    LandingKind,
    badges::{self, Badge},
    glyphs,
    render::{panel, panel_owned, text::truncate_end},
    theme,
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
                | 'O'
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
                Some(url) => UiAction::OpenPrDefault { url: url.clone() },
                None => return self.notify("no PR recorded"),
            },
            KeyCode::Char('i') => match &row.issue_url {
                Some(url) => UiAction::OpenLink { url: url.clone() },
                None => return self.notify("no issue URL recorded"),
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
    let width = list.width.saturating_sub(2) as usize;
    if model.history.selected < model.history.offset {
        model.history.offset = model.history.selected;
    }
    while model.history.selected >= model.history.offset
        && visible_row_count(rows, model.history.offset, height, width)
            <= model.history.selected.saturating_sub(model.history.offset)
    {
        model.history.offset = model.history.offset.saturating_add(1);
    }
    let (mut lines, _) = history_lines(
        rows,
        model.history.offset,
        model.history.selected,
        height,
        width,
    );
    if rows.is_empty() {
        lines = vec![
            Line::default(),
            Line::styled(" No removed worktrees yet.", theme::dim()),
        ];
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel_owned(format!("removed ({})", rows.len()))),
        list,
    );
    let block = panel("removal record");
    let inner = block.inner(details);
    frame.render_widget(block, details);
    let lines = rows
        .get(model.history.selected)
        .map(record_lines)
        .unwrap_or_else(|| {
            vec![
                Line::default(),
                Line::from(vec![
                    Span::styled("h", theme::bold(theme::ACCENT)),
                    Span::styled(" or ", theme::dim()),
                    Span::styled("esc", theme::bold(theme::ACCENT)),
                    Span::styled(" returns to the worktrees", theme::dim()),
                ]),
            ]
        });
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((model.history.scroll, 0)),
        inner.inner(Margin {
            horizontal: 1,
            vertical: 0,
        }),
    );
}

/// What a removal record says about how the branch ended, parsed from the
/// prepared `Label: value` detail lines.
#[derive(Default)]
struct Outcome<'a> {
    work: Option<WorkState>,
    git: Option<&'a str>,
    pr: Option<&'a str>,
}

fn outcome(row: &crate::RemovedHistoryRow) -> Outcome<'_> {
    let mut outcome = Outcome::default();
    for line in &row.details {
        if let Some(state) = line.strip_prefix("Work: ") {
            outcome.work = work_state(state);
        } else if let Some(state) = line.strip_prefix("Git: ") {
            outcome.git = Some(state.trim());
        } else if let Some(state) = line.strip_prefix("PR: ") {
            outcome.pr = Some(state.trim());
        }
    }
    outcome
}

pub(crate) fn work_state(value: &str) -> Option<WorkState> {
    [
        WorkState::Todo,
        WorkState::Working,
        WorkState::Review,
        WorkState::NeedsTesting,
        WorkState::NeedsHuman,
        WorkState::Ready,
        WorkState::Verified,
        WorkState::Dropped,
    ]
    .into_iter()
    .find(|state| state.as_str().eq_ignore_ascii_case(value.trim()))
}

/// The row marker, mirroring the live list: proven landings take the
/// release shape in the work-status hue; otherwise the outcome the record
/// captured (merged, closed, dropped, gone, still open), else a dim trash.
fn status_badge(row: &crate::RemovedHistoryRow) -> Badge {
    let outcome = outcome(row);
    // Finished work (verified, dropped) is gray in the list, but a landed
    // removal still reads as landed: keep the ok hue for those.
    let hue = match outcome.work.map(badges::work_state_color) {
        Some(color) if color != theme::FG_DIM => color,
        _ => theme::OK,
    };
    let badge = |glyph, color| Badge { glyph, color };
    match row.landed_on {
        Some(LandingKind::Production) => return badge(glyphs::PRODUCTION, hue),
        Some(LandingKind::Base) => return badge(glyphs::MERGE, hue),
        None => {}
    }
    let is = |value: Option<&str>, expected: &str| {
        value.is_some_and(|value| value.eq_ignore_ascii_case(expected))
    };
    if is(outcome.pr, "merged") || is(outcome.git, "merged") {
        badge(glyphs::MERGE, theme::OK)
    } else if is(outcome.pr, "closed") {
        badge(glyphs::PR_CLOSED, theme::ERR)
    } else if outcome.work == Some(WorkState::Dropped) {
        badge(glyphs::SLASH, theme::FG_DIM)
    } else if is(outcome.git, "gone") {
        badge(glyphs::SLASH, theme::WARN)
    } else if is(outcome.pr, "open") {
        badge(glyphs::PR_OPEN, theme::ACCENT_ALT)
    } else {
        badge(glyphs::TRASH, theme::FG_DIM)
    }
}

fn pr_badge(row: &crate::RemovedHistoryRow) -> Option<Badge> {
    let state = outcome(row).pr.map(str::to_ascii_uppercase);
    let badge = |glyph, color| Some(Badge { glyph, color });
    match state.as_deref() {
        Some("MERGED") => badge(glyphs::PR_MERGED, theme::INFO),
        Some("CLOSED") => badge(glyphs::PR_CLOSED, theme::ERR),
        Some("OPEN") => badge(glyphs::PR_OPEN, theme::ACCENT_ALT),
        _ if row.pr_url.is_some() => badge(glyphs::PR_OPEN, theme::FG_DIM),
        _ => None,
    }
}

fn issue_badge(status: &str) -> Badge {
    let status = status.to_ascii_lowercase();
    let badge = |glyph, color| Badge { glyph, color };
    if ["done", "complete", "closed", "merged", "released"]
        .iter()
        .any(|word| status.contains(word))
    {
        badge(glyphs::TASK_COMPLETE, theme::OK)
    } else if ["cancel", "duplicate", "won't", "wont"]
        .iter()
        .any(|word| status.contains(word))
    {
        badge(glyphs::TASK_CANCELLED, theme::FG_DIM)
    } else if status.contains("review") {
        badge(glyphs::HALF_CIRCLE, theme::INFO)
    } else if status.contains("progress") || status.contains("started") {
        badge(glyphs::DOT_CIRCLE, theme::ACCENT)
    } else {
        badge(glyphs::DOT_OUTLINE, theme::FG_DIM)
    }
}

fn visible_row_count(
    rows: &[crate::RemovedHistoryRow],
    offset: usize,
    height: usize,
    width: usize,
) -> usize {
    history_lines(rows, offset, usize::MAX, height, width).1
}

/// `── today ──────`: the list's quiet section rule.
fn day_rule(label: &str, width: usize) -> Line<'static> {
    let inner = width.saturating_sub(2);
    let label = truncate_end(&format!(" {label} "), inner.saturating_sub(4));
    let trail = inner.saturating_sub(2 + label.width());
    Line::from(vec![
        Span::raw(" "),
        Span::styled("──", theme::fg(theme::BORDER_DIM)),
        Span::styled(label, theme::dim()),
        Span::styled("─".repeat(trail), theme::fg(theme::BORDER_DIM)),
    ])
}

fn history_row(row: &crate::RemovedHistoryRow, selected: bool, width: usize) -> Line<'static> {
    let background = selected.then_some(theme::SELECTED_BG);
    let style = |color: Color| {
        let style = theme::fg(color);
        background.map_or(style, |bg| style.bg(bg))
    };
    let mut right: Vec<(String, Color)> = Vec::new();
    if row.automations_paused {
        right.push((format!("{} ", glyphs::PAUSE), theme::WARN));
    }
    if let Some(issue) = row.issue_status.as_deref().map(issue_badge) {
        right.push((format!("{} ", issue.glyph), issue.color));
    }
    if let Some(pr) = pr_badge(row) {
        right.push((format!("{} ", pr.glyph), pr.color));
    }
    let age = row.age.clone().unwrap_or_default();
    let right_width = right.iter().map(|(text, _)| text.width()).sum::<usize>()
        + if right.is_empty() { 0 } else { 1 }
        + 4;
    let remote = row.host.is_some();
    // One cell of padding each side and the three-cell marker slot.
    let budget = width.saturating_sub(2 + 3 + if remote { 2 } else { 0 } + right_width);
    let label = truncate_end(&row.title, budget);
    let filler = budget.saturating_sub(label.width());
    let marker = status_badge(row);
    let mut spans = vec![
        Span::styled(" ", style(theme::FG)),
        Span::styled(format!("{}  ", marker.glyph), style(marker.color)),
    ];
    if remote {
        spans.push(Span::styled(
            format!("{} ", glyphs::REMOTE),
            style(theme::INFO),
        ));
    }
    let mut title = style(if selected {
        theme::FG_BRIGHT
    } else {
        theme::FG
    });
    if selected {
        title = title.add_modifier(Modifier::BOLD);
    }
    spans.push(Span::styled(label, title));
    spans.push(Span::styled(" ".repeat(filler), style(theme::FG)));
    if !right.is_empty() {
        spans.push(Span::styled(" ", style(theme::FG)));
    }
    for (text, color) in right {
        spans.push(Span::styled(text, style(color)));
    }
    spans.push(Span::styled(format!("{age:>4}"), style(theme::FG_DIM)));
    spans.push(Span::styled(" ", style(theme::FG)));
    Line::from(spans)
}

fn history_lines(
    rows: &[crate::RemovedHistoryRow],
    offset: usize,
    selected: usize,
    height: usize,
    width: usize,
) -> (Vec<Line<'static>>, usize) {
    let mut lines = Vec::new();
    let mut row_count = 0;
    let mut previous_day = offset
        .checked_sub(1)
        .and_then(|previous| rows.get(previous))
        .and_then(|row| row.day_label.as_deref());
    for (index, row) in rows.iter().enumerate().skip(offset) {
        if lines.len() >= height {
            break;
        }
        let day = row.day_label.as_deref();
        if let Some(day) = day
            && Some(day) != previous_day
        {
            // A blank row separates day groups, as sections do in the list.
            let spaced = !lines.is_empty();
            if lines.len() + 1 + usize::from(spaced) >= height {
                break;
            }
            if spaced {
                lines.push(Line::default());
            }
            lines.push(day_rule(day, width));
        }
        lines.push(history_row(row, index == selected, width));
        row_count += 1;
        previous_day = day;
    }
    (lines, row_count)
}

/// The removal record: title, branch, when and where, then the captured
/// `Label: value` facts as an aligned key column with the work state in
/// its own color, free-form notes in mid gray, and the keys that apply.
fn record_lines(row: &crate::RemovedHistoryRow) -> Vec<Line<'static>> {
    const KEY: usize = 10;
    let label = |text: &str| Span::styled(format!("{text:<KEY$}"), theme::dim());
    let marker = status_badge(row);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!("{}  ", marker.glyph), theme::fg(marker.color)),
            Span::styled(row.title.clone(), theme::bold(theme::FG_BRIGHT)),
        ]),
        Line::from(vec![
            Span::raw("   "),
            Span::styled(row.branch.clone(), theme::fg(theme::ACCENT_ALT)),
        ]),
        Line::default(),
    ];
    let when = row.day_label.as_deref().unwrap_or(&row.removed_at);
    let mut removed = vec![
        label("removed"),
        Span::styled(when.to_owned(), theme::fg(theme::FG)),
    ];
    if let Some(age) = &row.age {
        removed.push(Span::styled(format!(" · {age} ago"), theme::dim()));
    }
    lines.push(Line::from(removed));
    if let Some(host) = &row.host {
        lines.push(Line::from(vec![
            label("host"),
            Span::styled(format!("{} ", glyphs::REMOTE), theme::fg(theme::INFO)),
            Span::styled(host.clone(), theme::fg(theme::FG)),
        ]));
    }
    if row.slug != row.title {
        lines.push(Line::from(vec![
            label("slug"),
            Span::styled(row.slug.clone(), theme::fg(theme::FG)),
        ]));
    }
    if row.automations_paused {
        lines.push(Line::from(vec![
            label("automate"),
            Span::styled(format!("{} paused", glyphs::PAUSE), theme::fg(theme::WARN)),
        ]));
    }
    for detail in &row.details {
        let parsed = detail
            .split_once(": ")
            .filter(|(key, _)| key.len() <= 32 && !key.contains('.'));
        let line = match parsed {
            Some(("Work", state)) => {
                let color = work_state(state).map_or(theme::FG, badges::work_state_color);
                let glyph = work_state(state).map_or(glyphs::DOT_OUTLINE, badges::work_state_glyph);
                Line::from(vec![
                    label("work"),
                    Span::styled(format!("{glyph}  {state}"), theme::fg(color)),
                ])
            }
            Some(("Verification is still owed", steps)) => Line::from(vec![
                label("verify"),
                Span::styled(format!("still owed: {steps}"), theme::fg(theme::WARN)),
            ]),
            Some(("Blocked on", what)) => Line::from(vec![
                label("blocked"),
                Span::styled(what.to_owned(), theme::fg(theme::WARN)),
            ]),
            Some((key, value)) => {
                let key = key.to_lowercase();
                let key = key
                    .strip_prefix("verify after merge")
                    .map_or(key.as_str(), |_| "verify");
                Line::from(vec![
                    label(key),
                    Span::styled(value.to_owned(), theme::fg(theme::FG)),
                ])
            }
            None if detail.starts_with("Landed on") => Line::from(vec![
                label("landed"),
                Span::styled(
                    detail
                        .trim_start_matches("Landed on ")
                        .trim_end_matches(" when removed")
                        .to_owned(),
                    theme::fg(theme::OK),
                ),
            ]),
            None => Line::from(vec![
                Span::raw(" ".repeat(KEY)),
                Span::styled(detail.clone(), theme::fg(theme::FG_MID)),
            ]),
        };
        lines.push(line);
    }
    lines.push(Line::default());
    let mut keys = vec![("⏎", "restore")];
    if row.pr_url.is_some() {
        keys.push(("p", "PR"));
    }
    if row.issue_url.is_some() {
        keys.push(("i", "issue"));
    }
    keys.extend([("y", "copy branch"), ("h", "back")]);
    let mut spans = Vec::new();
    for (index, (key, action)) in keys.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        spans.push(Span::styled(key, theme::bold(theme::ACCENT)));
        spans.push(Span::styled(format!(" {action}"), theme::dim()));
    }
    lines.push(Line::from(spans));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Board, RemovedHistoryRow};
    use std::sync::Arc;

    #[test]
    fn removed_rows_use_list_glyphs_day_rules_and_right_aligned_age() {
        let row = |slug: &str, day: &str, details: Vec<&str>| RemovedHistoryRow {
            key: slug.into(),
            slug: slug.into(),
            title: format!("{slug} title"),
            branch: format!("feature/{slug}"),
            day_label: Some(day.into()),
            age: Some("2d".into()),
            details: details.into_iter().map(Into::into).collect(),
            ..Default::default()
        };
        let mut landed = row("landed", "today", vec!["Work: ready"]);
        landed.landed_on = Some(LandingKind::Base);
        let rows = vec![
            landed,
            row("closed", "today", vec!["PR: CLOSED"]),
            row("plain", "yesterday", vec![]),
        ];
        let (lines, count) = history_lines(&rows, 0, 1, 20, 40);
        assert_eq!(count, 3);
        let text = |line: &Line<'_>| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        assert!(text(&lines[0]).contains("── today ─"));
        assert_eq!(lines[1].spans[1].content, format!("{}  ", glyphs::MERGE));
        assert_eq!(lines[1].spans[1].style.fg, Some(theme::OK));
        assert_eq!(lines[2].spans[1].style.fg, Some(theme::ERR));
        assert_eq!(lines[2].spans[0].style.bg, Some(theme::SELECTED_BG));
        assert!(text(&lines[1]).ends_with("  2d "));
        assert_eq!(text(&lines[1]).width(), 40);
        assert!(lines[3].spans.is_empty(), "blank row between days");
        assert!(text(&lines[4]).contains("yesterday"));
        assert_eq!(lines[5].spans[1].content, format!("{}  ", glyphs::TRASH));

        let record = record_lines(&rows[0]);
        let work = record
            .iter()
            .find(|line| text(line).contains("ready"))
            .unwrap();
        assert_eq!(work.spans[1].style.fg, Some(theme::OK));
    }

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
