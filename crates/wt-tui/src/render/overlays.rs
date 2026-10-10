//! Modal overlays: help, copy, performance, pickers, confirmations, logs,
//! and reviewers.
//!
//! Every modal shares one frame: a rounded border in the accent color (warn
//! for confirmations that discard or stop something) on the raised surface,
//! a bright title set into the top rule, and key hints along the bottom
//! edge. Lists mark the cursor with `›` on the selection background, and a
//! scroll thumb rides the right border when content overflows.

use ratatui::{
    Frame,
    layout::{Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
};
use unicode_width::UnicodeWidthStr;
use wt_core::WorkState;

use super::{
    centered,
    text::{fit, truncate_end, wrap},
};
use crate::{
    BoardRow, BusyView, CheckState, ConfirmAction, DisplayPolicy, GitPresentation, Interaction,
    LandingKind, Model, PickerAction, PrPresentation, ReviewBotView, ReviewState, SessionMode,
    WorkPresentation, badges, glyphs,
    model::{ConfirmPrompt, PickerPrompt, ReviewerPrompt},
    theme,
};

/// Widest a modal frame grows (border included). Past this a dialog reads
/// as disconnected fragments rather than one centered surface.
const MAX_FRAME_WIDTH: u16 = 104;
/// Below this terminal width modals take the full width.
const NARROW_WIDTH: u16 = 80;
/// Content width an action palette asks for, which places a ~98-cell frame
/// on wide terminals like the TS palette.
const ACTION_PICKER_WIDTH: usize = 94;
/// Widest the help keymap's key column grows; longer keys are cut.
const HELP_KEY_MAX: usize = 22;

pub(crate) fn render(frame: &mut Frame<'_>, model: &mut Model, area: Rect) {
    if area.width < 10 || area.height < 4 {
        return;
    }
    if let Some(selected) = model.yank {
        yank(frame, model, area, selected);
    }
    if model.show_perf {
        perf(frame, model, area);
    }
    if model.help {
        help(frame, model, area);
    }
    let primary = model.primary_harness().to_owned();
    match &mut model.interaction {
        Interaction::Reviewers(picker) => reviewers(frame, area, picker),
        Interaction::Log {
            title,
            lines,
            scroll,
        } => log(frame, area, title, lines, scroll),
        Interaction::Confirm(confirm) => confirmation(frame, area, confirm),
        Interaction::Picker(picker) => self::picker(frame, area, picker, &primary),
        Interaction::Text(_) | Interaction::None => {}
    }
}

// ── Frame ───────────────────────────────────────────────────────────────

type Hint = (String, String);

fn hint(key: impl Into<String>, label: impl Into<String>) -> Hint {
    (key.into(), label.into())
}

struct Modal {
    title: String,
    border: Color,
    hints: Vec<Hint>,
}

impl Modal {
    fn new(title: impl Into<String>, hints: Vec<Hint>) -> Self {
        Self {
            title: title.into(),
            border: theme::ACCENT,
            hints,
        }
    }

    fn border(mut self, color: Color) -> Self {
        self.border = color;
        self
    }

    /// At short heights secondary hints can eat the frame; keep movement,
    /// the primary action, and dismissal.
    fn visible_hints(&self, area: Rect) -> Vec<&Hint> {
        let last = self.hints.len().saturating_sub(1);
        self.hints
            .iter()
            .enumerate()
            .filter(|(index, (key, label))| {
                area.height >= 14
                    || *index == 0
                    || *index == last
                    || key.contains('⏎')
                    || label == "cancel"
            })
            .map(|(_, hint)| hint)
            .collect()
    }

    fn top_padding(area: Rect) -> u16 {
        u16::from(area.height >= 18)
    }

    /// The frame for `body_rows` of content that would like
    /// `content_width` cells. `fill` takes the full available height.
    fn place(&self, area: Rect, content_width: usize, body_rows: usize, fill: bool) -> Rect {
        let max_width = if area.width < NARROW_WIDTH {
            area.width
        } else {
            area.width.saturating_sub(4).min(MAX_FRAME_WIDTH)
        };
        let min_width = 40.min(max_width);
        // Prefer a frame wide enough to keep the hints on one row.
        let hints = self.visible_hints(area);
        let hint_width = hints
            .iter()
            .map(|(key, label)| key.width() + 1 + label.width())
            .sum::<usize>()
            + 3 * hints.len().saturating_sub(1);
        let width = (content_width
            .max(hint_width)
            .saturating_add(4)
            .min(usize::from(u16::MAX)) as u16)
            .clamp(min_width, max_width);
        let hint_rows = hint_lines(&self.visible_hints(area), width.saturating_sub(4)).len();
        let chrome = 2 + Self::top_padding(area) as usize + hint_rows + usize::from(hint_rows > 0);
        let max_height = if area.height >= 24 {
            area.height - 2
        } else {
            area.height
        };
        let height = if fill {
            max_height
        } else {
            ((body_rows + chrome).min(usize::from(max_height)) as u16).max(3.min(max_height))
        };
        centered(area, width, height)
    }

    /// Paint the frame and hints; returns the body rectangle.
    fn draw(&self, frame: &mut Frame<'_>, area: Rect, outer: Rect) -> Rect {
        // A one-cell gutter of board background on each side, so text
        // behind the modal never sits flush against its border.
        let gutter = Rect {
            x: outer.x.saturating_sub(1).max(area.x),
            width: (outer.width + 2).min(area.width),
            ..outer
        };
        frame.render_widget(Clear, gutter);
        frame.render_widget(Block::new().style(Style::new().bg(theme::BG)), gutter);
        frame.render_widget(Clear, outer);
        let surface = Style::new().bg(theme::BG_ALT).fg(theme::FG);
        let title = truncate_end(&self.title, outer.width.saturating_sub(6) as usize);
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(self.border).bg(theme::BG_ALT))
            .style(surface)
            .padding(Padding::new(1, 1, Self::top_padding(area), 0))
            .title(Line::from(vec![
                Span::styled(" ", surface),
                Span::styled(
                    title,
                    surface.fg(theme::FG_BRIGHT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ", surface),
            ]));
        let inner = block.inner(outer);
        frame.render_widget(block, outer);
        let hints = hint_lines(&self.visible_hints(area), inner.width);
        let rows = hints.len() as u16;
        if inner.height <= rows {
            return inner;
        }
        let gap = u16::from(inner.height > rows + 1);
        let hint_area = Rect {
            y: inner.y + inner.height - rows,
            height: rows,
            ..inner
        };
        frame.render_widget(Paragraph::new(hints), hint_area);
        Rect {
            height: inner.height - rows - gap,
            ..inner
        }
    }
}

/// Hint chips (`key label`) joined by ` · `, wrapping between chips so a
/// hint never splits mid-pair or overruns the border.
fn hint_lines(hints: &[&Hint], width: u16) -> Vec<Line<'static>> {
    let width = width as usize;
    let mut lines = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (key, label) in hints {
        let chip = key.width() + 1 + label.width();
        let separator = if spans.is_empty() { 0 } else { 3 };
        if !spans.is_empty() && used + separator + chip > width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            used = 0;
        }
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", theme::dim()));
            used += 3;
        }
        spans.push(Span::styled(key.clone(), theme::bold(theme::ACCENT)));
        spans.push(Span::styled(format!(" {label}"), theme::dim()));
        used += chip;
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

/// A thumb on the modal's right border while content overflows.
fn scrollbar(frame: &mut Frame<'_>, outer: Rect, border: Color, total: usize, offset: usize) {
    let visible = outer.height.saturating_sub(2) as usize;
    if total <= visible {
        return;
    }
    let mut state = ScrollbarState::new(total.saturating_sub(visible)).position(offset);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(Style::new().fg(border).bg(theme::BG_ALT))
            .thumb_symbol("┃")
            .thumb_style(Style::new().fg(theme::FG_BRIGHT).bg(theme::BG_ALT)),
        outer.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut state,
    );
}

/// Fit spans into exactly `width` cells: the overflowing span is cut with
/// an ellipsis and the rest of the row is padded.
fn fit_spans(spans: Vec<Span<'static>>, width: usize, pad: Style) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len() + 1);
    let mut used = 0;
    for span in spans {
        let cells = span.content.width();
        if used + cells <= width {
            used += cells;
            out.push(span);
            continue;
        }
        let room = width - used;
        if room > 0 {
            out.push(Span::styled(truncate_end(&span.content, room), span.style));
            used = width;
        }
        break;
    }
    if used < width {
        out.push(Span::styled(" ".repeat(width - used), pad));
    }
    out
}

