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
        if group != current {
            match group {
                Some(group) => rows.push(PickRow::Group(group.to_owned())),
                None => rows.push(PickRow::Spacer),
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
    let opener = match picker.action {
        PickerAction::Actions { .. } => Some("!"),
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
    let outer = modal.place(area, prefix + widest + 2, rows.len(), false);
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
                    spans.push(Span::styled(
                        super::text::truncate_middle(first, room),
                        label_style(is_selected),
                    ));
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
            wrap(line, width.saturating_sub(indent).max(1))
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
        .map_or("n / esc".to_owned(), |key| format!("n / {key} / esc"));
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

fn perf(frame: &mut Frame<'_>, model: &mut Model, area: Rect) {
    let modal = Modal::new(
        "performance · wt and everything downstream",
        vec![
            hint("j/k", "scroll"),
            hint("r", "resample"),
            hint("i", "investigate"),
            hint(
                "c",
                if model.perf_continuous {
                    "continuous: on"
                } else {
                    "continuous: off"
                },
            ),
            hint("P / esc / q", "close"),
        ],
    );
    let outer = modal.place(area, MAX_FRAME_WIDTH as usize, 0, true);
    let body = modal.draw(frame, area, outer);
    let (width, visible) = (body.width as usize, body.height as usize);
    if visible < 2 || width == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::styled(
            truncate_end(
                &format!(
                    "frame {} · last draw {} µs",
                    model.frame_count, model.last_frame_micros
                ),
                width,
            ),
            theme::dim(),
        )),
        Rect { height: 1, ..body },
    );
    let list = Rect {
        y: body.y + 2.min(body.height - 1),
        height: body.height.saturating_sub(2).max(1),
        ..body
    };
    let visible = list.height as usize;
    let lines = &model.board.perf;
    model.perf_scroll = model.perf_scroll.min(lines.len().saturating_sub(visible));
    let rendered = if lines.is_empty() {
        vec![Line::styled("sampling…", theme::dim())]
    } else {
        lines
            .iter()
            .skip(model.perf_scroll)
            .take(visible)
            .map(|line| perf_line(line, width))
            .collect()
    };
    frame.render_widget(Paragraph::new(rendered), list);
    scrollbar(
        frame,
        outer,
        modal.border,
        lines.len() + 2,
        model.perf_scroll,
    );
}

/// Prepared perf lines are prose; give them a hierarchy. Headings get the
/// section bar, headline figures are bright, and a process burning CPU
/// warms from warn to error.
fn perf_line(line: &str, width: usize) -> Line<'static> {
    let indented = line.starts_with(' ');
    if !indented && line.ends_with(':') {
        return section_bar(line.trim_end_matches(':'), width);
    }
    let style = if line.starts_with("Sampled at") || line.contains("unavailable") {
        theme::dim()
    } else if line.starts_with("Orphaned") {
        theme::fg(theme::WARN)
    } else if line.starts_with("No orphaned") {
        theme::fg(theme::OK)
    } else if indented {
        match cpu_percent(line) {
            Some(cpu) if cpu >= 90.0 => theme::fg(theme::ERR),
            Some(cpu) if cpu >= 50.0 => theme::fg(theme::WARN),
            _ => theme::fg(theme::FG_MID),
        }
    } else {
        theme::fg(theme::FG_BRIGHT)
    };
    Line::styled(truncate_end(line, width), style)
}

fn cpu_percent(line: &str) -> Option<f64> {
    let before = &line[..line.find("% CPU")?];
    before.rsplit(' ').next()?.parse().ok()
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
        let (text, cursor) = model.help_query.viewport(room);
        spans.push(Span::styled(fit(&text, room), theme::fg(theme::FG_BRIGHT)));
        spans.push(Span::styled(count, theme::dim()));
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect { height: 1, ..body },
        );
        if model.help_searching {
            frame.set_cursor_position((body.x + 1 + cursor, body.y));
        }
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
        assert!(text.contains("n / d / esc cancel"), "{text}");
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
}
