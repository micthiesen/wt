//! Frame composition: title bar, list, details, activity, footer, overlays.
//! Rendering reads only the prepared snapshot and model state.

mod details;
mod list;
mod overlays;
pub(crate) mod text;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use unicode_width::UnicodeWidthStr;
use wt_runtime::SourceState;

use crate::{Interaction, Model, badges, glyphs, theme};
use text::truncate_end;

/// The details pane never grows past this many rows; the activity pane takes
/// the rest of the right column.
const DETAILS_MAX: u16 = 20;
const ACTIVITY_MIN: u16 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Panes {
    pub header: Rect,
    pub list: Rect,
    pub details: Rect,
    pub activity: Rect,
    pub footer: Rect,
}

/// List width tracks the terminal but stays within 32..=52 cells, so the
/// details pane gets the space on wide terminals. Below 60 columns the panes
/// stack vertically instead of squeezing both.
pub(crate) fn panes(area: Rect, full_width_activity: bool) -> Panes {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);
    if area.width < 60 {
        let [list, right] =
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
        let [details, activity] =
            Layout::vertical([Constraint::Percentage(65), Constraint::Percentage(35)]).areas(right);
        return Panes {
            header,
            list,
            details,
            activity,
            footer,
        };
    }
    let list_width = ((u32::from(area.width) * 44 / 100) as u16).clamp(32, 52);
    let activity_height = body.height.saturating_sub(DETAILS_MAX).max(ACTIVITY_MIN);
    if full_width_activity {
        let [top, activity] = Layout::vertical([
            Constraint::Length(body.height.saturating_sub(activity_height)),
            Constraint::Min(0),
        ])
        .areas(body);
        let [list, details] =
            Layout::horizontal([Constraint::Length(list_width), Constraint::Min(0)]).areas(top);
        return Panes {
            header,
            list,
            details,
            activity,
            footer,
        };
    }
    let [list, right] =
        Layout::horizontal([Constraint::Length(list_width), Constraint::Min(0)]).areas(body);
    let [details, activity] = Layout::vertical([
        Constraint::Length(right.height.saturating_sub(activity_height)),
        Constraint::Min(0),
    ])
    .areas(right);
    Panes {
        header,
        list,
        details,
        activity,
        footer,
    }
}

pub(crate) fn render(frame: &mut Frame<'_>, model: &mut Model) {
    let area = frame.area();
    frame.render_widget(
        Block::new().style(Style::new().bg(theme::BG).fg(theme::FG)),
        area,
    );
    if area.width < 20 || area.height < 5 {
        frame.render_widget(
            Paragraph::new("Resize terminal to view wt").style(theme::dim()),
            area,
        );
        return;
    }
    let panes = panes(area, model.board.full_width_activity);
    header(frame, model, panes.header);
    if model.history.active {
        crate::history::render(frame, model, panes.list, panes.details);
    } else {
        list::render(frame, model, panes.list);
        details::render(frame, model, panes.details);
    }
    let available = panes.activity.height.saturating_sub(2) as usize;
    let available_width = panes.activity.width.saturating_sub(2) as usize;
    let (title, activity_lines) = model.output_view(available, available_width);
    frame.render_widget(
        Paragraph::new(activity_lines).block(panel(&title)),
        panes.activity,
    );
    footer(frame, model, panes.footer);
    overlays::render(frame, model, area);
}