/// A list row: cursor marker, content, and the selection background.
fn list_row(spans: Vec<Span<'static>>, selected: bool, width: usize) -> Line<'static> {
    let mut all = vec![if selected {
        Span::styled("› ", theme::bold(theme::ACCENT))
    } else {
        Span::raw("  ")
    }];
    all.extend(spans);
    let mut spans = fit_spans(all, width, Style::new());
    if selected {
        for span in &mut spans {
            span.style = span.style.bg(theme::SELECTED_BG);
        }
    }
    Line::from(spans)
}

/// Keep the cursor in view, centered when the list scrolls.
fn list_offset(selected: usize, total: usize, visible: usize) -> usize {
    selected
        .saturating_sub(visible / 2)
        .min(total.saturating_sub(visible))
}

fn label_style(selected: bool) -> Style {
    if selected {
        theme::bold(theme::FG_BRIGHT)
    } else {
        theme::fg(theme::FG)
    }
}

// ── Pickers ─────────────────────────────────────────────────────────────

enum PickRow {
    Group(String),
    Spacer,
    Option(usize),
}

fn picker(frame: &mut Frame<'_>, area: Rect, picker: &PickerPrompt, primary: &str) {
    let actions = matches!(picker.action, PickerAction::Actions { .. });
    let chords = picker.options.iter().any(|option| option.chord.is_some());
    let digit_rows = crate::model::digit_rows(picker);
    let digits = !chords && !digit_rows.is_empty() && picker.options.len() <= 9;
    let glyphs = matches!(
        picker.action,
        PickerAction::Status { .. } | PickerAction::Sessions { .. } | PickerAction::Harness { .. }
    );

    // Action palettes group their entries as `group: name`; the group
    // becomes a header and the entry keeps only its name.
    let mut rows = Vec::with_capacity(picker.options.len());
    let mut labels = Vec::with_capacity(picker.options.len());
    let mut current: Option<&str> = None;
    for (index, option) in picker.options.iter().enumerate() {
        let (group, label) = match option.label.split_once(": ").filter(|_| actions) {
            Some((group, label)) if !group.is_empty() => (Some(group), label),
            _ => (None, option.label.as_str()),
        };
        // One blank before every group change except at the top; entries
        // inside a group stay contiguous.
        if index == 0 || group != current {
            if index > 0 {
                rows.push(PickRow::Spacer);
            }
            if let Some(group) = group {
                rows.push(PickRow::Group(group.to_owned()));
            }
            current = group;
        }
        rows.push(PickRow::Option(index));
        labels.push(label);
    }

    let mut title = picker.title.as_str();
    let mut extra = Vec::new();
    match &picker.action {
        PickerAction::Status { .. } => extra.push(hint("m", "note")),
        PickerAction::Section { .. } => {
            title = title.strip_suffix(" · n new").unwrap_or(title);
            extra.push(hint("n", "new section"));
        }
        PickerAction::Sessions { choices }
            if choices
                .iter()
                .any(|choice| choice.mode == SessionMode::Resume) =>
        {
            extra.extend([
                hint("c / x / o", "new …"),
                hint("d", "close"),
                hint("x", "kill"),
            ]);
        }
        _ => {}
    }
    // The key that opened this picker re-presses to confirm (see the
    // model's opener table), so the hint names the surface's own key.
    let opener = match &picker.action {
        PickerAction::Actions { surface } => Some(match surface {
            crate::ActionSurface::Row { .. } => "!",
            crate::ActionSurface::Manager { .. } => "M",
            crate::ActionSurface::Slot { target } => match target {
                crate::SessionTarget::WtSource => "<",
                crate::SessionTarget::Main => ">",
                _ => "\\",
            },
        }),
        PickerAction::Harness { .. } => Some("F12"),
        PickerAction::Status { .. } => Some("u"),
        PickerAction::Base { .. } => Some("b"),
        PickerAction::Section { .. } => Some("l"),
        PickerAction::Output { .. } => Some("'"),
        PickerAction::Sessions { .. } => Some(";"),
        _ => None,
    };
    let mut hints = vec![hint("j/k", "move")];
    if chords {
        hints.push(hint("letter", "quick pick"));
    } else if digits {
        hints.push(hint("1-9", "quick pick"));
    }
    hints.push(hint(
        opener.map_or("⏎".to_owned(), |key| format!("{key} / ⏎")),
        "pick",
    ));
    hints.extend(extra);
    hints.push(hint("esc / q", "cancel"));
    let modal = Modal::new(title, hints);

    let prefix = 2 + if chords || digits { 2 } else { 0 } + if glyphs { 3 } else { 0 };
    let widest = labels
        .iter()
        .zip(&picker.options)
        .map(|(label, option)| {
            label.width()
                + option
                    .detail
                    .as_deref()
                    .map_or(0, |detail| detail.width() + 3)
        })
        .max()
        .unwrap_or(0);
    // Action palettes take the wide frame (TS ~98 columns) so the
    // trailing detail column is legible rather than cut to a stub.
    let wanted = if actions {
        (prefix + widest + 2).max(ACTION_PICKER_WIDTH)
    } else {
        prefix + widest + 2
    };
    let outer = modal.place(area, wanted, rows.len(), false);
    let body = modal.draw(frame, area, outer);
    let width = body.width as usize;
    let visible = body.height as usize;
    if visible == 0 || width == 0 {
        return;
    }
    if picker.options.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled("nothing to pick", theme::dim())),
            body,
        );
        return;
    }
    let cursor = rows
        .iter()
        .position(|row| matches!(row, PickRow::Option(index) if *index == picker.selected))
        .unwrap_or(0);
    // Keep the group header above the first entry visible at the top.
    let offset = list_offset(cursor, rows.len(), visible);
    let lines = rows
        .iter()
        .skip(offset)
        .take(visible)
        .map(|row| match row {
            PickRow::Group(group) => Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    truncate_end(&group.to_lowercase(), width.saturating_sub(2)),
                    theme::dim().add_modifier(Modifier::BOLD),
                ),
            ]),
            PickRow::Spacer => Line::default(),
            PickRow::Option(index) => {
                let option = &picker.options[*index];
                let selected = *index == picker.selected;
                let mut spans = Vec::new();
                let key_color = if selected {
                    theme::ACCENT
                } else {
                    theme::FG_DIM
                };
                if chords {
                    spans.push(Span::styled(
                        option
                            .chord
                            .map_or("  ".to_owned(), |chord| format!("{chord} ")),
                        theme::fg(key_color),
                    ));
                } else if digits {
                    spans.push(Span::styled(
                        digit_rows
                            .iter()
                            .position(|&row| row == *index)
                            .map_or("  ".to_owned(), |digit| format!("{} ", digit + 1)),
                        theme::fg(key_color),
                    ));
                }
                if glyphs {
                    let (glyph, color) = picker_glyph(&picker.action, option, primary);
                    spans.push(Span::styled(format!("{glyph}  "), theme::fg(color)));
                }
                let blocked = actions
                    && option
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.starts_with('('));
                let mut label = option_label(labels[*index], actions, selected);
                if blocked {
                    for span in &mut label {
                        span.style = if selected {
                            theme::fg(theme::FG_MID)
                        } else {
                            theme::dim()
                        };
                    }
                }
                if let Some(detail) = option.detail.as_deref() {
                    // Right-align the detail; the label gives way first.
                    let used = 2 + spans.iter().map(|span| span.content.width()).sum::<usize>();
                    let detail = truncate_end(detail, width.saturating_sub(used + 4) / 2);
                    let room = width.saturating_sub(used + detail.width() + 2);
                    label = fit_spans(label, room, Style::new());
                    let label_width = label.iter().map(|span| span.content.width()).sum::<usize>();
                    spans.extend(label);
                    spans.push(Span::raw(" ".repeat(room.saturating_sub(label_width) + 1)));
                    spans.push(Span::styled(
                        detail,
                        if blocked {
                            theme::dim().add_modifier(Modifier::ITALIC)
                        } else {
                            theme::dim()
                        },
                    ));
                } else {
                    spans.extend(label);
                }
                list_row(spans, selected, width)
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), body);
    scrollbar(frame, outer, modal.border, rows.len(), offset);
}

/// An action's availability reason trails its name in parentheses; the
/// entry is unavailable, so the whole row recedes. A session's `· live`
/// suffix reads as state, in the ok color.
fn option_label(label: &str, actions: bool, selected: bool) -> Vec<Span<'static>> {
    if actions
        && label.ends_with(')')
        && let Some(open) = label.rfind(" (")
    {
        return vec![
            Span::styled(
                label[..open].to_owned(),
                if selected {
                    theme::fg(theme::FG_MID)
                } else {
                    theme::dim()
                },
            ),
            Span::styled(
                label[open..].to_owned(),
                theme::dim().add_modifier(Modifier::ITALIC),
            ),
        ];
    }
    if let Some(name) = label.strip_suffix(" · live") {
        return vec![
            Span::styled(name.to_owned(), label_style(selected)),
            Span::styled(" · ", theme::dim()),
            Span::styled("live", theme::fg(theme::OK)),
        ];
    }
    vec![Span::styled(label.to_owned(), label_style(selected))]
}

/// The glyph column doubles as a legend: status pickers show the exact
/// dot the list draws; session pickers show the harness.
fn picker_glyph(
    action: &PickerAction,
    option: &crate::PickerOption,
    primary: &str,
) -> (&'static str, Color) {
    match action {
        PickerAction::Status { .. } => {
            match option.value.as_deref().and_then(crate::history::work_state) {
                // Ready with post-merge steps owes a check: warn, as TS.
                Some(state) if option.verify_after_merge.is_some() => {
                    (badges::work_state_glyph(state), theme::WARN)
                }
                Some(state) => (
                    badges::work_state_glyph(state),
                    badges::work_state_color(state),
                ),
                None => (glyphs::DOT_OUTLINE, theme::FG_DIM),
            }
        }
        PickerAction::Sessions { choices } => {
            let harness = option
                .value
                .as_deref()
                .and_then(|value| value.parse::<usize>().ok())
                .and_then(|index| choices.get(index))
                .map_or(primary, |choice| choice.harness.as_str());
            (
                badges::harness_glyph(harness),
                badges::harness_color(harness),
            )
        }
        PickerAction::Harness { .. } => {
            let harness = option.value.as_deref().unwrap_or(primary);
            (
                badges::harness_glyph(harness),
                badges::harness_color(harness),
            )
        }
        _ => (" ", theme::FG),
    }
}

fn reviewers(frame: &mut Frame<'_>, area: Rect, picker: &ReviewerPrompt) {
    let modal = Modal::new(
        format!("Reviewers · PR #{}", picker.pr_number),
        vec![
            hint("j/k", "move"),
            hint("space", "toggle"),
            hint("v / ⏎", "submit"),
            hint("esc / q", "cancel"),
        ],
    );
    let widest = picker
        .candidates
        .iter()
        .map(|option| option.label.width())
        .max()
        .unwrap_or(0);
    let outer = modal.place(area, widest + 8, picker.candidates.len().max(1), false);
    let body = modal.draw(frame, area, outer);
    let (width, visible) = (body.width as usize, body.height as usize);
    if visible == 0 || width == 0 {
        return;
    }
    if picker.candidates.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled("no candidates", theme::dim())),
            body,
        );
        return;
    }
    let offset = list_offset(picker.selected, picker.candidates.len(), visible);
    let lines = picker
        .candidates
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(index, option)| {
            let selected = index == picker.selected;
            list_row(
                vec![
                    if option.selected {
                        Span::styled("[x] ", theme::fg(theme::OK))
                    } else {
                        Span::styled("[ ] ", theme::dim())
                    },
                    Span::styled(option.label.clone(), label_style(selected)),
                ],
                selected,
                width,
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), body);
    scrollbar(frame, outer, modal.border, picker.candidates.len(), offset);
}

fn yank(frame: &mut Frame<'_>, model: &Model, area: Rect, selected: usize) {
    let choices = model.yank_choices();
    let modal = Modal::new(
        "yank · pick what to copy",
        vec![
            hint("j/k", "move"),
            hint("letter", "direct"),
            hint("1-9", "quick pick"),
            hint("y / ⏎", "copy"),
            hint("esc / q", "cancel"),
        ],
    );
    let label_width = choices
        .iter()
        .map(|(_, label, _)| label.width())
        .max()
        .unwrap_or(0)
        + 2;
    let widest = choices
        .iter()
        .map(|(_, _, value)| value.lines().next().unwrap_or("").width() + 12)
        .max()
        .unwrap_or(0);
    let outer = modal.place(area, 4 + label_width + widest, choices.len().max(1), false);
    let body = modal.draw(frame, area, outer);
    let (width, visible) = (body.width as usize, body.height as usize);
    if visible == 0 || width == 0 {
        return;
    }
    if choices.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled("nothing to copy", theme::dim())),
            body,
        );
        return;
    }
    let offset = list_offset(selected, choices.len(), visible);
    let lines = choices
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(index, (key, label, value))| {
            let is_selected = index == selected;
            let mut spans = vec![
                Span::styled(
                    format!("{key} "),
                    theme::fg(if is_selected {
                        theme::ACCENT
                    } else {
                        theme::FG_DIM
                    }),
                ),
                Span::styled(fit(label, label_width), theme::fg(theme::FG_MID)),
            ];
            let mut value_lines = value.lines();
            match value_lines.next().filter(|line| !line.trim().is_empty()) {
                Some(first) => {
                    let room = width.saturating_sub(4 + label_width);
                    // A path keeps its end, which names the checkout.
                    let shown = if *label == "path" {
                        super::text::truncate_start(first, room)
                    } else {
                        super::text::truncate_middle(first, room)
                    };
                    spans.push(Span::styled(shown, label_style(is_selected)));
                    let more = value_lines.count();
                    if more > 0 {
                        spans.push(Span::styled(format!("  +{more} lines"), theme::dim()));
                    }
                }
                None => spans.push(Span::styled("(none)", theme::fg(theme::BORDER))),
            }
            list_row(spans, is_selected, width)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), body);
    scrollbar(frame, outer, modal.border, choices.len(), offset);
}

// ── Confirmation ────────────────────────────────────────────────────────

fn confirm_verb(action: &ConfirmAction) -> &'static str {
    match action {
        ConfirmAction::HardRefresh => "clear caches",
        ConfirmAction::StopTerminal { .. } => "stop",
        ConfirmAction::KillAction { .. } => "kill",
        ConfirmAction::ReviewCheckout { .. } => "check out",
        ConfirmAction::RestoreRemoved { .. } => "restore",
        ConfirmAction::Github { ship: true, .. } => "ship",
        ConfirmAction::Github { ship: false, .. } => "mark ready",
        ConfirmAction::Remove { .. } => "remove",
        ConfirmAction::Cleanup { .. } => "clean",
        #[allow(unreachable_patterns)]
        _ => "confirm",
    }
}

fn destructive(action: &ConfirmAction) -> bool {
    !matches!(
        action,
        ConfirmAction::ReviewCheckout { .. }
            | ConfirmAction::RestoreRemoved { .. }
            | ConfirmAction::Github { .. }
    )
}

/// How a confirmation line reads: hazards are loud, kept rows recede, and
/// removals carry the trash glyph so a cleanup list scans as two columns.
fn confirm_line_kind(line: &str) -> (Option<(&'static str, Color)>, Style) {
    // Fleet cleanup prefixes each line with its host label.
    let body = line
        .split_once(": ")
        .map_or(line, |(_, rest)| rest)
        .trim_start();
    let starts = |prefix: &str| line.starts_with(prefix) || body.starts_with(prefix);
    let lower = line.to_lowercase();
    if starts("Will discard")
        || starts("Destroy")
        || starts("Checkout or hazards changed")
        || lower.contains("will be lost")
    {
        (Some((glyphs::CONFLICT, theme::ERR)), theme::fg(theme::ERR))
    } else if starts("Keep ") {
        (Some((glyphs::SLASH, theme::WARN)), theme::fg(theme::FG_MID))
    } else if starts("Remove ") {
        (Some((glyphs::TRASH, theme::FG_DIM)), theme::fg(theme::FG))
    } else if lower.contains("unavailable")
        || lower.contains("warning")
        || lower.contains("failed")
        || lower.contains("unexpected")
        || lower.contains("invalid")
    {
        (
            Some((glyphs::CONFLICT, theme::WARN)),
            theme::fg(theme::WARN),
        )
    } else {
        (None, theme::fg(theme::FG))
    }
}

/// Wrapped confirmation body: one entry of styled visual rows per logical
/// line. The first line names the subject, so it is bright.
fn confirm_rows(confirm: &ConfirmPrompt, width: usize) -> Vec<Vec<Line<'static>>> {
    confirm
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let (glyph, mut style) = confirm_line_kind(line);
            if index == 0 && glyph.is_none() && confirm.lines.len() > 1 {
                style = theme::bold(theme::FG_BRIGHT);
            }
            let indent = if glyph.is_some() { 3 } else { 0 };
            // Nerd Font glyphs measure one cell but paint two; a spare
            // cell keeps a hazard line off the right border.
            wrap(line, width.saturating_sub(indent + 1).max(1))
                .into_iter()
                .enumerate()
                .map(|(row, text)| {
                    let lead = match (row, glyph) {
                        (0, Some((glyph, color))) => {
                            Span::styled(format!("{glyph}  "), theme::fg(color))
                        }
                        _ => Span::raw(" ".repeat(indent)),
                    };
                    Line::from(vec![lead, Span::styled(text, style)])
                })
                .collect()
        })
        .collect()
}

fn confirmation(frame: &mut Frame<'_>, area: Rect, confirm: &mut ConfirmPrompt) {
    let cancel = confirm
        .cancel_key
        .filter(|key| !matches!(key, 'n' | 'q'))
        .map_or("n / esc / q".to_owned(), |key| {
            format!("n / {key} / esc / q")
        });
    let mut hints = vec![
        hint("y / ⏎", confirm_verb(&confirm.action)),
        hint(cancel, "cancel"),
    ];
    let widest = confirm
        .lines
        .iter()
        .map(|line| line.width() + 3)
        .max()
        .unwrap_or(0)
        .max(confirm.title.width() + 4)
        .clamp(44, 76);
    // Measure at the width the frame will actually get.
    let probe = Modal::new(confirm.title.clone(), hints.clone());
    let probe_width = probe.place(area, widest, 1, false).width.saturating_sub(4) as usize;
    let total: usize = confirm_rows(confirm, probe_width)
        .iter()
        .map(Vec::len)
        .sum();
    let max_body = probe.place(area, widest, usize::MAX / 4, false).height as usize;
    if total + 6 > max_body {
        hints.push(hint("j/k", "scroll"));
    }
    let modal = Modal::new(confirm.title.clone(), hints).border(if destructive(&confirm.action) {
        theme::WARN
    } else {
        theme::ACCENT
    });
    let outer = modal.place(area, widest, total.max(1), false);
    let body = modal.draw(frame, area, outer);
    let (width, visible) = (body.width as usize, body.height as usize);
    if visible == 0 || width == 0 {
        return;
    }
    let rows = confirm_rows(confirm, width);
    // `selected` is the first visible logical line; never scroll past the
    // point where the tail already fits.
    let mut max_offset = rows.len().saturating_sub(1);
    let mut tail = 0;
    for (index, entry) in rows.iter().enumerate().rev() {
        tail += entry.len();
        if tail > visible {
            break;
        }
        max_offset = index;
    }
    confirm.selected = confirm.selected.min(max_offset);
    let skipped: usize = rows[..confirm.selected].iter().map(Vec::len).sum();
    let total: usize = rows.iter().map(Vec::len).sum();
    let lines = rows
        .into_iter()
        .skip(confirm.selected)
        .flatten()
        .take(visible)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), body);
    scrollbar(frame, outer, modal.border, total, skipped);
}

// ── Logs and performance ────────────────────────────────────────────────

fn log(frame: &mut Frame<'_>, area: Rect, title: &str, lines: &[String], scroll: &mut usize) {
    let modal = Modal::new(
        title,
        vec![
            hint("j/k", "scroll"),
            hint("g/G", "top / bottom"),
            hint("esc / q", "close"),
        ],
    );
    let outer = if area.width < NARROW_WIDTH {
        modal.place(area, area.width as usize, lines.len(), true)
    } else {
        // Logs are wide content: use the terminal, not the dialog cap.
        let height = modal.place(area, 0, lines.len(), true).height;
        centered(area, area.width - 4, height)
    };
    let body = modal.draw(frame, area, outer);
    let (width, visible) = (body.width as usize, body.height as usize);
    if visible == 0 || width == 0 {
        return;
    }
    *scroll = (*scroll).min(lines.len().saturating_sub(visible));
    let rendered = if lines.is_empty() {
        vec![Line::styled("no output", theme::dim())]
    } else {
        lines
            .iter()
            .skip(*scroll)
            .take(visible)
            .map(|line| Line::styled(truncate_end(line, width), log_style(line)))
            .collect()
    };
    frame.render_widget(Paragraph::new(rendered), body);
    scrollbar(frame, outer, modal.border, lines.len(), *scroll);
}

fn log_style(line: &str) -> Style {
    let lower = line.to_ascii_lowercase();
    if lower.contains("error") || lower.contains("fail") || lower.contains("panic") {
        theme::fg(theme::ERR)
    } else if lower.contains("warn") {
        theme::fg(theme::WARN)
    } else {
        theme::fg(theme::FG)
    }
}

/// Label column of a perf meter row.
const PERF_LABEL_W: usize = 16;
/// Cells in a perf meter bar.
const PERF_BAR_W: usize = 22;
/// Narrower than this, meter rows drop the bar and keep the numbers.
const PERF_BAR_MIN_WIDTH: usize = PERF_LABEL_W + PERF_BAR_W + 18;

fn perf(frame: &mut Frame<'_>, model: &mut Model, area: Rect) {
    let title = match &model.board.perf_view {
        Some(view) => format!(
            "perf · {} downstream of wt · sampled {}",
            perf_count(view.downstream_count, "process"),
            view.sampled_at
        ),
        None => "perf · wt and everything downstream".to_owned(),
    };
    let modal = Modal::new(
        title,
        vec![
            hint("j/k", "scroll"),
            hint("r", "resample"),
            hint(
                "c",
                if model.perf_continuous {
                    "continuous: on"
                } else {
                    "continuous: off"
                },
            ),
            hint("i", "investigate"),
            hint("P / esc / q", "close"),
        ],
    );
    let outer = modal.place(area, MAX_FRAME_WIDTH as usize, 0, true);
    let body = modal.draw(frame, area, outer);
    let width = body.width as usize;
    if body.height == 0 || width == 0 {
        return;
    }
    let (header, lines) = match &model.board.perf_view {
        Some(view) => {
            let (header, mut lines) = perf_view_lines(view, model.last_frame_micros, width);
            // Remote hosts forward their plain report after a `Host:` line.
            if let Some(start) = model
                .board
                .perf
                .iter()
                .position(|line| line.starts_with("Host: "))
            {
                lines.push(Line::default());
                lines.extend(
                    model.board.perf[start..]
                        .iter()
                        .map(|line| perf_line(line, width)),
                );
            }
            (header, lines)
        }
        None if model.board.perf.is_empty() => {
            (vec![Line::styled("sampling…", theme::dim())], Vec::new())
        }
        None => (
            Vec::new(),
            model
                .board
                .perf
                .iter()
                .map(|line| perf_line(line, width))
                .collect(),
        ),
    };
    let header_rows = (header.len() as u16).min(body.height);
    frame.render_widget(
        Paragraph::new(header),
        Rect {
            height: header_rows,
            ..body
        },
    );
    let gap = u16::from(header_rows > 0 && body.height > header_rows + 1);
    let list = Rect {
        y: body.y + header_rows + gap,
        height: body.height - header_rows - gap,
        ..body
    };
    let visible = list.height as usize;
    model.perf_max_scroll = lines.len().saturating_sub(visible);
    model.perf_scroll = model.perf_scroll.min(model.perf_max_scroll);
    if visible == 0 {
        return;
    }
    let total = lines.len() + usize::from(header_rows + gap);
    let shown = lines
        .into_iter()
        .skip(model.perf_scroll)
        .take(visible)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(shown), list);
    scrollbar(frame, outer, modal.border, total, model.perf_scroll);
}