/// One row: counts and load state on the left; automation state, usage, and
/// the primary harness on the right.
fn header(frame: &mut Frame<'_>, model: &Model, area: Rect) {
    let board = &model.board;
    let base = Style::new().bg(theme::BG_ALT);
    frame.render_widget(Block::new().style(base), area);
    let archived = board.rows.iter().filter(|row| row.archived).count();
    let active = board.rows.len() - archived;
    let mut title = format!(
        " wt · {active} worktree{}",
        if active == 1 { "" } else { "s" }
    );
    if archived > 0 {
        title.push_str(&format!(" · {archived} archived"));
    }
    let mut left = vec![
        Span::raw(" "),
        Span::styled(
            title,
            base.fg(theme::FG_BRIGHT).add_modifier(Modifier::BOLD),
        ),
    ];
    match &model.source_state {
        SourceState::Empty => left.push(Span::styled(" · loading…", base.fg(theme::FG_DIM))),
        SourceState::Refreshing => left.push(Span::styled(" ↻", base.fg(theme::FG_DIM))),
        SourceState::Failed(_) => left.push(Span::styled(" · refresh failed", base.fg(theme::ERR))),
        SourceState::Ready => {}
    }

    let mut right: Vec<Span<'static>> = Vec::new();
    if board.automations_paused {
        // An inverse chip, not plain warn text: while paused the automated
        // half of the escalation ladder is inert, which must not blend into
        // the telemetry beside it.
        right.push(Span::styled(
            format!(" auto {} ", glyphs::PAUSE),
            Style::new()
                .fg(theme::BG)
                .bg(theme::WARN)
                .add_modifier(Modifier::BOLD),
        ));
        right.push(Span::styled("  ", base));
    } else if board.automations_pending > 0 {
        right.push(Span::styled(
            format!("auto {} queued  ", board.automations_pending),
            base.fg(theme::FG_DIM),
        ));
    }
    let usage = usage_spans(model, base);
    if !usage.is_empty() {
        right.extend(usage);
        right.push(Span::styled(" · ", base.fg(theme::FG_DIM)));
    }
    let harness = model.primary_harness();
    right.push(Span::styled(
        format!("{} ", badges::harness_glyph(harness)),
        base.fg(badges::harness_color(harness)),
    ));
    right.push(Span::styled(" ", base));
    draw_split(frame, area, left, right);
}

/// The primary harness's rate-limit windows, each colored by how close it is
/// to the limit, with the time until it resets. Codex reports headroom, so
/// its figure is the percentage left.
fn usage_spans(model: &Model, base: Style) -> Vec<Span<'static>> {
    let primary = model.primary_harness();
    let now = text::now_ms();
    let codex = primary.eq_ignore_ascii_case("codex");
    let mut spans = Vec::new();
    for item in model
        .board
        .usage
        .iter()
        .filter(|item| item.harness.eq_ignore_ascii_case(primary))
    {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", base.fg(theme::FG_DIM)));
        }
        if let Some(cost) = &item.cost {
            spans.push(Span::styled(
                format!("{} {cost}", item.period),
                base.fg(theme::FG),
            ));
            continue;
        }
        let Some(mut percent) = item.percent else {
            continue;
        };
        let expired = item.resets_at_ms.is_some_and(|reset| reset <= now);
        if expired {
            percent = 0;
        }
        let used = percent.min(100);
        let color = match used {
            80.. => theme::ERR,
            60..80 => theme::WARN,
            _ => theme::FG,
        };
        let shown = if codex { 100 - used } else { used };
        spans.push(Span::styled(
            format!(
                "{} {shown}%{}",
                item.period,
                if codex { " left" } else { "" }
            ),
            base.fg(color),
        ));
        if let Some(reset) = item.resets_at_ms.filter(|_| !expired) {
            spans.push(Span::styled(
                format!(" ({})", text::age(now, reset)),
                base.fg(theme::FG_DIM),
            ));
        }
    }
    spans
}