/// The fixed header (verdict and alarms) and the scrolling body of a
/// prepared perf sample, laid out for `width` cells.
fn perf_view_lines(
    view: &crate::PerfView,
    last_frame_micros: u128,
    width: usize,
) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    let ceiling = f64::from(view.cores.max(1)) * 100.0;
    let tone = match view.verdict_tone {
        crate::PerfTone::Calm => theme::OK,
        crate::PerfTone::Ours => theme::WARN,
        crate::PerfTone::Elsewhere => theme::INFO,
    };
    // Alarms lead: on a short terminal the header is clipped from the
    // bottom, and a stale or leaking sample must stay visible.
    let mut header = Vec::new();
    if let Some(error) = &view.error {
        header.push(Line::styled(
            truncate_end(
                &format!("resample failed, showing the last good sample: {error}"),
                width,
            ),
            theme::fg(theme::ERR),
        ));
    }
    if !view.orphans.is_empty() {
        header.push(Line::styled(
            truncate_end(
                &format!(
                    "{} leaked: the terminal died but the process survived; see LEAKED below",
                    perf_count(view.orphans.len(), "headless wt instance")
                ),
                width,
            ),
            theme::fg(theme::ERR),
        ));
    }
    header.extend(
        wrap(&view.verdict, width)
            .into_iter()
            .map(|line| Line::styled(line, theme::fg(tone))),
    );

    let mut lines = vec![
        perf_meter(
            "cpu (all)",
            view.system_cpu / ceiling,
            format!(
                "{} of {} · {} cores",
                perf_percent(view.system_cpu),
                perf_percent(ceiling),
                view.cores
            ),
            width,
        ),
        perf_meter(
            "cpu (wt)",
            view.wt_cpu / ceiling,
            format!(
                "{} · {} rss",
                perf_percent(view.wt_cpu),
                perf_memory(view.wt_rss_kb)
            ),
            width,
        ),
    ];
    if let (Some(used), Some(total)) = (view.memory_used_bytes, view.memory_total_bytes) {
        lines.push(perf_meter(
            "memory",
            if total == 0 {
                0.0
            } else {
                used as f64 / total as f64
            },
            format!(
                "{} of {}",
                perf_memory(used / 1024),
                perf_memory(total / 1024)
            ),
            width,
        ));
    }
    if let Some([one, five, fifteen]) = view.load_average {
        lines.push(perf_text_row(
            "load avg",
            vec![
                Span::styled(
                    format!("{one:.2}   {five:.2}   {fifteen:.2}"),
                    theme::fg(theme::FG),
                ),
                Span::styled("   1m / 5m / 15m", theme::dim()),
            ],
            width,
        ));
    }
    if last_frame_micros > 0 {
        lines.push(perf_text_row(
            "tui draw",
            vec![Span::styled(
                format!("{:.1} ms last frame", last_frame_micros as f64 / 1000.0),
                theme::fg(theme::FG),
            )],
            width,
        ));
    }

    lines.push(Line::default());
    lines.push(section_bar("wt downstream by category", width));
    if view.categories.is_empty() {
        lines.push(Line::styled(
            "nothing running downstream of wt",
            theme::dim(),
        ));
    }
    for group in &view.categories {
        lines.push(perf_meter(
            &group.label,
            group.cpu / ceiling,
            format!(
                "{:>5}  {:>6}  {}",
                perf_percent(group.cpu),
                perf_memory(group.rss_kb),
                perf_count(group.count, "proc")
            ),
            width,
        ));
    }
    if !view.sessions.is_empty() {
        lines.push(Line::default());
        lines.push(section_bar("by session", width));
        for group in &view.sessions {
            lines.push(perf_meter(
                &group.label,
                group.cpu / ceiling,
                format!(
                    "{:>5}  {:>6}  {}",
                    perf_percent(group.cpu),
                    perf_memory(group.rss_kb),
                    group.summary
                ),
                width,
            ));
        }
    }

    lines.push(Line::default());
    lines.push(section_bar("heaviest processes downstream of wt", width));
    lines.extend(perf_note(
        "%cpu is averaged by ps; on macOS over up to one minute. It is not instantaneous.",
        width,
    ));
    lines.extend(perf_processes(&view.top_downstream, ceiling, width));

    if !view.orphans.is_empty() {
        lines.push(Line::default());
        lines.push(section_bar(
            &format!(
                "LEAKED: {}",
                perf_count(view.orphans.len(), "headless wt instance")
            ),
            width,
        ));
        lines.extend(perf_note(
            "Reparented to launchd when a terminal died and not owned by a com.wt.* job. They keep polling until killed; verify identity first.",
            width,
        ));
        for process in &view.orphans {
            lines.push(Line::from(fit_spans(
                vec![
                    Span::styled(
                        format!("{:>5}", perf_percent(process.cpu)),
                        theme::fg(theme::ERR),
                    ),
                    Span::styled(format!("{:>6}", perf_memory(process.rss_kb)), theme::dim()),
                    Span::styled(format!("  pid {}", process.pid), theme::fg(theme::FG)),
                    Span::styled(format!("  up {}", process.elapsed), theme::dim()),
                ],
                width,
                Style::new(),
            )));
        }
        let pids = view
            .orphans
            .iter()
            .map(|process| process.pid.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(Line::styled(
            truncate_end(&format!("  kill {pids}"), width),
            theme::fg(theme::WARN),
        ));
    }

    lines.push(Line::default());
    lines.push(section_bar(
        "heaviest processes not downstream of wt",
        width,
    ));
    lines.extend(perf_note(
        "If the answer to \"why is my machine slow\" is here, it is not wt or its agents.",
        width,
    ));
    lines.extend(perf_processes(&view.top_other, ceiling, width));

    let mut caveats = Vec::new();
    if !view.tmux_probe_available {
        caveats.push("tmux session attribution unavailable.");
    }
    if !view.orphan_probe_available {
        caveats.push("Leaked-instance check unavailable on this platform.");
    }
    if !caveats.is_empty() {
        lines.push(Line::default());
        for caveat in caveats {
            lines.extend(perf_note(caveat, width));
        }
    }
    (header, lines)
}

/// `label  ███░░░  trailing`; narrow frames drop the bar, not the numbers.
fn perf_meter(label: &str, fraction: f64, trailing: String, width: usize) -> Line<'static> {
    if width < PERF_BAR_MIN_WIDTH {
        return perf_text_row(
            label,
            vec![Span::styled(trailing, theme::fg(theme::FG))],
            width,
        );
    }
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    // A non-zero value always fills one cell: an empty bar reads as idle.
    let filled = if fraction > 0.0 {
        ((fraction * PERF_BAR_W as f64).round() as usize).clamp(1, PERF_BAR_W)
    } else {
        0
    };
    Line::from(fit_spans(
        vec![
            Span::styled(fit(label, PERF_LABEL_W - 1) + " ", theme::dim()),
            Span::styled("█".repeat(filled), theme::fg(perf_load_color(fraction))),
            Span::styled("░".repeat(PERF_BAR_W - filled), theme::fg(theme::BORDER)),
            Span::styled(format!("  {trailing}"), theme::fg(theme::FG)),
        ],
        width,
        Style::new(),
    ))
}

fn perf_text_row(label: &str, mut value: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let label_width = PERF_LABEL_W.min(width / 3);
    let mut spans = vec![Span::styled(
        fit(label, label_width.saturating_sub(1)) + " ",
        theme::dim(),
    )];
    spans.append(&mut value);
    Line::from(fit_spans(spans, width, Style::new()))
}

fn perf_note(text: &str, width: usize) -> Vec<Line<'static>> {
    wrap(text, width)
        .into_iter()
        .map(|line| Line::styled(line, theme::dim()))
        .collect()
}

/// One row per process, `cpu mem [session] command`. Near-idle rows fold
/// into a count: this is the heaviest list, and 0% wrappers are noise.
fn perf_processes(
    processes: &[crate::PerfProcessView],
    ceiling: f64,
    width: usize,
) -> Vec<Line<'static>> {
    if processes.is_empty() {
        return vec![Line::styled("none", theme::dim())];
    }
    let busy = processes
        .iter()
        .filter(|process| process.cpu >= 0.5 || process.rss_kb >= 100 * 1024)
        .collect::<Vec<_>>();
    let shown = if busy.len() >= 3 {
        busy
    } else {
        processes.iter().take(3).collect()
    };
    let session_width = shown
        .iter()
        .filter_map(|process| process.session.as_deref())
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
        .min(16)
        .min(width.saturating_sub(27));
    let mut lines = shown
        .iter()
        .map(|process| {
            let mut spans = vec![
                Span::styled(
                    format!("{:>5}", perf_percent(process.cpu)),
                    theme::fg(perf_load_color(process.cpu / ceiling)),
                ),
                Span::styled(
                    format!("{:>6}  ", perf_memory(process.rss_kb)),
                    theme::dim(),
                ),
            ];
            if session_width > 0 {
                spans.push(Span::styled(
                    fit(process.session.as_deref().unwrap_or(""), session_width) + "  ",
                    theme::fg(theme::ACCENT_ALT),
                ));
            }
            spans.push(Span::styled(process.command.clone(), theme::fg(theme::FG)));
            Line::from(fit_spans(spans, width, Style::new()))
        })
        .collect::<Vec<_>>();
    let hidden = processes.len() - shown.len();
    if hidden > 0 {
        lines.push(Line::styled(
            truncate_end(
                &format!("      + {hidden} more near idle (<0.5% cpu, <100M)"),
                width,
            ),
            theme::dim(),
        ));
    }
    lines
}

/// Share of capacity above which a bar reads as pressure, not use.
fn perf_load_color(fraction: f64) -> Color {
    if fraction >= 0.9 {
        theme::ERR
    } else if fraction >= 0.6 {
        theme::WARN
    } else {
        theme::OK
    }
}

fn perf_percent(value: f64) -> String {
    format!("{value:.0}%")
}

/// `48M` under a gigabyte, `2.3G` above, so small numbers never read `0.0G`.
fn perf_memory(kb: u64) -> String {
    let mib = kb as f64 / 1024.0;
    if mib >= 1000.0 {
        format!("{:.1}G", mib / 1024.0)
    } else {
        format!("{mib:.0}M")
    }
}

fn perf_count(count: usize, noun: &str) -> String {
    let suffix = match (count, noun.ends_with('s')) {
        (1, _) => "",
        (_, true) => "es",
        _ => "s",
    };
    format!("{count} {noun}{suffix}")
}

/// Plain report lines (remote hosts, or a failure before the first sample):
/// headings get the section bar and a busy process warms toward error.
fn perf_line(line: &str, width: usize) -> Line<'static> {
    let indented = line.starts_with(' ');
    if !indented && line.ends_with(':') {
        return section_bar(line.trim_end_matches(':'), width);
    }
    let style = if line.contains("failed") || line.starts_with("LEAKED") {
        theme::fg(theme::ERR)
    } else if line.starts_with("Host: ") {
        theme::bold(theme::ACCENT)
    } else if line.contains("unavailable") || line.starts_with("Note:") {
        theme::dim()
    } else if indented {
        match perf_leading_percent(line) {
            Some(cpu) if cpu >= 90.0 => theme::fg(theme::ERR),
            Some(cpu) if cpu >= 50.0 => theme::fg(theme::WARN),
            _ => theme::fg(theme::FG_MID),
        }
    } else {
        theme::fg(theme::FG_BRIGHT)
    };
    Line::styled(truncate_end(line, width), style)
}

/// The `12%` that opens an indented report process row.
fn perf_leading_percent(line: &str) -> Option<f64> {
    line.trim_start().split_once('%')?.0.parse().ok()
}

/// A full-width filled bar so a section title reads as a block.
fn section_bar(title: &str, width: usize) -> Line<'static> {
    let style = Style::new()
        .bg(theme::SELECTED_BG)
        .fg(theme::FG_BRIGHT)
        .add_modifier(Modifier::BOLD);
    Line::styled(fit(&format!(" {title}"), width), style)
}

// ── Help ────────────────────────────────────────────────────────────────

enum HelpItem {
    Key {
        key: String,
        label: String,
    },
    Glyph {
        glyph: String,
        color: Color,
        label: &'static str,
        search: &'static str,
    },
    Note(String),
}

struct HelpBlock {
    title: String,
    /// Short single-line items laid out in two columns on wide frames.
    grid: bool,
    items: Vec<HelpItem>,
}

/// Split the shared keymap text into blocks. A key row separates its key
/// from its meaning with a run of two or more spaces; a bare line followed
/// by key rows is a block heading; any other bare line is a note.
fn keymap_blocks() -> Vec<HelpBlock> {
    let lines = crate::help::LINES;
    let split = |line: &str| {
        line.trim()
            .split_once("  ")
            .map(|(key, label)| (key.trim().to_owned(), label.trim().to_owned()))
            .filter(|(key, label)| !key.is_empty() && !label.is_empty())
    };
    let mut blocks: Vec<HelpBlock> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((key, label)) = split(line) {
            if blocks.is_empty() {
                blocks.push(HelpBlock {
                    title: "keys".into(),
                    grid: false,
                    items: Vec::new(),
                });
            }
            if let Some(block) = blocks.last_mut() {
                block.items.push(HelpItem::Key { key, label });
            }
            continue;
        }
        let heading = lines
            .get(index + 1)
            .is_some_and(|next| split(next).is_some());
        if heading || blocks.is_empty() {
            blocks.push(HelpBlock {
                title: line.trim().to_lowercase(),
                grid: false,
                items: Vec::new(),
            });
            if !heading && let Some(block) = blocks.last_mut() {
                block.title = "notes".into();
                block.items.push(HelpItem::Note(line.trim().to_owned()));
            }
        } else if let Some(block) = blocks.last_mut() {
            block.items.push(HelpItem::Note(line.trim().to_owned()));
        }
    }
    blocks
}

fn glyph_item(badge: badges::Badge, label: &'static str, search: &'static str) -> HelpItem {
    HelpItem::Glyph {
        glyph: badge.glyph.to_owned(),
        color: badge.color,
        label,
        search,
    }
}

/// The visual reference. Every glyph and color comes from `badges` (fed
/// synthetic rows where a helper needs one), so a recolor or a new state
/// cannot drift from what the list draws; only the prose is written here.
fn legend_blocks(policy: &DisplayPolicy, primary: &str) -> Vec<HelpBlock> {
    use badges::Badge;
    let work = |state: WorkState, label, search| {
        glyph_item(
            Badge {
                glyph: badges::work_state_glyph(state),
                color: badges::work_state_color(state),
            },
            label,
            search,
        )
    };
    let presented = |presentation: WorkPresentation| {
        badges::work_status_badge(Some(&WorkPresentation {
            effective_state: Some(WorkState::Ready),
            ..presentation
        }))
    };
    let mut work_items = vec![
        work(WorkState::NeedsHuman, "needs-human · blocked on you", ""),
        work(
            WorkState::NeedsTesting,
            "needs-testing · verification pending",
            "",
        ),
        work(WorkState::Ready, "ready · tested, merge it", ""),
        work(WorkState::Review, "review · findings being addressed", ""),
        work(WorkState::Working, "working · implementation in flight", ""),
        work(
            WorkState::Todo,
            "todo · queued, not started (or no status)",
            "",
        ),
        work(
            WorkState::Verified,
            "verified · merged and confirmed where it deployed",
            "",
        ),
        work(WorkState::Dropped, "dropped · will never land", ""),
    ];
    work_items.push(glyph_item(
        presented(WorkPresentation {
            stale: Some(true),
            ..Default::default()
        }),
        "hollow in a state color · stale, commits landed after the assertion",
        "stale",
    ));
    work_items.push(glyph_item(
        presented(WorkPresentation {
            blocked: true,
            ..Default::default()
        }),
        "ready but gated on an external prerequisite",
        "blocked gate",
    ));
    work_items.push(glyph_item(
        presented(WorkPresentation {
            verification_overdue: true,
            ..Default::default()
        }),
        "post-merge verification overdue",
        "verify after merge",
    ));

    let row = |edit: &dyn Fn(&mut BoardRow)| {
        let mut row = BoardRow {
            git: GitPresentation {
                tracked_changes: Some(0),
                untracked_files: Some(0),
                ..Default::default()
            },
            ..Default::default()
        };
        edit(&mut row);
        row
    };
    let busy = |op: &str| {
        let op = op.to_owned();
        row(&move |row| {
            row.busy = Some(BusyView {
                op: op.clone(),
                ..Default::default()
            })
        })
    };
    let loud = |row: BoardRow| {
        badges::loud_status_badge(&row).unwrap_or(Badge {
            glyph: " ",
            color: theme::FG_DIM,
        })
    };
    let mut status_items = vec![
        glyph_item(
            loud(busy("init")),
            "busy · creating or installing",
            "rocket",
        ),
        glyph_item(loud(busy("remove")), "busy · removing", "trash"),
        glyph_item(loud(busy("restack")), "busy · restacking", "rebase"),
        glyph_item(
            loud(row(&|row| row.path_missing = true)),
            "missing · the worktree path vanished",
            "",
        ),
        glyph_item(
            loud(row(&|row| row.branch_gone = true)),
            "gone · branch deleted upstream",
            "",
        ),
        glyph_item(
            loud(row(&|row| row.git.landed_on = Some(LandingKind::Base))),
            "merged into the base branch",
            "landed",
        ),
    ];
    if policy.production {
        status_items.push(glyph_item(
            Badge {
                glyph: glyphs::PRODUCTION,
                color: theme::FG_DIM,
            },
            "in production (color is the work status)",
            "landed release",
        ));
    }
    let (dirty, _) = badges::status_verb(&row(&|row| row.git.tracked_changes = Some(1)));
    status_items.push(glyph_item(dirty, "uncommitted changes", "dirty pencil"));

    let pr = |state: &str, draft: bool| {
        badges::pr_state_badge(&PrPresentation {
            state: Some(state.into()),
            draft,
            ..Default::default()
        })
    };
    let mut pr_items = vec![
        glyph_item(pr("OPEN", false), "PR open", ""),
        glyph_item(pr("OPEN", true), "PR draft", ""),
        glyph_item(pr("MERGED", false), "PR merged", ""),
        glyph_item(pr("CLOSED", false), "PR closed", ""),
        glyph_item(
            badges::pr_slot_badge(&PrPresentation {
                state: Some("OPEN".into()),
                auto_merge_armed: true,
                ..Default::default()
            }),
            "merge when ready armed, not queued yet",
            "auto-merge",
        ),
    ];
    for (state, label) in [
        ("MERGEABLE", "merge queue position N · mergeable"),
        ("QUEUED", "merge queue position N · waiting"),
        ("UNMERGEABLE", "merge queue position N · blocked"),
    ] {
        pr_items.push(HelpItem::Glyph {
            glyph: format!("{} N", glyphs::MERGE_QUEUE),
            color: badges::merge_queue_state(state).1,
            label,
            search: "merge queue",
        });
    }
    for (checks, label) in [
        (CheckState::Pass, "checks passing"),
        (CheckState::Fail, "checks failing"),
        (CheckState::Pending, "checks pending"),
    ] {
        if let Some(badge) = badges::check_badge(checks) {
            pr_items.push(glyph_item(badge, label, "ci"));
        }
    }
    if policy.reviewers {
        for (review, label) in [
            (ReviewState::Approved, "review approved"),
            (ReviewState::ChangesRequested, "changes requested"),
            (ReviewState::Pending, "review requested, waiting"),
            (ReviewState::Unrequested, "no reviewer requested"),
        ] {
            if let Some(badge) = badges::review_badge(review, policy) {
                pr_items.push(glyph_item(badge, label, "human review"));
            }
        }
    }
    for (state, stale, label) in [
        ("unresolved", false, "review bot · unresolved findings"),
        ("pending", false, "review bot · reviewing"),
        ("clean", false, "review bot · clean"),
        ("clean", true, "review bot · clean, but an older head"),
    ] {
        let pr = PrPresentation {
            state: Some("OPEN".into()),
            review_bot: Some(ReviewBotView {
                state: state.into(),
                stale,
                ..Default::default()
            }),
            ..Default::default()
        };
        if let Some(badge) = badges::review_bot_badge(&pr, policy) {
            pr_items.push(glyph_item(badge, label, "coderabbit"));
        }
    }

    let restack_running = badges::rebase_badge(&busy("restack"), None);
    let resolving = badges::rebase_badge(&row(&|row| row.git.rebasing = true), None);
    let conflict = badges::rebase_badge(
        &row(&|row| row.git.base_conflicts = Some(vec!["file".into()])),
        None,
    );
    let mut row_items = Vec::new();
    for (badge, label, search) in [
        (restack_running, "restack running (whole chain)", "rebase"),
        (resolving, "mid-rebase or conflict being resolved", "rebase"),
        (
            conflict,
            "will not rebase cleanly onto its base",
            "conflict",
        ),
    ] {
        if let Some(badge) = badge {
            row_items.push(glyph_item(badge, label, search));
        }
    }
    row_items.extend([
        glyph_item(
            Badge {
                glyph: glyphs::BOLT,
                color: theme::WARN,
            },
            "stage deployed or dev server running",
            "environment",
        ),
        glyph_item(
            Badge {
                glyph: glyphs::COMMENT,
                color: theme::OK,
            },
            "action running",
            "!",
        ),
        glyph_item(
            Badge {
                glyph: glyphs::REMOTE,
                color: theme::INFO,
            },
            "hosted on an SSH remote",
            "remote",
        ),
    ]);
    for (harness, label) in [
        ("claude", "Claude session live"),
        ("codex", "Codex session live"),
        ("opencode", "OpenCode session live"),
    ] {
        row_items.push(glyph_item(
            Badge {
                glyph: badges::harness_glyph(harness),
                color: badges::harness_color(harness),
            },
            label,
            "harness ai",
        ));
    }

    let session_items = [
        ("working", "working"),
        ("asking", "asking"),
        ("polling", "polling"),
        ("waiting", "waiting (your turn)"),
        ("idle", "idle"),
        ("abandoned", "abandoned"),
        ("unknown", "unknown"),
    ]
    .into_iter()
    .map(|(state, label)| HelpItem::Glyph {
        glyph: badges::session_state_dot(state).to_owned(),
        color: badges::session_state_color(primary, state),
        label,
        search: "ai session state",
    })
    .collect();

    vec![
        HelpBlock {
            title: "work status (u, wt status)".into(),
            grid: false,
            items: work_items,
        },
        HelpBlock {
            title: "status glyphs".into(),
            grid: false,
            items: status_items,
        },
        HelpBlock {
            title: "PR and CI badges".into(),
            grid: false,
            items: pr_items,
        },
        HelpBlock {
            title: "row badges".into(),
            grid: false,
            items: row_items,
        },
        HelpBlock {
            title: "AI session states".into(),
            grid: true,
            items: session_items,
        },
    ]
}