/// The legend (or a toast, or an active text prompt) on the left; on the
/// right, the special-session slots tinted by their live state.
fn footer(frame: &mut Frame<'_>, model: &Model, area: Rect) {
    let base = Style::new().bg(theme::BG_ALT);
    frame.render_widget(Block::new().style(base), area);
    let prompt = match (&model.interaction, &model.title_prompt) {
        (_, Some(title)) => Some(("title:".to_owned(), &title.editor)),
        (Interaction::Text(prompt), None) => {
            Some((prompt.prompt.trim().to_owned(), &prompt.editor))
        }
        _ => None,
    };
    if let Some((label, editor)) = prompt {
        let hint = if area.width >= 80 {
            " (⏎ submit, esc cancel)"
        } else {
            ""
        };
        let label = truncate_end(&label, (area.width as usize * 45 / 100).max(4));
        let label_width = (label.width() + 2) as u16;
        let room = area
            .width
            .saturating_sub(label_width + hint.width() as u16 + 1) as usize;
        let (text, cursor) = editor.viewport(room);
        let spans = vec![
            Span::raw(" "),
            Span::styled(label, base.fg(theme::ACCENT).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(format!("{text:<room$}"), base.fg(theme::FG_BRIGHT)),
            Span::styled(hint, base.fg(theme::FG_DIM)),
        ];
        frame.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
        if room > 0 {
            frame.set_cursor_position((area.x + label_width + cursor, area.y));
        }
        return;
    }
    // A reply that only opens a modal carries an empty message; it must
    // not blank the legend while the modal is up.
    let toast = model
        .toast
        .as_ref()
        .filter(|(message, _)| !message.trim().is_empty());
    let left = if let Some((message, failed)) = toast {
        // Leave the slot buttons their cells; a long toast truncates.
        let room = (area.width as usize).saturating_sub(slot_buttons_width(model) + 4);
        if *failed {
            vec![
                Span::raw(" "),
                Span::styled(format!("{} ", glyphs::CHECK_FAIL), base.fg(theme::ERR)),
                Span::styled(
                    truncate_end(message, room.saturating_sub(2)),
                    base.fg(theme::ERR),
                ),
            ]
        } else {
            vec![
                Span::raw(" "),
                Span::styled(truncate_end(message, room), base.fg(theme::FG)),
            ]
        }
    } else if let SourceState::Failed(error) = &model.source_state {
        vec![
            Span::raw(" "),
            Span::styled(error.to_string(), base.fg(theme::ERR)),
        ]
    } else {
        let mut spans = vec![
            Span::raw(" "),
            Span::styled("?", base.fg(theme::ACCENT)),
            Span::styled(" help", base.fg(theme::FG_DIM)),
        ];
        if area.width >= 80 {
            spans.extend([
                Span::styled(" · ", base.fg(theme::FG_DIM)),
                Span::styled("t", base.fg(theme::ACCENT)),
                Span::styled(" title", base.fg(theme::FG_DIM)),
            ]);
        }
        spans
    };
    draw_split(frame, area, left, slot_buttons(model, base));
}

const SLOTS: [(&str, char); 4] = [
    ("manager", 'm'),
    ("main", '.'),
    ("wt", ','),
    ("dotfiles", '/'),
];

fn slot_buttons_width(model: &Model) -> usize {
    slot_buttons(model, Style::new())
        .iter()
        .map(|span| span.content.width())
        .sum()
}

fn slot_buttons(model: &Model, base: Style) -> Vec<Span<'static>> {
    let board = &model.board;
    let primary = model.primary_harness();
    let mut spans = Vec::new();
    if let Some(percent) = board
        .slot_sessions
        .get("manager")
        .and_then(|sessions| sessions.iter().find(|session| session.live))
        .and_then(|session| session.context_percent)
    {
        let color = match percent {
            85.. => theme::ERR,
            70..85 => theme::WARN,
            _ => theme::FG_DIM,
        };
        spans.push(Span::styled(format!("{percent}% "), base.fg(color)));
    }
    for (index, (slot, key)) in SLOTS.into_iter().enumerate() {
        let sessions = board.slot_sessions.get(slot);
        if slot == "dotfiles" && sessions.is_none() {
            continue;
        }
        let state = sessions
            .and_then(|sessions| sessions.iter().find(|session| session.live))
            .map(|session| session.state.as_str());
        if index > 0 {
            spans.push(Span::styled("  ", base));
        }
        spans.push(Span::styled("[", base.fg(theme::FG_DIM)));
        spans.push(Span::styled(
            key.to_string(),
            base.fg(state.map_or(theme::FG_DIM, |state| {
                badges::session_state_color(primary, state)
            })),
        ));
        spans.push(Span::styled("]", base.fg(theme::FG_DIM)));
    }
    spans.push(Span::styled(" ", base));
    spans
}

/// Draw `left` truncated so `right` always keeps its cells.
fn draw_split(
    frame: &mut Frame<'_>,
    area: Rect,
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
) {
    let right_width: u16 = right
        .iter()
        .map(|span| span.content.width() as u16)
        .sum::<u16>()
        .min(area.width);
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_width)]).areas(area);
    frame.render_widget(Paragraph::new(Line::from(left)), left_area);
    frame.render_widget(Paragraph::new(Line::from(right)), right_area);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MousePane {
    List,
    Details,
    Output,
    Other,
}

pub(crate) fn mouse_pane(area: Rect, full_width_activity: bool, x: u16, y: u16) -> MousePane {
    let panes = panes(area, full_width_activity);
    let contains = |rect: Rect| {
        x >= rect.x
            && x < rect.x.saturating_add(rect.width)
            && y >= rect.y
            && y < rect.y.saturating_add(rect.height)
    };
    if contains(panes.list) {
        MousePane::List
    } else if contains(panes.details) {
        MousePane::Details
    } else if contains(panes.activity) {
        MousePane::Output
    } else {
        MousePane::Other
    }
}

/// A bordered pane with a lowercase title set into the top rule.
pub(crate) fn panel(title: &str) -> Block<'static> {
    panel_owned(title.to_owned())
}

pub(crate) fn panel_owned(title: String) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(theme::fg(theme::BORDER))
        .title(Line::from(vec![
            Span::styled("─ ", theme::fg(theme::BORDER)),
            Span::styled(title, theme::fg(theme::FG_DIM)),
            Span::styled(" ", theme::fg(theme::BORDER)),
        ]))
}

pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ConfirmPrompt;
    use crate::{Board, BoardRow, ConfirmAction, Interaction, PickerAction, PickerOption};
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::Arc;

    #[test]
    fn footer_slots_take_the_live_session_color_and_manager_context() {
        let model = Model {
            board: std::sync::Arc::new(Board {
                slot_sessions: [(
                    "manager".into(),
                    vec![crate::SessionView {
                        live: true,
                        harness: "Claude".into(),
                        state: "asking".into(),
                        context_percent: Some(87),
                        ..Default::default()
                    }],
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let spans = slot_buttons(&model, Style::new());
        let text: String = spans.iter().map(|span| span.content.to_string()).collect();
        assert_eq!(text, "87% [m]  [.]  [,] ");
        assert_eq!(spans[0].style.fg, Some(theme::ERR));
        assert_eq!(spans[2].style.fg, Some(theme::INFO));
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
                        detail: None,
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
                assert!(text.contains("1 worktree"));
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
        let mut terminal = Terminal::new(TestBackend::new(90, 16)).unwrap();
        terminal.draw(|frame| render(frame, &mut model)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains(&format!(
            "{}  A deliberately narrow title",
            glyphs::DOT_OUTLINE
        )));
        let details = details::work_status_lines(&model.board.rows[0], false)
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
        let section = crate::BoardSection {
            rollup,
            ..Default::default()
        };
        let details = details::section_rollup_lines(&section)
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
                detail: None,
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

    #[test]
    fn an_empty_toast_keeps_the_help_legend_under_a_modal() {
        let mut model = Model {
            toast: Some((String::new(), false)),
            interaction: Interaction::Picker(crate::model::PickerPrompt {
                action: PickerAction::Status { key: "one".into() },
                title: "status".into(),
                options: Vec::new(),
                selected: 0,
            }),
            ..Model::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| render(frame, &mut model)).unwrap();
        let buffer = terminal.backend().buffer();
        let footer = (0..100)
            .map(|x| buffer[(x, 29)].symbol())
            .collect::<String>();
        assert!(footer.contains("? help · t title"), "{footer}");
    }
}