fn item_text(item: &HelpItem) -> String {
    match item {
        HelpItem::Key { key, label } => format!("{key} {label}"),
        HelpItem::Glyph { label, search, .. } => format!("{label} {search}"),
        HelpItem::Note(note) => note.clone(),
    }
    .to_lowercase()
}

/// A block whose title matches keeps every row; otherwise only matching
/// rows survive, and a block with none is dropped.
fn filter_blocks(blocks: Vec<HelpBlock>, query: &str) -> Vec<HelpBlock> {
    if query.is_empty() {
        return blocks;
    }
    blocks
        .into_iter()
        .filter_map(|mut block| {
            if !block.title.to_lowercase().contains(query) {
                block.items.retain(|item| item_text(item).contains(query));
            }
            (!block.items.is_empty()).then_some(block)
        })
        .collect()
}

/// Spans for `text` with every case-insensitive match of `query` marked.
fn highlighted(text: &str, query: &str, style: Style) -> Vec<Span<'static>> {
    let lower = text.to_lowercase();
    if query.is_empty() || lower.len() != text.len() {
        return vec![Span::styled(text.to_owned(), style)];
    }
    let mark = style
        .fg(theme::WARN)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mut spans = Vec::new();
    let mut start = 0;
    while let Some(found) = lower[start..].find(query) {
        let at = start + found;
        if at > start {
            spans.push(Span::styled(text[start..at].to_owned(), style));
        }
        let end = at + query.len();
        spans.push(Span::styled(text[at..end].to_owned(), mark));
        start = end;
    }
    if start < text.len() {
        spans.push(Span::styled(text[start..].to_owned(), style));
    }
    spans
}

fn help_item_lines(
    item: &HelpItem,
    width: usize,
    key_width: usize,
    query: &str,
) -> Vec<Line<'static>> {
    match item {
        HelpItem::Key { key, label } => {
            let key_width = key_width.min((width * 2 / 5).max(1));
            let wrapped = wrap(label, width.saturating_sub(key_width).max(1));
            wrapped
                .into_iter()
                .enumerate()
                .map(|(row, text)| {
                    let mut spans = if row == 0 {
                        let key = truncate_end(key, key_width.saturating_sub(1));
                        let pad = key_width.saturating_sub(key.width());
                        let mut spans = highlighted(&key, query, theme::bold(theme::ACCENT));
                        spans.push(Span::raw(" ".repeat(pad)));
                        spans
                    } else {
                        vec![Span::raw(" ".repeat(key_width))]
                    };
                    spans.extend(highlighted(&text, query, theme::fg(theme::FG)));
                    Line::from(spans)
                })
                .collect()
        }
        HelpItem::Glyph {
            glyph,
            color,
            label,
            ..
        } => {
            // Five cells: a glyph slot (two) plus room, or `` N`` notation.
            let glyph_width = 5;
            wrap(label, width.saturating_sub(glyph_width).max(1))
                .into_iter()
                .enumerate()
                .map(|(row, text)| {
                    let mut spans = vec![if row == 0 {
                        Span::styled(fit(glyph, glyph_width), theme::fg(*color))
                    } else {
                        Span::raw(" ".repeat(glyph_width))
                    }];
                    spans.extend(highlighted(&text, query, theme::fg(theme::FG)));
                    Line::from(spans)
                })
                .collect()
        }
        HelpItem::Note(note) => wrap(note, width.max(1))
            .into_iter()
            .map(|text| Line::from(highlighted(&text, query, theme::dim())))
            .collect(),
    }
}

/// One key column for the whole keymap, sized to its widest key, so every
/// block's meanings start at the same column whatever the filter keeps.
fn key_column(blocks: &[HelpBlock]) -> usize {
    blocks
        .iter()
        .flat_map(|block| &block.items)
        .filter_map(|item| match item {
            HelpItem::Key { key, .. } => Some(key.width() + 2),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        .min(HELP_KEY_MAX)
}

fn help_lines(
    blocks: &[HelpBlock],
    width: usize,
    key_width: usize,
    query: &str,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        lines.push(section_bar(&block.title, width));
        if block.grid && width >= 60 {
            let column = (width - 3) / 2;
            let per = block.items.len().div_ceil(2);
            for row in 0..per {
                let mut spans = Vec::new();
                for (side, item) in [block.items.get(row), block.items.get(row + per)]
                    .into_iter()
                    .enumerate()
                {
                    let Some(item) = item else { continue };
                    if side == 1 {
                        spans.push(Span::raw("   "));
                    }
                    let cell = help_item_lines(item, column, key_width, query)
                        .into_iter()
                        .next()
                        .unwrap_or_default();
                    spans.extend(fit_spans(cell.spans, column, Style::new()));
                }
                lines.push(Line::from(spans));
            }
        } else {
            for item in &block.items {
                lines.extend(help_item_lines(item, width, key_width, query));
            }
        }
    }
    lines
}

fn help(frame: &mut Frame<'_>, model: &mut Model, area: Rect) {
    let query_text = model.help_query.text();
    let query = query_text.trim().to_lowercase();
    let mut blocks = keymap_blocks();
    blocks.extend(legend_blocks(&model.board.display, model.primary_harness()));
    let key_width = key_column(&blocks);
    let blocks = filter_blocks(blocks, &query);
    let matches: usize = blocks.iter().map(|block| block.items.len()).sum();
    let empty = !query.is_empty() && matches == 0;
    let hints = if model.help_searching {
        vec![
            hint("type", "filter"),
            hint("⏎", "apply"),
            hint("esc", "cancel"),
        ]
    } else if !query_text.is_empty() {
        let mut hints = Vec::new();
        if !empty {
            hints.push(hint("j/k", "scroll"));
        }
        hints.extend([
            hint("/", "search"),
            hint("esc", "clear"),
            hint("? / q", "close"),
        ]);
        hints
    } else {
        vec![
            hint("j/k", "scroll"),
            hint("g/G", "top / bottom"),
            hint("/", "search"),
            hint("? / esc / q", "close"),
        ]
    };
    let modal = Modal::new("help · keys and glyphs", hints);
    let outer = modal.place(area, MAX_FRAME_WIDTH as usize, 0, true);
    let body = modal.draw(frame, area, outer);
    let width = body.width as usize;
    if body.height == 0 || width == 0 {
        return;
    }
    let mut list = body;
    if model.help_searching || !query_text.is_empty() {
        let mut spans = vec![Span::styled(
            "/",
            theme::bold(if model.help_searching {
                theme::ACCENT
            } else {
                theme::FG_DIM
            }),
        )];
        let count = if empty || query.is_empty() {
            String::new()
        } else {
            format!("  {matches} match{}", if matches == 1 { "" } else { "es" })
        };
        let room = width.saturating_sub(1 + count.width());
        if model.help_searching {
            // A drawn `▌` at the caret, like the TS prompt; it reserves
            // one cell so the caret never falls off the end.
            let (text, cursor) = model.help_query.viewport(room.saturating_sub(1));
            let at = text
                .char_indices()
                .scan(0usize, |cells, (index, ch)| {
                    let here = *cells;
                    *cells += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    Some((index, here))
                })
                .find(|(_, cells)| *cells >= usize::from(cursor))
                .map_or(text.len(), |(index, _)| index);
            let style = theme::fg(theme::FG_BRIGHT);
            spans.push(Span::styled(text[..at].to_owned(), style));
            spans.push(Span::styled("▌", theme::fg(theme::ACCENT)));
            let tail = room.saturating_sub(text[..at].width() + 1);
            spans.push(Span::styled(fit(&text[at..], tail), style));
        } else {
            let (text, _) = model.help_query.viewport(room);
            spans.push(Span::styled(fit(&text, room), theme::fg(theme::FG_BRIGHT)));
        }
        spans.push(Span::styled(count, theme::dim()));
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect { height: 1, ..body },
        );
        let skip = 2.min(body.height);
        list = Rect {
            y: body.y + skip,
            height: body.height - skip,
            ..body
        };
    }
    if list.height == 0 {
        return;
    }
    let visible = list.height as usize;
    if empty {
        let message = truncate_end(&format!("no matches for \"{}\"", query_text.trim()), width);
        let row = Rect {
            y: list.y + list.height / 2,
            height: 1,
            ..list
        };
        frame.render_widget(
            Paragraph::new(Line::styled(message, theme::dim()))
                .alignment(ratatui::layout::Alignment::Center),
            row,
        );
        model.help_max_scroll = 0;
        model.help_scroll = 0;
        return;
    }
    // Leave the scrollbar its column on the border, not over content.
    let lines = help_lines(&blocks, width, key_width, &query);
    model.help_max_scroll = lines.len().saturating_sub(visible);
    model.help_scroll = model.help_scroll.min(model.help_max_scroll);
    let total = lines.len();
    let shown = lines
        .into_iter()
        .skip(model.help_scroll)
        .take(visible)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(shown), list);
    scrollbar(frame, outer, modal.border, total, model.help_scroll);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PickerOption, RemovalRevision, ReviewerOption, model::ConfirmPrompt};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    fn draw(model: &mut Model, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render(frame, model, frame.area()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn find(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
        rows(buffer).iter().enumerate().find_map(|(y, row)| {
            row.find(needle).map(|byte| {
                let x = row[..byte].width() as u16;
                (x, y as u16)
            })
        })
    }

    fn remove_confirm() -> ConfirmPrompt {
        ConfirmPrompt {
            action: ConfirmAction::Remove {
                key: "one".into(),
                force: true,
                revision: RemovalRevision {
                    key: "one".into(),
                    path: "/worktrees/one".into(),
                    branch: "feature/one".into(),
                    head: "abc123".into(),
                    digest: "deadbeef".into(),
                    hazards: vec!["uncommitted changes".into()],
                    published_base: None,
                },
            },
            title: "Force remove worktree?".into(),
            lines: vec![
                "one (feature/one)".into(),
                "Will discard or abandon: uncommitted changes".into(),
            ],
            selected: 0,
            cancel_key: Some('d'),
        }
    }

    #[test]
    fn confirm_modal_warns_and_colors_hazards() {
        let mut model = Model {
            interaction: Interaction::Confirm(remove_confirm()),
            ..Model::default()
        };
        let buffer = draw(&mut model, 120, 40);
        let text = rows(&buffer).join("\n");
        assert!(text.contains("Force remove worktree?"), "{text}");
        assert!(text.contains("y / ⏎ remove"), "{text}");
        assert!(text.contains("n / d / esc / q cancel"), "{text}");
        let (x, y) = find(&buffer, "╭").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::WARN, "destructive border warns");
        let (x, y) = find(&buffer, "Will discard").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::ERR, "hazard line is loud");
        assert_eq!(buffer[(x, y)].bg, theme::BG_ALT);
        let (x, y) = find(&buffer, "one (feature/one)").unwrap();
        assert_eq!(
            buffer[(x, y)].fg,
            theme::FG_BRIGHT,
            "subject line is bright"
        );
    }

    #[test]
    fn picker_shows_chords_status_glyphs_and_selection() {
        let option = |state: &str, chord| PickerOption {
            value: Some(state.into()),
            label: state.into(),
            chord: Some(chord),
            note: None,
            verify_after_merge: None,
            detail: None,
        };
        let mut model = Model {
            interaction: Interaction::Picker(PickerPrompt {
                action: PickerAction::Status { key: "one".into() },
                title: "Work status".into(),
                options: vec![
                    option("todo", 't'),
                    option("ready", 'y'),
                    PickerOption {
                        value: None,
                        label: "Clear status".into(),
                        chord: Some('x'),
                        note: None,
                        verify_after_merge: None,
                        detail: None,
                    },
                ],
                selected: 1,
            }),
            ..Model::default()
        };
        let buffer = draw(&mut model, 100, 30);
        let text = rows(&buffer).join("\n");
        assert!(text.contains("u / ⏎ pick"), "{text}");
        assert!(text.contains("m note"), "{text}");
        assert!(text.contains("letter quick pick"), "{text}");
        let (x, y) = find(&buffer, "› y").unwrap();
        assert_eq!(buffer[(x, y)].bg, theme::SELECTED_BG);
        let dot = find(&buffer, &format!("{}  ready", glyphs::DOT)).unwrap();
        assert_eq!(buffer[dot].fg, theme::OK, "ready dot matches the list");
        assert!(text.contains(&format!("t {}  todo", glyphs::DOT_OUTLINE)));
    }

    #[test]
    fn action_picker_groups_entries_and_dims_unavailable_ones() {
        let option = |label: &str, chord| PickerOption {
            value: Some(label.into()),
            label: label.into(),
            chord,
            note: None,
            verify_after_merge: None,
            detail: None,
        };
        let mut model = Model {
            interaction: Interaction::Picker(PickerPrompt {
                action: PickerAction::Actions {
                    surface: crate::ActionSurface::Row { key: "one".into() },
                },
                title: "one actions".into(),
                options: vec![
                    option("Toggle merge when ready", Some('m')),
                    option("Agent: Update status", Some('u')),
                    option("Agent: Deploy (needs a PR)", Some('d')),
                    option("Custom prompt…", Some('c')),
                ],
                selected: 0,
            }),
            ..Model::default()
        };
        let buffer = draw(&mut model, 100, 30);
        let text = rows(&buffer).join("\n");
        assert!(text.contains("  agent"), "{text}");
        assert!(text.contains("u Update status"), "{text}");
        let (x, y) = find(&buffer, "Deploy").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::FG_DIM);
        assert!(text.contains("! / ⏎ pick"), "{text}");
    }

    #[test]
    fn help_groups_keys_and_ends_with_a_glyph_legend() {
        let mut model = Model {
            help: true,
            ..Model::default()
        };
        let buffer = draw(&mut model, 200, 50);
        let text = rows(&buffer).join("\n");
        assert!(text.contains("help · keys and glyphs"));
        let (x, y) = find(&buffer, " navigation").unwrap();
        assert_eq!(buffer[(x + 1, y)].bg, theme::SELECTED_BG, "heading bar");
        assert!(model.help_max_scroll > 0);
        // Scroll to the end: the legend uses the list's own glyphs.
        model.help_scroll = usize::MAX;
        let buffer = draw(&mut model, 200, 50);
        let text = rows(&buffer).join("\n");
        assert!(text.contains("AI session states"), "{text}");
        assert!(text.contains(glyphs::CHECK_FAIL));
        assert_eq!(model.help_scroll, model.help_max_scroll);
    }

    #[test]
    fn help_search_filters_and_highlights() {
        let mut model = Model {
            help: true,
            help_query: crate::LineEditor::new("merge"),
            ..Model::default()
        };
        let buffer = draw(&mut model, 120, 40);
        let text = rows(&buffer).join("\n");
        assert!(text.contains("match"), "{text}");
        assert!(!text.contains("Open editor"), "{text}");
        let (x, y) = find(&buffer, "merge queue position").unwrap();
        assert!(buffer[(x, y)].modifier.contains(Modifier::UNDERLINED));
        model.help_query = crate::LineEditor::new("zzzz-nothing");
        let text = rows(&draw(&mut model, 120, 40)).join("\n");
        assert!(text.contains("no matches for \"zzzz-nothing\""));
    }

    #[test]
    fn modals_fit_inside_a_small_terminal() {
        let mut model = Model {
            interaction: Interaction::Confirm(ConfirmPrompt {
                lines: (0..30).map(|n| format!("Remove worktree-{n}")).collect(),
                ..remove_confirm()
            }),
            ..Model::default()
        };
        let buffer = draw(&mut model, 80, 24);
        let text = rows(&buffer);
        assert!(text.iter().any(|row| row.contains("╭")));
        assert!(text.iter().any(|row| row.contains("╰")));
        assert!(text.join("\n").contains("j/k scroll"));
        // Scrolling past the end clamps to the last full page.
        if let Interaction::Confirm(confirm) = &mut model.interaction {
            confirm.selected = 29;
        }
        let text = rows(&draw(&mut model, 80, 24)).join("\n");
        assert!(text.contains("worktree-29"));
        let Interaction::Confirm(confirm) = &model.interaction else {
            unreachable!()
        };
        assert!(confirm.selected < 29);

        model.interaction = Interaction::Reviewers(ReviewerPrompt {
            key: "one".into(),
            pr_number: 7,
            original: Vec::new(),
            candidates: vec![ReviewerOption {
                login: "a".into(),
                label: "alice".into(),
                selected: true,
            }],
            selected: 0,
        });
        let text = rows(&draw(&mut model, 80, 24)).join("\n");
        assert!(text.contains("[x] alice"));
        model.interaction = Interaction::None;
        model.help = true;
        for (width, height) in [(80, 24), (40, 10), (12, 5)] {
            draw(&mut model, width, height);
        }
    }

    fn action_option(label: &str, chord: Option<char>, detail: Option<&str>) -> PickerOption {
        PickerOption {
            value: Some(label.into()),
            label: label.into(),
            chord,
            note: None,
            verify_after_merge: None,
            detail: detail.map(str::to_owned),
        }
    }

    fn action_model(surface: crate::ActionSurface) -> Model {
        Model {
            interaction: Interaction::Picker(PickerPrompt {
                action: PickerAction::Actions { surface },
                title: "actions".into(),
                options: vec![
                    action_option("Agent: Update status", Some('u'), Some("claude update")),
                    action_option("Agent: Continue", Some('g'), Some("claude continue")),
                    action_option(
                        "Shell: Deploy",
                        Some('d'),
                        Some("$ deploy-to-the-staging-environment-now"),
                    ),
                    action_option("Custom prompt…", Some('c'), Some("freeform")),
                ],
                selected: 0,
            }),
            ..Model::default()
        }
    }

    #[test]
    fn action_palette_names_its_surface_opener() {
        for (surface, key) in [
            (crate::ActionSurface::Row { key: "one".into() }, "!"),
            (crate::ActionSurface::Manager { key: None }, "M"),
            (
                crate::ActionSurface::Slot {
                    target: crate::SessionTarget::WtSource,
                },
                "<",
            ),
            (
                crate::ActionSurface::Slot {
                    target: crate::SessionTarget::Main,
                },
                ">",
            ),
        ] {
            let mut model = action_model(surface);
            let text = rows(&draw(&mut model, 200, 50)).join("\n");
            assert!(text.contains(&format!("{key} / ⏎ pick")), "{key}: {text}");
        }
    }

    #[test]
    fn action_palette_is_wide_with_one_blank_between_groups() {
        let mut model = action_model(crate::ActionSurface::Row { key: "one".into() });
        let buffer = draw(&mut model, 200, 50);
        let (left, top) = find(&buffer, "╭").unwrap();
        let (right, _) = find(&buffer, "╮").unwrap();
        assert!(right - left + 1 >= 96, "frame {} wide", right - left + 1);
        let text = rows(&buffer);
        assert!(
            text.iter()
                .any(|row| row.contains("deploy-to-the-staging-environment-now"))
        );
        let body = text
            .iter()
            .skip(usize::from(top) + 1)
            .map(|row| {
                row.chars()
                    .skip(usize::from(left) + 1)
                    .take(usize::from(right - left - 1))
                    .collect::<String>()
                    .trim()
                    .to_owned()
            })
            .take_while(|row| !row.contains("pick"))
            .collect::<Vec<_>>();
        let first = body.iter().position(|row| !row.is_empty()).unwrap();
        let end = body.iter().rposition(|row| !row.is_empty()).unwrap();
        let body = &body[first..=end];
        assert_eq!(body[0], "agent", "{body:#?}");
        let shell = body.iter().position(|row| row == "shell").unwrap();
        assert!(body[shell - 1].is_empty(), "{body:#?}");
        assert!(!body[shell - 2].is_empty(), "{body:#?}");
        // The ungrouped custom entry follows one blank, and no group has a
        // blank inside it.
        assert!(body[body.len() - 2].is_empty(), "{body:#?}");
        assert_eq!(
            body.iter().filter(|row| row.is_empty()).count(),
            2,
            "{body:#?}"
        );
    }

    #[test]
    fn confirm_body_wraps_clear_of_the_right_border_and_offers_q() {
        let mut model = Model {
            interaction: Interaction::Confirm(ConfirmPrompt {
                lines: vec![
                    "one (feature/one)".into(),
                    // An unbroken run hard-splits at the full wrap width.
                    format!("Will discard or abandon: {}", "x".repeat(200)),
                ],
                ..remove_confirm()
            }),
            ..Model::default()
        };
        let buffer = draw(&mut model, 120, 40);
        let (_, top) = find(&buffer, "╭").unwrap();
        let (right, _) = find(&buffer, "╮").unwrap();
        let text = rows(&buffer).join("\n");
        assert!(text.contains("n / d / esc / q cancel"), "{text}");
        // The hazard glyph paints two cells in a Nerd Font terminal but
        // measures one, so the body keeps a spare cell before the padding.
        for y in top + 1..buffer.area.height {
            if buffer[(right, y)].symbol() == "╯" {
                break;
            }
            for gap in 1..=2 {
                assert_eq!(
                    buffer[(right - gap, y)].symbol(),
                    " ",
                    "row {y} touches the border"
                );
            }
        }
    }

    #[test]
    fn ready_plus_verify_wears_the_warn_glyph() {
        let mut model = Model {
            interaction: Interaction::Picker(PickerPrompt {
                action: PickerAction::Status { key: "one".into() },
                title: "Work status".into(),
                options: vec![PickerOption {
                    value: Some("ready".into()),
                    label: "ready + verify after merge".into(),
                    chord: Some('a'),
                    note: None,
                    verify_after_merge: Some("probe".into()),
                    detail: None,
                }],
                selected: 0,
            }),
            ..Model::default()
        };
        let buffer = draw(&mut model, 100, 30);
        let dot = find(&buffer, &format!("{}  ready + verify", glyphs::DOT)).unwrap();
        assert_eq!(buffer[dot].fg, theme::WARN);
    }

    #[test]
    fn yank_paths_keep_their_end_visible() {
        let mut model = Model {
            board: std::sync::Arc::new(crate::Board {
                rows: vec![BoardRow {
                    key: "one".into(),
                    slug: "the-slug-at-the-end".into(),
                    branch: "feature/one".into(),
                    path: format!("/{}/the-slug-at-the-end", "deep/".repeat(40)),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Model::default()
        };
        model.rebuild_items();
        model.selected = Some(0);
        model.yank = Some(0);
        let text = rows(&draw(&mut model, 100, 30)).join("\n");
        let path = text
            .lines()
            .find(|row| row.contains(" path "))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(
            path.contains("…") && path.contains("/the-slug-at-the-end"),
            "{path}"
        );
    }

    #[test]
    fn help_search_draws_a_caret_and_finds_rebase_aliases() {
        let mut model = Model {
            help: true,
            help_searching: true,
            help_query: crate::LineEditor::new("rebase"),
            ..Model::default()
        };
        let text = rows(&draw(&mut model, 160, 50)).join("\n");
        assert!(text.contains("/rebase▌"), "{text}");
        assert!(text.contains("Restack (rebase)"), "{text}");
        assert!(text.contains("never rebases"), "{text}");

        let mut model = Model {
            help: true,
            help_query: crate::LineEditor::new("ahead"),
            ..Model::default()
        };
        let text = rows(&draw(&mut model, 160, 50)).join("\n");
        assert!(text.contains("sync notation"), "{text}");
        assert!(text.contains("[↑N ↓M]"), "{text}");
        for query in ["--base", "mouse drag"] {
            let mut model = Model {
                help: true,
                help_query: crate::LineEditor::new(query),
                ..Model::default()
            };
            let text = rows(&draw(&mut model, 160, 50)).join("\n");
            assert!(!text.contains("no matches"), "{query}: {text}");
        }
    }

    #[test]
    fn perf_overlay_leads_with_a_verdict_and_meters() {
        let process =
            |pid, cpu, rss_kb, command: &str, session: Option<&str>| crate::PerfProcessView {
                pid,
                cpu,
                rss_kb,
                elapsed: "01:00".into(),
                command: command.into(),
                session: session.map(Into::into),
            };
        let view = crate::PerfView {
            sampled_at: "14:03:22".into(),
            verdict: "wt is most of the load: 600% of the 800% in use (75%)".into(),
            verdict_tone: crate::PerfTone::Ours,
            cores: 8,
            system_cpu: 800.0,
            wt_cpu: 600.0,
            wt_rss_kb: 48 * 1024,
            downstream_count: 3,
            load_average: Some([7.5, 6.0, 4.25]),
            memory_used_bytes: Some(20 << 30),
            memory_total_bytes: Some(32 << 30),
            categories: vec![crate::PerfGroupView {
                label: "agents".into(),
                cpu: 590.0,
                rss_kb: 2_411_725,
                count: 2,
                summary: String::new(),
            }],
            sessions: Vec::new(),
            top_downstream: vec![
                process(11, 590.0, 2_411_725, "codex --resume", Some("feature")),
                process(12, 0.0, 10, "sleep 1", None),
                process(13, 0.0, 10, "sleep 2", None),
                process(14, 0.0, 10, "sleep 3", None),
            ],
            top_other: vec![process(20, 150.0, 1024, "WindowServer", None)],
            orphans: vec![process(40, 1.0, 1024, "wt", None)],
            orphan_probe_available: true,
            tmux_probe_available: true,
            error: None,
        };
        let mut board = crate::Board {
            perf_view: Some(Box::new(view)),
            perf: vec![
                "report".into(),
                "Host: box".into(),
                "Verdict: remote calm".into(),
            ],
            ..Default::default()
        };
        board.name = "repo".into();
        let mut model = Model {
            show_perf: true,
            board: board.into(),
            ..Model::default()
        };
        let buffer = draw(&mut model, 120, 80);
        let text = rows(&buffer).join("\n");
        assert!(
            text.contains("perf · 3 processes downstream of wt · sampled 14:03:22"),
            "{text}"
        );
        let (x, y) = find(&buffer, "wt is most of the load").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::WARN);
        assert!(text.contains("1 headless wt instance leaked"), "{text}");
        assert!(text.contains("cpu (all)"), "{text}");
        assert!(text.contains("800% of 800% · 8 cores"), "{text}");
        assert!(text.contains("600% · 48M rss"), "{text}");
        assert!(text.contains("20.0G of 32.0G"), "{text}");
        assert!(text.contains("7.50   6.00   4.25"), "{text}");
        let (x, y) = find(&buffer, "agents").unwrap();
        assert_eq!(buffer[(x + 16, y)].symbol(), "█");
        assert!(text.contains("2.3G  2 procs"), "{text}");
        assert!(text.contains("feature  codex --resume"), "{text}");
        assert!(text.contains("+ 1 more near idle"), "{text}");
        assert!(text.contains("kill 40"), "{text}");
        assert!(text.contains("WindowServer"), "{text}");
        assert!(
            text.contains("Host: box") && text.contains("remote calm"),
            "{text}"
        );
        assert!(!text.contains('\u{2014}') && !text.contains("UTC"));

        // Narrow frames keep the numbers and drop the bars.
        let text = rows(&draw(&mut model, 50, 80)).join("\n");
        assert!(text.contains("800% of 800%"), "{text}");
        assert!(!text.contains("░"), "{text}");

        // Short frames scroll the body under a fixed verdict.
        let _ = draw(&mut model, 120, 20);
        assert!(model.perf_max_scroll > 0);
        model.perf_scroll = usize::MAX;
        let text = rows(&draw(&mut model, 120, 20)).join("\n");
        assert!(text.contains("wt is most of the load"), "{text}");
        assert!(text.contains("remote calm"), "{text}");
        assert_eq!(model.perf_scroll, model.perf_max_scroll);
    }

    #[test]
    fn perf_overlay_without_a_sample_says_so() {
        let mut model = Model {
            show_perf: true,
            ..Model::default()
        };
        let text = rows(&draw(&mut model, 100, 30)).join("\n");
        assert!(text.contains("sampling…"), "{text}");
        model.board = crate::Board {
            perf: vec!["Performance snapshot failed: ps timed out".into()],
            ..Default::default()
        }
        .into();
        let buffer = draw(&mut model, 100, 30);
        let (x, y) = find(&buffer, "Performance snapshot failed").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::ERR);
    }

    #[test]
    fn perf_units_and_counts_read_naturally() {
        assert_eq!(perf_memory(48 * 1024), "48M");
        assert_eq!(perf_memory(2_411_725), "2.3G");
        assert_eq!(perf_count(1, "process"), "1 process");
        assert_eq!(perf_count(2, "proc"), "2 procs");
        assert_eq!(perf_leading_percent("  12% 48 MiB pid 1"), Some(12.0));
    }
}
