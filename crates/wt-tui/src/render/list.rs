//! The worktree list (left pane). Rows are grouped under labeled rules with a
//! blank line between groups. Each row reads left to right: stack rail, the
//! status marker, the title (with an issue number prefix and, for a split
//! stack, where its parent went), then the right-aligned badge cluster.

use ratatui::{
    Frame,
    layout::{Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use unicode_width::UnicodeWidthStr;

use super::text::{fit, truncate_end};
use crate::{
    BoardRow, BoardSection, Model, ReviewRequestRow, badges,
    cleanup::{CleanupMark, CleanupPreview},
    glyphs,
    model::{ARCHIVED_SECTION, ListAnchor, REMOVING_SECTION, VisualItem},
    theme,
};

/// Room kept for a split-stack reference, and the label width it must
/// leave behind: identity beats relationship when there is no room for both.
const SECTION_REF_CELLS: usize = 20;
const SECTION_REF_MIN: usize = 10;
const MIN_LABEL_CELLS: usize = 24;
/// Fewest title cells a row in the Landed block keeps beside what it waits
/// for. That label outranks the title, which is cut to fit; only a pane
/// too narrow for both drops it.
const LANDED_TITLE_MIN_CELLS: usize = 8;
const REF_ARROW: &str = " → ";
/// The deepest rail drawn; a pathological chain cannot eat the title.
const MAX_GUTTER: usize = 3;

/// What a folded header says about its hidden rows: their own markers,
/// counted, plus the two alarms that live in a row's badge cluster.
#[derive(Default)]
struct FoldSummary {
    markers: Vec<(badges::Badge, usize)>,
    conflicted: usize,
    failing_checks: usize,
}

impl FoldSummary {
    fn of(rows: &[&BoardRow], policy: &crate::DisplayPolicy) -> Self {
        Self {
            markers: badges::marker_tally(rows, policy),
            conflicted: rows
                .iter()
                .filter(|row| !row.git.conflict_files.is_empty())
                .count(),
            failing_checks: rows
                .iter()
                .filter(|row| row.pr.as_ref().map(|pr| pr.checks) == Some(crate::CheckState::Fail))
                .count(),
        }
    }
}

/// One drawn line and the cursor stop it represents, if any.
struct ListLine {
    line: Line<'static>,
    item: Option<usize>,
}

pub(crate) fn render(frame: &mut Frame<'_>, model: &mut Model, area: Rect) {
    let block = super::status::pane("worktrees".into(), super::status::LIST, model);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 4 || inner.height == 0 {
        return;
    }
    let lines = build(model, inner.width as usize, inner.height as usize);
    let height = inner.height as usize;
    model.list_height = height;
    model.list_max_offset = lines.len().saturating_sub(height);
    let selected_line = model
        .selected
        .and_then(|selected| lines.iter().position(|line| line.item == Some(selected)));
    // A wheel-scrolled viewport stays put until the cursor moves.
    let follow = if model.list_free_scroll {
        None
    } else {
        selected_line
    };
    let line_of = |item: Option<usize>| {
        item.and_then(|item| lines.iter().position(|line| line.item == Some(item)))
    };
    let mut offset = model.offset;
    // A board that changed on its own is drawn around what the user was
    // looking at: the cursor's line, or the top row of a scrolled viewport.
    if std::mem::take(&mut model.list_reanchor)
        && let Some(anchor) = model.list_anchor.take()
    {
        let line = match &anchor.item {
            None => follow,
            Some(id) => line_of(
                (0..model.item_count())
                    .find(|&position| model.item_id(position).as_ref() == Some(id)),
            ),
        };
        if let Some(line) = line {
            offset = line.saturating_sub(anchor.screen_row);
        }
    }
    offset = scroll_offset(offset, follow, lines.len(), height);
    // An opened section shows as many of its rows as fit below the
    // cursor's usual context, so the next frame has nothing to scroll back.
    if std::mem::take(&mut model.list_reveal)
        && let Some(first) = follow
    {
        let rows = lines[first..]
            .iter()
            .take_while(|line| line.item.is_some())
            .count();
        let top = first.saturating_sub(scroll_margin(height));
        offset = offset.max((first + rows).saturating_sub(height)).min(top);
    }
    model.offset = offset;
    model.list_anchor = match follow {
        Some(line) => (offset..offset + height)
            .contains(&line)
            .then(|| ListAnchor {
                item: None,
                screen_row: line - offset,
            }),
        None => lines
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .find_map(|(line, entry)| {
                Some(ListAnchor {
                    item: Some(model.item_id(entry.item?)?),
                    screen_row: line - offset,
                })
            }),
    };
    let total = lines.len();
    let visible: Vec<_> = lines
        .into_iter()
        .skip(model.offset)
        .take(height)
        .map(|line| line.line)
        .collect();
    frame.render_widget(Paragraph::new(visible), inner);
    if total > height {
        let mut state = ScrollbarState::new(total.saturating_sub(height)).position(model.offset);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .track_style(theme::fg(theme::BORDER))
                .thumb_symbol("┃")
                .thumb_style(theme::fg(theme::FG_DIM)),
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

fn scroll_margin(height: usize) -> usize {
    3.min(height.saturating_sub(1) / 2)
}

/// Keep the selected line in view with three lines of context (vim's
/// scrolloff), and never scroll past the end.
fn scroll_offset(offset: usize, selected: Option<usize>, total: usize, height: usize) -> usize {
    let mut offset = offset;
    if let Some(selected) = selected {
        let margin = scroll_margin(height);
        if selected < offset.saturating_add(margin) {
            offset = selected.saturating_sub(margin);
        } else if selected + margin >= offset.saturating_add(height) {
            offset = (selected + margin + 1).saturating_sub(height);
        }
    }
    offset.min(total.saturating_sub(height))
}

fn build(model: &Model, width: usize, height: usize) -> Vec<ListLine> {
    let board = &model.board;
    let mut lines = Vec::new();
    if model.item_count() == 0 {
        lines.push(blank());
        let message = if super::status::status(model, super::status::LIST).loading
            || matches!(model.source_state, wt_runtime::SourceState::Empty)
        {
            vec![Span::styled(" Loading worktrees…", theme::dim())]
        } else {
            vec![
                Span::styled(" No worktrees. Press ", theme::dim()),
                Span::styled("n", theme::bold(theme::ACCENT)),
                Span::styled(" to create one.", theme::dim()),
            ]
        };
        lines.push(ListLine {
            line: Line::from(message),
            item: None,
        });
        return lines;
    }
    let positions: std::collections::HashMap<VisualItem, usize> = (0..model.item_count())
        .filter_map(|position| model.item(position).map(|item| (item, position)))
        .collect();
    let gutter = board
        .rows
        .iter()
        .map(|row| row.stack_prefix.chars().count())
        .max()
        .unwrap_or(0)
        .min(MAX_GUTTER);
    let context = RowContext {
        model,
        width,
        gutter,
        cleanup: model.cleanup_preview(),
    };
    if board.sections.is_empty() {
        lines.push(blank());
        for index in 0..board.rows.len() {
            let item = positions.get(&VisualItem::Row(index)).copied();
            lines.push(context.row(&board.rows[index], item));
        }
        return lines;
    }
    for (index, section) in board.sections.iter().enumerate() {
        if matches!(section.key.as_str(), ARCHIVED_SECTION | REMOVING_SECTION) {
            continue;
        }
        context.section(&mut lines, index, section, &positions, true);
    }
    let active_lines = lines.len();
    let mut bottom = Vec::new();
    if !board.review_requests.is_empty() {
        if model.reviews_folded {
            let item = positions.get(&VisualItem::ReviewHeader).copied();
            bottom.push(folded_header(
                "Review requests",
                board.review_requests.len(),
                &FoldSummary::default(),
                item == model.selected && item.is_some(),
                width,
                item,
            ));
        } else {
            bottom.push(divider("Review requests", width));
            for (index, review) in board.review_requests.iter().enumerate() {
                let item = positions.get(&VisualItem::ReviewRequest(index)).copied();
                bottom.push(review_row(
                    review,
                    item.is_some() && item == model.selected,
                    width,
                    item,
                ));
            }
        }
    }
    for (index, section) in board.sections.iter().enumerate().filter(|(_, section)| {
        matches!(section.key.as_str(), REMOVING_SECTION | ARCHIVED_SECTION)
            && !section.rows.is_empty()
    }) {
        if !bottom.is_empty() {
            bottom.push(blank());
        }
        context.section(&mut bottom, index, section, &positions, false);
    }
    if !bottom.is_empty() {
        // The requested reviews and the archive sit at the bottom of a short
        // list, and simply follow a long one after a one-line gap.
        let spare = height.saturating_sub(active_lines + bottom.len());
        let gap = if active_lines == 0 { 1 } else { spare.max(1) };
        lines.extend((0..gap).map(|_| blank()));
        lines.extend(bottom);
    }
    lines
}

struct RowContext<'a> {
    model: &'a Model,
    width: usize,
    gutter: usize,
    cleanup: Option<CleanupPreview<'a>>,
}

impl RowContext<'_> {
    fn section(
        &self,
        lines: &mut Vec<ListLine>,
        index: usize,
        section: &BoardSection,
        positions: &std::collections::HashMap<VisualItem, usize>,
        leading_blank: bool,
    ) {
        let rows: Vec<_> = section
            .rows
            .iter()
            .filter_map(|&row| self.model.board.rows.get(row).map(|data| (row, data)))
            .collect();
        if rows.is_empty() && !section.drawn() {
            return;
        }
        if leading_blank {
            lines.push(blank());
        }
        if rows.is_empty() {
            let item = positions.get(&VisualItem::Section(index)).copied();
            // Rows filed here that `[ui] owed_sections` draws in the Landed
            // block: the section is not empty, only shown elsewhere.
            let landed = if section.key.starts_with('\0') {
                0
            } else {
                self.model.board.section_members(&section.key).len()
            };
            let title = if landed == 0 {
                section.title.clone()
            } else {
                format!("{} · {landed} landed, shown below", section.title)
            };
            lines.push(empty_section(
                &title,
                item.is_some() && item == self.model.selected,
                self.width,
                item,
            ));
            return;
        }
        let landed = section.key == crate::LANDED_SECTION;
        let members: Vec<&BoardRow> = rows.iter().map(|(_, row)| *row).collect();
        if section.folded {
            let item = positions.get(&VisualItem::Section(index)).copied();
            let selected = item.is_some() && item == self.model.selected;
            let mut header = if landed {
                folded_landed_header(&section.title, &members, selected, self.width, item)
            } else {
                folded_header(
                    &section.title,
                    rows.len(),
                    &FoldSummary::of(&members, &self.model.board.display),
                    selected,
                    self.width,
                    item,
                )
            };
            // A candidate hidden by a fold must still show.
            if rows
                .iter()
                .any(|(_, row)| self.cleanup_mark(row) == Some(CleanupMark::Remove))
            {
                for span in &mut header.line.spans {
                    span.style = span.style.bg(theme::CLEAN_BG);
                }
            }
            lines.push(header);
            return;
        }
        lines.push(ListLine {
            line: if landed {
                counted_rule(&landed_labels(&section.title, &members), self.width)
            } else {
                rule(&section.title, self.width, false)
            },
            item: None,
        });
        for (row, data) in rows {
            let item = positions.get(&VisualItem::Row(row)).copied();
            lines.push(self.row_in(data, item, landed));
        }
    }

    fn cleanup_mark(&self, row: &BoardRow) -> Option<CleanupMark> {
        self.cleanup.as_ref()?.mark(&self.model.board, row)
    }

    fn row(&self, row: &BoardRow, item: Option<usize>) -> ListLine {
        self.row_in(row, item, false)
    }

    /// `landed`: the row is drawn in the Landed block, where it says on the
    /// right what it waits for next.
    fn row_in(&self, row: &BoardRow, item: Option<usize>, landed: bool) -> ListLine {
        let selected = item.is_some() && item == self.model.selected;
        let policy = &self.model.board.display;
        let mark = self.cleanup_mark(row);
        let background = if mark == Some(CleanupMark::Remove) {
            Some(theme::CLEAN_BG)
        } else {
            selected.then_some(theme::SELECTED_BG)
        };
        let style = |color: Color| {
            let style = Style::new().fg(color);
            background.map_or(style, |bg| style.bg(bg))
        };
        let cluster = badges::cluster(row, policy);
        let pending =
            super::status::pending_glyph(&super::status::row_pending(self.model, &row.key));
        let pending_width = if pending.is_some() { 3 } else { 0 };
        let cluster_width = badges::cluster_width(&cluster) + pending_width;
        let remote = row.host.is_some();
        let remote_cells = if remote { 2 } else { 0 };
        // One cell of padding on each side, the rail gutter, and the
        // three-cell marker slot.
        let budget = self
            .width
            .saturating_sub(2 + self.gutter + 3 + remote_cells + cluster_width);
        let waits = landed
            .then(|| waits_for(row, &self.model.board))
            .flatten()
            .filter(|(text, _)| budget >= text.width() + 1 + LANDED_TITLE_MIN_CELLS);
        let budget = budget.saturating_sub(waits.as_ref().map_or(0, |(text, _)| text.width() + 1));
        let reference_room =
            SECTION_REF_CELLS.min(budget.saturating_sub(MIN_LABEL_CELLS + REF_ARROW.width() + 1));
        let reference = row
            .split_parent_section
            .as_deref()
            .filter(|_| reference_room >= SECTION_REF_MIN)
            .map(|section| format!("{REF_ARROW}{}", truncate_end(section, reference_room)))
            .unwrap_or_default();
        let label_room = budget.saturating_sub(reference.width());
        let label = truncate_end(&row_label(row), label_room);
        let filler = budget.saturating_sub(label.width() + reference.width());

        let mut spans = vec![Span::styled(" ", style(theme::FG))];
        if self.gutter > 0 {
            spans.push(Span::styled(
                fit(&rail(&row.stack_prefix, self.gutter), self.gutter),
                style(theme::lane_color(row.stack_lane)),
            ));
        }
        let marker = badges::marker(row, policy);
        spans.push(Span::styled(
            format!("{}  ", marker.glyph),
            style(marker.color),
        ));
        if remote {
            spans.push(Span::styled(
                format!("{} ", badges::REMOTE_HOST.glyph),
                style(if row.archived {
                    theme::FG_DIM
                } else {
                    badges::REMOTE_HOST.color
                }),
            ));
        }
        let title_color = match (selected, row.archived) {
            (true, false) => theme::FG_BRIGHT,
            (true, true) | (false, false) => theme::FG,
            (false, true) => theme::FG_DIM,
        };
        let mut title_style = style(title_color);
        if row.busy.as_ref().is_some_and(|busy| busy.op == "remove") {
            title_style = title_style
                .fg(theme::FG_DIM)
                .add_modifier(Modifier::CROSSED_OUT);
        } else if mark == Some(CleanupMark::Remove) {
            title_style = title_style
                .fg(theme::FG_BRIGHT)
                .add_modifier(Modifier::BOLD);
        } else if selected {
            title_style = title_style.add_modifier(Modifier::BOLD);
        }
        let title_span = spans.len();
        spans.push(Span::styled(label, title_style));
        if !reference.is_empty() {
            spans.push(Span::styled(reference, style(theme::FG_DIM)));
        }
        spans.push(Span::styled(" ".repeat(filler), style(theme::FG)));
        if let Some((text, color)) = waits {
            spans.push(Span::styled(format!(" {text}"), style(color)));
        }
        if let Some((glyph, color)) = pending {
            spans.push(Span::styled(format!(" {glyph} "), style(color)));
        }
        spans.extend(badges::cluster_spans(&cluster, background));
        spans.push(Span::styled(" ", style(theme::FG)));
        // Kept rows recede so candidates carry the color; the title alone
        // says why a row stays.
        if let Some(mark) = mark.filter(|mark| *mark != CleanupMark::Remove) {
            for (index, span) in spans.iter_mut().enumerate() {
                let color = if index == title_span {
                    mark.color()
                } else {
                    theme::FG_DIM
                };
                span.style = span.style.fg(color);
            }
        }
        ListLine {
            line: Line::from(spans),
            item,
        }
    }
}

/// What a row in the Landed block waits for next, from the block
/// `wt owed --all` lists it under: `due staging`, `production`, a time in
/// the controller's clock (`22:32`, `Fri 18:12`), `other work`, `done`.
fn waits_for(row: &BoardRow, board: &crate::Board) -> Option<(String, Color)> {
    use crate::OwedBlock;
    let block = row.owed_block?;
    // The list is ordered most urgent first: due checks by stage, then due
    // steps.
    let due_stage = row.owes.iter().find(|item| item.due()).and_then(|item| {
        (item.kind == wt_core::OwedKind::Check)
            .then_some(item.stage)
            .flatten()
    });
    let at = row
        .owes
        .iter()
        .filter_map(|item| item.next_at_ms)
        .min()
        .and_then(|at_ms| board.history_times.get(&at_ms))
        .map(|stamp| stamp.terse.as_str());
    let color = match block {
        OwedBlock::Due if row.owes.iter().any(|item| item.overdue) => theme::ERR,
        OwedBlock::Due => theme::WARN,
        _ => theme::FG_DIM,
    };
    Some((block.waits_for(due_stage, at), color))
}

/// How many rows of the Landed block have something due, wait, and are
/// done. The header and the block's details pane both count from here.
pub(crate) fn landed_counts(rows: &[&BoardRow]) -> [usize; 3] {
    use crate::OwedBlock;
    let count = |wanted: fn(OwedBlock) -> bool| {
        rows.iter()
            .filter(|row| row.owed_block.is_some_and(wanted))
            .count()
    };
    [
        count(|block| block == OwedBlock::Due),
        count(|block| !matches!(block, OwedBlock::Due | OwedBlock::Done)),
        count(|block| block == OwedBlock::Done),
    ]
}

/// One form of the Landed header: its pieces and their colours.
type LandedLabel = Vec<(String, Color)>;

fn label_width(label: &LandedLabel) -> usize {
    label.iter().map(|(text, _)| text.width()).sum()
}

/// `label` cut to `room` cells, each piece keeping its colour.
fn cut_label(label: &LandedLabel, room: usize) -> LandedLabel {
    let text: String = label.iter().map(|(text, _)| text.as_str()).collect();
    let mut left = truncate_end(&text, room);
    let mut cut = LandedLabel::new();
    for (piece, color) in label {
        if left.is_empty() {
            break;
        }
        let taken = if left.starts_with(piece.as_str()) {
            piece.clone()
        } else {
            std::mem::take(&mut left)
        };
        left = left.strip_prefix(taken.as_str()).unwrap_or("").to_owned();
        cut.push((taken, *color));
    }
    cut
}

/// The counts as coloured pieces. Only `N due` is loud, in the colour its
/// rows' own `due` labels take; the rest stay as quiet as theirs.
fn landed_count_label(rows: &[&BoardRow], short: bool) -> LandedLabel {
    let due = if rows
        .iter()
        .any(|row| row.owes.iter().any(|item| item.overdue))
    {
        theme::ERR
    } else {
        theme::WARN
    };
    let waiting = if short { "wait" } else { "waiting" };
    let mut label = LandedLabel::new();
    for (count, (word, color)) in landed_counts(rows).into_iter().zip([
        ("due", due),
        (waiting, theme::FG_DIM),
        ("done", theme::FG_DIM),
    ]) {
        if count == 0 {
            continue;
        }
        if !label.is_empty() {
            label.push((" · ".to_owned(), theme::FG_DIM));
        }
        label.push((format!("{count} {word}"), color));
    }
    label
}

/// The open Landed header from the fullest form to the shortest:
/// `Landed · 3 due · 24 waiting · 2 done`, the same with `wait`, and the
/// counts alone. It gives up its rule and then its words before it cuts a
/// count.
fn landed_labels(title: &str, rows: &[&BoardRow]) -> Vec<LandedLabel> {
    let named = |short| {
        let counts = landed_count_label(rows, short);
        let mut label = vec![(title.to_owned(), theme::FG_DIM)];
        if !counts.is_empty() {
            label.push((" · ".to_owned(), theme::FG_DIM));
            label.extend(counts);
        }
        label
    };
    let short = landed_count_label(rows, true);
    if short.is_empty() {
        return vec![named(false)];
    }
    vec![named(false), named(true), short]
}

/// The row's title exactly as named. Issue identity, removal progress and
/// other status belong to the marker, badges and details, never the title.
pub(crate) fn row_label(row: &BoardRow) -> String {
    row.title.clone()
}

/// Keep the trailing (deepest) columns of a rail that is wider than the
/// shared gutter.
fn rail(prefix: &str, gutter: usize) -> String {
    let chars: Vec<char> = prefix.chars().collect();
    chars[chars.len().saturating_sub(gutter)..].iter().collect()
}

fn blank() -> ListLine {
    ListLine {
        line: Line::default(),
        item: None,
    }
}

/// `── Label ──────`: a quiet rule with a dim label, or under the cursor
/// the selection fill with a bright bold label.
fn rule(label: &str, width: usize, selected: bool) -> Line<'static> {
    // One cell of right margin past the leading space, like the TS rule,
    // which stops short of the scrollbar gutter.
    let inner = width.saturating_sub(3);
    let label = truncate_end(&format!(" {label} "), inner.saturating_sub(4));
    let trail = inner.saturating_sub(2 + label.width());
    let fill = |style: Style| {
        if selected {
            style.bg(theme::SELECTED_BG)
        } else {
            style
        }
    };
    let dashes = fill(theme::fg(theme::BORDER_DIM));
    let label_style = if selected {
        theme::bold(theme::FG_BRIGHT)
    } else {
        theme::dim()
    };
    Line::from(vec![
        Span::styled(" ", fill(Style::new())),
        Span::styled("──", dashes),
        Span::styled(label, fill(label_style)),
        Span::styled("─".repeat(trail), dashes),
    ])
}

/// A rule whose label must not be cut: `labels` from the fullest form to
/// the shortest. Each is tried with the whole rule, then with one leading
/// dash, then with none, before the next shorter one. Only a pane narrower
/// than the shortest form cuts it.
fn counted_rule(labels: &[LandedLabel], width: usize) -> Line<'static> {
    let body = width.saturating_sub(3);
    let dashes = theme::fg(theme::BORDER_DIM);
    let line = |lead: usize, pad: (&str, &str), label: &LandedLabel, trail: usize| {
        let mut spans = vec![
            Span::raw(" "),
            Span::styled("─".repeat(lead), dashes),
            Span::raw(pad.0.to_owned()),
        ];
        spans.extend(
            label
                .iter()
                .map(|(text, color)| Span::styled(text.clone(), theme::fg(*color))),
        );
        spans.push(Span::raw(pad.1.to_owned()));
        spans.push(Span::styled("─".repeat(trail), dashes));
        Line::from(spans)
    };
    for label in labels {
        let cells = label_width(label);
        if cells + 4 <= body {
            return line(2, (" ", " "), label, body - cells - 4);
        }
        if cells + 2 <= body {
            return line(1, (" ", ""), label, 0);
        }
        if cells <= body {
            return line(0, ("", ""), label, 0);
        }
    }
    let shortest = labels.last().cloned().unwrap_or_default();
    line(0, ("", ""), &cut_label(&shortest, body), 0)
}

fn divider(label: &str, width: usize) -> ListLine {
    ListLine {
        line: rule(label, width, false),
        item: None,
    }
}

/// A named section with no rows: its rule alone, which is also its one
/// cursor stop, so it can be renamed, deleted, moved, or filled.
fn empty_section(label: &str, selected: bool, width: usize, item: Option<usize>) -> ListLine {
    ListLine {
        line: rule(label, width, selected),
        item,
    }
}

/// A folded group collapsed to one stop: a `[×NN]` count chip, the label,
/// and on the right the hidden rows' markers, counted, so folding a section
/// never hides that something inside needs attention.
fn folded_header(
    label: &str,
    count: usize,
    fold: &FoldSummary,
    selected: bool,
    width: usize,
    item: Option<usize>,
) -> ListLine {
    let background = selected.then_some(theme::SELECTED_BG);
    let style = |color: Color| {
        let style = Style::new().fg(color);
        background.map_or(style, |bg| style.bg(bg))
    };
    let modifier = if selected {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };
    let chip = format!("[×{count:02}] ");
    let mut states: Vec<Span<'static>> = fold
        .markers
        .iter()
        .map(|(badge, count)| Span::styled(format!("{} {count} ", badge.glyph), style(badge.color)))
        .collect();
    let mut alarms: Vec<Span<'static>> = Vec::new();
    if fold.conflicted > 0 {
        alarms.push(Span::styled(
            format!("{} {} ", glyphs::CONFLICT, fold.conflicted),
            style(theme::ERR),
        ));
    }
    if fold.failing_checks > 0 {
        alarms.push(Span::styled(
            format!("{} {} ", glyphs::CHECK_FAIL, fold.failing_checks),
            style(theme::ERR),
        ));
    }
    // The name wins over the counts: the least urgent state counts go first
    // when the name would otherwise be cut; alarms always stay.
    let cells =
        |spans: &[Span<'static>]| spans.iter().map(|span| span.content.width()).sum::<usize>();
    while !states.is_empty()
        && width.saturating_sub(2 + chip.width() + cells(&states) + cells(&alarms)) < label.width()
    {
        states.pop();
    }
    let mut right = states;
    right.extend(alarms);
    let right_width: usize = right.iter().map(|span| span.content.width()).sum();
    let room = width.saturating_sub(2 + chip.width() + right_width);
    let label = truncate_end(label, room);
    let filler = room.saturating_sub(label.width());
    let mut spans = vec![
        Span::styled(" ", style(theme::FG)),
        Span::styled(chip, style(theme::ACCENT).add_modifier(modifier)),
        Span::styled(
            label,
            style(if selected {
                theme::FG_BRIGHT
            } else {
                theme::FG_DIM
            })
            .add_modifier(modifier),
        ),
        Span::styled(" ".repeat(filler), style(theme::FG)),
    ];
    spans.extend(right);
    spans.push(Span::styled(" ", style(theme::FG)));
    ListLine {
        line: Line::from(spans),
        item,
    }
}

/// The folded Landed block: the count chip, its name, and on the right
/// the same counts its open header carries. Its rows' markers say only what
/// the counts already do, so it shows no marker tally. The name goes before
/// a count is cut.
fn folded_landed_header(
    title: &str,
    rows: &[&BoardRow],
    selected: bool,
    width: usize,
    item: Option<usize>,
) -> ListLine {
    let background = selected.then_some(theme::SELECTED_BG);
    let style = |color: Color| {
        let style = Style::new().fg(color);
        background.map_or(style, |bg| style.bg(bg))
    };
    let modifier = if selected {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };
    let chip = format!("[×{:02}] ", rows.len());
    let room = width.saturating_sub(2 + chip.width());
    let name_cells = title.width();
    let (named, counts) = [false, true]
        .into_iter()
        .map(|short| landed_count_label(rows, short))
        .find(|counts| counts.is_empty() || name_cells + 2 + label_width(counts) <= room)
        .map_or_else(
            || (false, landed_count_label(rows, true)),
            |counts| (true, counts),
        );
    let mut spans = vec![
        Span::styled(" ", style(theme::FG)),
        Span::styled(chip, style(theme::ACCENT).add_modifier(modifier)),
    ];
    let count_spans = |spans: &mut Vec<Span<'static>>| {
        spans.extend(
            counts
                .iter()
                .map(|(text, color)| Span::styled(text.clone(), style(*color))),
        );
    };
    if named {
        let name = truncate_end(title, room);
        let filler = room.saturating_sub(name.width() + label_width(&counts));
        spans.push(Span::styled(
            name,
            style(if selected {
                theme::FG_BRIGHT
            } else {
                theme::FG_DIM
            })
            .add_modifier(modifier),
        ));
        spans.push(Span::styled(" ".repeat(filler), style(theme::FG)));
        count_spans(&mut spans);
    } else {
        let cells = label_width(&counts);
        if cells <= room {
            count_spans(&mut spans);
            spans.push(Span::styled(" ".repeat(room - cells), style(theme::FG)));
        } else {
            spans.extend(
                cut_label(&counts, room)
                    .into_iter()
                    .map(|(text, color)| Span::styled(text, style(color))),
            );
        }
    }
    spans.push(Span::styled(" ", style(theme::FG)));
    ListLine {
        line: Line::from(spans),
        item,
    }
}

/// A PR someone asked the user to review: its PR glyph and title, with the
/// CI glyph on the right when checks are reporting.
fn review_row(
    review: &ReviewRequestRow,
    selected: bool,
    width: usize,
    item: Option<usize>,
) -> ListLine {
    let background = selected.then_some(theme::SELECTED_BG);
    let style = |color: Color| {
        let style = Style::new().fg(color);
        background.map_or(style, |bg| style.bg(bg))
    };
    let (glyph, color) = if review.draft {
        (glyphs::PR_DRAFT, theme::FG_DIM)
    } else {
        (glyphs::PR_OPEN, theme::ACCENT_ALT)
    };
    let check = badges::check_badge(review.checks);
    let right = if check.is_some() { 4 } else { 0 };
    let room = width.saturating_sub(2 + 3 + right);
    let mut title = review.title.clone();
    if let Some(first) = title.get(..1) {
        title = first.to_uppercase() + &title[1..];
    }
    let label = truncate_end(&title, room);
    let filler = room.saturating_sub(label.width());
    let mut title_style = style(if selected {
        theme::FG_BRIGHT
    } else {
        theme::FG
    });
    if selected {
        title_style = title_style.add_modifier(Modifier::BOLD);
    }
    let mut spans = vec![
        Span::styled(" ", style(theme::FG)),
        Span::styled(format!("{glyph}  "), style(color)),
        Span::styled(label, title_style),
        Span::styled(" ".repeat(filler), style(theme::FG)),
    ];
    if let Some(check) = check {
        spans.push(Span::styled("  ", style(theme::FG)));
        spans.push(Span::styled(
            format!("{} ", check.glyph),
            style(check.color),
        ));
    }
    spans.push(Span::styled(" ", style(theme::FG)));
    ListLine {
        line: Line::from(spans),
        item,
    }
}

#[cfg(test)]
mod scroll_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_named_section_is_one_selectable_rule() {
        let model = Model::default();
        let context = RowContext {
            model: &model,
            width: 60,
            gutter: 0,
            cleanup: None,
        };
        let text = |line: &ListLine| {
            line.line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let positions = std::collections::HashMap::from([(VisualItem::Section(0), 0)]);
        let named = BoardSection {
            key: "Later".into(),
            title: "Later".into(),
            ..Default::default()
        };
        let mut lines = Vec::new();
        context.section(&mut lines, 0, &named, &positions, false);
        assert_eq!(lines.len(), 1);
        assert!(text(&lines[0]).contains("── Later ─"));
        assert_eq!(lines[0].item, Some(0));
        assert!(
            lines[0]
                .line
                .spans
                .iter()
                .all(|span| span.style.bg.is_none())
        );
        let selected = empty_section("Later", true, 60, Some(0));
        assert_eq!(text(&selected), text(&lines[0]));
        for span in &selected.line.spans {
            assert_eq!(span.style.bg, Some(theme::SELECTED_BG), "{span:?}");
            let label = span.content.contains("Later");
            assert_eq!(
                span.style.fg == Some(theme::FG_BRIGHT)
                    && span.style.add_modifier.contains(Modifier::BOLD),
                label,
                "{span:?}"
            );
        }
        // Rows filed in it that `[ui] owed_sections` draws in a derived
        // block: the header says they are there.
        let mut filed = Model::default();
        std::sync::Arc::make_mut(&mut filed.board).rows = vec![
            BoardRow {
                key: "one".into(),
                filed_in: Some("Later".into()),
                ..Default::default()
            },
            BoardRow {
                key: "two".into(),
                filed_in: Some("Elsewhere".into()),
                ..Default::default()
            },
        ];
        let context = RowContext {
            model: &filed,
            width: 60,
            gutter: 0,
            cleanup: None,
        };
        let mut lines = Vec::new();
        context.section(&mut lines, 0, &named, &positions, false);
        assert!(
            text(&lines[0]).contains("── Later · 1 landed, shown below ─"),
            "{}",
            text(&lines[0])
        );
        let context = RowContext {
            model: &model,
            width: 60,
            gutter: 0,
            cleanup: None,
        };
        // A derived group without rows (the Inbox) is not drawn.
        let inbox = BoardSection {
            key: "\0inbox".into(),
            title: "Inbox".into(),
            ..Default::default()
        };
        let mut lines = Vec::new();
        context.section(&mut lines, 0, &inbox, &positions, false);
        assert!(lines.is_empty());
    }

    #[test]
    fn pending_removal_title_is_dim_and_struck_through_even_when_selected() {
        let model = Model::default();
        let context = RowContext {
            model: &model,
            width: 60,
            gutter: 0,
            cleanup: None,
        };
        let mut row = BoardRow {
            title: "Pending deletion".into(),
            busy: Some(crate::BusyView {
                op: "remove".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        for selection in [None, Some(0)] {
            let rendered = context.row(&row, selection);
            let title = rendered
                .line
                .spans
                .iter()
                .find(|span| span.content == "Pending deletion")
                .unwrap();
            assert_eq!(title.style.fg, Some(theme::FG_DIM));
            assert!(title.style.add_modifier.contains(Modifier::CROSSED_OUT));
        }
        row.busy = None;
        let rendered = context.row(&row, None);
        assert!(
            rendered
                .line
                .spans
                .iter()
                .all(|span| !span.style.add_modifier.contains(Modifier::CROSSED_OUT))
        );
    }

    #[test]
    fn initializing_rows_wear_the_hourglass_marker_until_ready() {
        let model = Model::default();
        let context = RowContext {
            model: &model,
            width: 60,
            gutter: 0,
            cleanup: None,
        };
        let marker = |row: &BoardRow| context.row(row, None).line.spans[1].clone();
        let creating = BoardRow {
            title: "New task".into(),
            pending_creation: true,
            busy: Some(crate::BusyView {
                op: "create".into(),
                label: "creating".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut installing = BoardRow {
            title: "New task".into(),
            installing: true,
            ..Default::default()
        };
        for row in [&creating, &installing] {
            let span = marker(row);
            assert_eq!(span.content, format!("{}  ", crate::glyphs::HOURGLASS));
            assert_eq!(span.style.fg, Some(theme::WARN));
        }
        // Ready for a session: the ordinary work marker returns.
        installing.installing = false;
        let span = marker(&installing);
        assert_eq!(span.content, format!("{}  ", crate::glyphs::DOT_OUTLINE));
        assert_eq!(span.style.fg, Some(theme::FG_DIM));
    }

    #[test]
    fn scrolloff_keeps_context_and_clamps_to_the_end() {
        assert_eq!(scroll_offset(0, Some(2), 40, 10), 0);
        assert_eq!(scroll_offset(0, Some(8), 40, 10), 2);
        assert_eq!(scroll_offset(20, Some(21), 40, 10), 18);
        assert_eq!(scroll_offset(35, None, 40, 10), 30);
    }

    #[test]
    fn archive_uses_bottom_space_without_changing_selection() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut model = Model::default();
        model.apply(wt_runtime::SourceSnapshot::test_live(
            Some(crate::Board {
                rows: vec![
                    BoardRow {
                        key: "one".into(),
                        title: "Active task".into(),
                        ..Default::default()
                    },
                    BoardRow {
                        key: "old".into(),
                        title: "Old task".into(),
                        archived: true,
                        ..Default::default()
                    },
                ],
                sections: vec![
                    crate::BoardSection {
                        key: "\0inbox".into(),
                        title: "Inbox".into(),
                        rows: vec![0],
                        ..Default::default()
                    },
                    crate::BoardSection {
                        key: ARCHIVED_SECTION.into(),
                        title: "Archived".into(),
                        rows: vec![1],
                        folded: true,
                    },
                ],
                ..Default::default()
            }),
            wt_runtime::SourceState::Ready,
            wt_core::Domain::Git,
        ));
        let selected = model.selected;
        for height in [14u16, 7] {
            let mut terminal = Terminal::new(TestBackend::new(50, height)).unwrap();
            terminal
                .draw(|frame| render(frame, &mut model, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let bottom: String = (0..50).map(|x| buffer[(x, height - 2)].symbol()).collect();
            assert!(bottom.contains("Archived"), "{height}: {bottom:?}");
            assert_eq!(model.selected, selected);
        }
    }

    #[test]
    fn the_open_cleanup_menu_fills_candidates_and_colors_kept_rows_by_reason() {
        let landed = |key: &str| BoardRow {
            key: key.into(),
            slug: key.into(),
            title: key.into(),
            path: format!("/worktrees/{key}"),
            git: crate::GitPresentation {
                head_sha: Some("head".into()),
                landed_on: Some(crate::LandingKind::Base),
                tracked_changes: Some(0),
                untracked_files: Some(0),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut dirty = landed("dirty");
        dirty.git.untracked_files = Some(1);
        let mut unlanded = landed("unlanded");
        unlanded.git.landed_on = None;
        // Dropped work that never landed is kept for a reason worth showing;
        // work that is simply unfinished is none of the menu's business.
        let mut dropped = landed("dropped");
        dropped.git.landed_on = None;
        dropped.work = Some(crate::WorkPresentation {
            effective_state: Some(wt_core::WorkState::Dropped),
            ..Default::default()
        });
        let board = crate::Board {
            rows: vec![landed("done"), dirty, unlanded, dropped],
            ..Default::default()
        };
        let model = Model {
            board: board.clone().into(),
            ..Default::default()
        };
        let preview = CleanupPreview::new(["done"]);
        let context = RowContext {
            model: &model,
            width: 60,
            gutter: 0,
            cleanup: Some(preview),
        };
        let title = |row: usize| {
            let line = context.row(&board.rows[row], None).line;
            let name = board.rows[row].title.as_str();
            let span = line.spans.iter().find(|span| span.content.contains(name));
            (line.clone(), span.unwrap().style)
        };
        let (done, style) = title(0);
        assert!(
            done.spans
                .iter()
                .all(|span| span.style.bg == Some(theme::CLEAN_BG))
        );
        assert_eq!(style.fg, Some(theme::FG_BRIGHT));
        let (held, style) = title(1);
        assert_eq!(style.fg, Some(theme::WARN));
        assert!(held.spans.iter().all(|span| span.style.bg.is_none()));
        // It will never land, so removing it is the person's move.
        assert_eq!(title(3).1.fg, Some(theme::WARN));
        // Left out of the menu, so drawn as it is with no menu open.
        let plain = RowContext {
            model: &model,
            width: 60,
            gutter: 0,
            cleanup: None,
        };
        assert_eq!(
            context.row(&board.rows[2], None).line,
            plain.row(&board.rows[2], None).line
        );
    }

    #[test]
    fn removing_and_failed_rows_render_above_archive_in_cursor_order() {
        let rows = vec![
            BoardRow {
                key: "active".into(),
                title: "Active task".into(),
                ..Default::default()
            },
            BoardRow {
                key: "running".into(),
                title: "Running task".into(),
                removal: Some(crate::RemovalView::default()),
                ..Default::default()
            },
            BoardRow {
                key: "failed".into(),
                title: "Failed task".into(),
                removal: Some(crate::RemovalView {
                    error: Some("denied".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            BoardRow {
                key: "old".into(),
                title: "Old task".into(),
                archived: true,
                ..Default::default()
            },
        ];
        let sections = vec![
            BoardSection {
                key: "\0inbox".into(),
                title: "Inbox".into(),
                rows: vec![0],
                ..Default::default()
            },
            BoardSection {
                key: REMOVING_SECTION.into(),
                title: "Removing".into(),
                rows: vec![1, 2],
                ..Default::default()
            },
            BoardSection {
                key: ARCHIVED_SECTION.into(),
                title: "Archived".into(),
                rows: vec![3],
                ..Default::default()
            },
        ];
        let mut model = Model::default();
        model.apply(wt_runtime::SourceSnapshot::test_live(
            Some(crate::Board {
                rows,
                sections,
                ..Default::default()
            }),
            wt_runtime::SourceState::Ready,
            wt_core::Domain::Metadata,
        ));
        let lines = build(&model, 80, 20);
        let text: Vec<_> = lines.iter().map(|line| line.line.to_string()).collect();
        let position = |needle: &str| text.iter().position(|line| line.contains(needle)).unwrap();
        assert!(position("Active task") < position("Running task"));
        assert!(position("Running task") < position("Failed task"));
        assert!(position("Failed task") < position("Archived"));
        assert!(!text[position("Failed task")].contains("removal failed"));
        assert!(text[position("Failed task")].contains(crate::glyphs::CONFLICT));
        assert!(text[position("Running task")].contains(crate::glyphs::TRASH));
        let cursor_items: Vec<_> = lines.iter().filter_map(|line| line.item).collect();
        assert_eq!(cursor_items, [0, 1, 2, 3]);
    }

    #[test]
    fn pending_status_never_replaces_the_title_with_text() {
        use wt_runtime::{PendingPhase, PendingView, Target};
        for phase in [
            PendingPhase::InFlight,
            PendingPhase::Accepted,
            PendingPhase::Unconfirmed,
        ] {
            let row = BoardRow {
                key: "auth-proxy-endpoints".into(),
                title: "auth-proxy-endpoints".into(),
                badge: "legacy status must not render".into(),
                busy: Some(crate::BusyView {
                    label: "busy text must not render".into(),
                    op: "restack".into(),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let model = Model {
                board: std::sync::Arc::new(crate::Board {
                    pending_views: vec![
                        PendingView {
                            target: Target::row(None, &row.key),
                            label: "arming merge (unconfirmed) very long status message".repeat(10),
                            phase,
                        };
                        10
                    ],
                    ..Default::default()
                }),
                ..Default::default()
            };
            for width in [32, 40, 52] {
                let context = RowContext {
                    model: &model,
                    width,
                    gutter: 0,
                    cleanup: None,
                };
                let rendered = context.row(&row, Some(0));
                let text: String = rendered
                    .line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                assert!(text.contains(&row.title), "{text}");
                assert_eq!(text.width(), width);
                assert!(
                    !text.contains("arming")
                        && !text.contains("unconfirmed")
                        && !text.contains("text"),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn labels_are_the_title_without_identity_or_status() {
        let row = BoardRow {
            title: "Fix the thing".into(),
            issue_id: Some("ENG-1234".into()),
            ..Default::default()
        };
        assert_eq!(row_label(&row), "Fix the thing");
        let incidental = BoardRow {
            title: "Christian meetings bugs".into(),
            issue_id: Some("2".into()),
            removal: Some(crate::RemovalView {
                error: Some("denied".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(row_label(&incidental), "Christian meetings bugs");
        let plain = BoardRow {
            title: "Fix".into(),
            ..Default::default()
        };
        assert_eq!(row_label(&plain), "Fix");
    }

    /// A landed row as the owed overlay files it.
    fn landed_row(key: &str, block: crate::OwedBlock, owes: Vec<crate::OwedView>) -> BoardRow {
        BoardRow {
            key: key.into(),
            slug: key.into(),
            title: format!("Title of {key}"),
            git: crate::GitPresentation {
                landed_on: Some(crate::LandingKind::Base),
                ..Default::default()
            },
            owed_phase: Some(match block {
                crate::OwedBlock::Due => wt_core::OwedPhase::Due,
                crate::OwedBlock::Done => wt_core::OwedPhase::Done,
                _ => wt_core::OwedPhase::Waiting,
            }),
            owed_block: Some(block),
            owes,
            ..Default::default()
        }
    }

    fn landed_board() -> crate::Board {
        use crate::{OwedBlock, OwedView};
        use wt_core::{OwedKind, OwedState, VerifyStage};
        let item = |kind, state, stage: Option<VerifyStage>| {
            let mut item = OwedView::new(kind, state, "x");
            item.stage = stage;
            item
        };
        let check = |state, stage| item(OwedKind::Check, state, Some(stage));
        let mut timed = item(OwedKind::Step, OwedState::Waiting, None);
        timed.next_at_ms = Some(2_000);
        let mut later = timed.clone();
        later.next_at_ms = Some(9_000);
        let mut overdue = check(OwedState::Due, VerifyStage::Production);
        overdue.overdue = true;
        let rows = vec![
            landed_row(
                "a",
                OwedBlock::Due,
                vec![
                    check(OwedState::Due, VerifyStage::Staging),
                    check(OwedState::Waiting, VerifyStage::Production),
                ],
            ),
            landed_row("b", OwedBlock::Due, vec![overdue]),
            landed_row(
                "c",
                OwedBlock::Due,
                vec![item(OwedKind::Step, OwedState::Due, None)],
            ),
            landed_row(
                "d",
                OwedBlock::WaitingStaging,
                vec![check(OwedState::Waiting, VerifyStage::Staging)],
            ),
            landed_row(
                "e",
                OwedBlock::WaitingProduction,
                vec![check(OwedState::Waiting, VerifyStage::Production)],
            ),
            landed_row("f", OwedBlock::Scheduled, vec![later, timed]),
            landed_row(
                "g",
                OwedBlock::WaitingOther,
                vec![item(OwedKind::Step, OwedState::Waiting, None)],
            ),
            landed_row("h", OwedBlock::Done, Vec::new()),
        ];
        let mut board = crate::Board {
            sections: vec![BoardSection {
                key: crate::LANDED_SECTION.into(),
                title: crate::LANDED_TITLE.into(),
                rows: (0..rows.len()).collect(),
                folded: false,
            }],
            rows,
            ..Default::default()
        };
        board.display.owed_sections = true;
        board.history_times.insert(
            2_000,
            crate::LocalStamp {
                terse: "Fri 18:12".into(),
                ..Default::default()
            },
        );
        board
    }

    fn drawn(board: &crate::Board, width: usize) -> Vec<ListLine> {
        let mut model = Model::default();
        model.apply(wt_runtime::SourceSnapshot::test_live(
            Some(board.clone()),
            wt_runtime::SourceState::Ready,
            wt_core::Domain::Git,
        ));
        build(&model, width, 40)
            .into_iter()
            .filter(|line| !line.line.spans.is_empty())
            .collect()
    }

    fn plain(line: &ListLine) -> String {
        line.line.to_string().trim_end().to_owned()
    }

    /// What a landed row waits for outranks its title: a long title is cut
    /// to make room, down to a pane with no room for both.
    #[test]
    fn a_landed_rows_label_stays_and_a_long_title_is_cut_for_it() {
        let mut board = landed_board();
        for row in &mut board.rows {
            row.title = format!(
                "{} with a title far too long for a narrow list pane",
                row.key
            );
        }
        let labels = [
            "due staging",
            "due production",
            "due",
            "staging",
            "production",
            "Fri 18:12",
            "other work",
            "done",
        ];
        for width in [30, 35, 50] {
            let lines = drawn(&board, width);
            for (line, label) in lines[1..].iter().zip(labels) {
                let text = plain(line);
                assert!(text.contains(&format!("… {label}")), "{width}: {text}");
                assert!(line.line.width() <= width, "{width}: {text}");
            }
        }
        // Eight title cells and the label, or the title alone.
        let room = |width| plain(&drawn(&board, width)[1]);
        let fits = (10..40)
            .find(|width| room(*width).contains("due staging"))
            .unwrap();
        let kept = room(fits);
        let title = kept
            .trim_start()
            .split('…')
            .next()
            .unwrap()
            .trim_start_matches(|c: char| !c.is_alphanumeric());
        assert_eq!(title.width() + 1, LANDED_TITLE_MIN_CELLS, "{kept}");
        assert!(!room(fits - 1).contains("due"), "{}", room(fits - 1));
    }

    /// The Landed header gives up its rule, then says `wait`, and folded
    /// then drops its name, before any count is cut. Folded, the counts sit
    /// at the right edge and there is no marker tally.
    #[test]
    fn the_landed_header_never_cuts_a_count() {
        let board = landed_board();
        let mut folded = board.clone();
        folded.sections[0].folded = true;
        let header = |board: &crate::Board, width| plain(&drawn(board, width)[0]);
        assert_eq!(
            header(&board, 44).trim_end(),
            " ── Landed · 3 due · 4 waiting · 1 done ──"
        );
        assert_eq!(
            header(&board, 40).trim_end(),
            " ─ Landed · 3 due · 4 waiting · 1 done"
        );
        assert_eq!(
            header(&board, 38).trim_end(),
            " Landed · 3 due · 4 waiting · 1 done"
        );
        assert_eq!(
            header(&board, 37).trim_end(),
            " ─ Landed · 3 due · 4 wait · 1 done"
        );
        assert_eq!(
            header(&board, 35).trim_end(),
            " Landed · 3 due · 4 wait · 1 done"
        );
        assert_eq!(
            header(&folded, 50).trim_end(),
            " [×08] Landed          3 due · 4 waiting · 1 done"
        );
        assert_eq!(
            header(&folded, 43).trim_end(),
            " [×08] Landed   3 due · 4 waiting · 1 done"
        );
        assert_eq!(
            header(&folded, 42).trim_end(),
            " [×08] Landed  3 due · 4 waiting · 1 done"
        );
        assert_eq!(
            header(&folded, 41).trim_end(),
            " [×08] Landed    3 due · 4 wait · 1 done"
        );
        assert_eq!(
            header(&folded, 39).trim_end(),
            " [×08] Landed  3 due · 4 wait · 1 done"
        );
        for width in [38, 35] {
            assert_eq!(
                header(&folded, width).trim_end(),
                " [×08] 3 due · 4 wait · 1 done"
            );
        }
        for width in 32..=120 {
            for board in [&board, &folded] {
                let text = header(board, width);
                assert!(!text.contains('…'), "{width}: {text}");
                assert!(text.contains("3 due · 4 wait"), "{width}: {text}");
                assert!(text.contains("1 done"), "{width}: {text}");
                assert!(text.width() <= width, "{width}: {text}");
            }
        }
    }

    #[test]
    fn the_landed_block_counts_its_rows_and_each_says_what_it_waits_for() {
        let board = landed_board();
        for width in [50, 70, 120] {
            let lines = drawn(&board, width);
            let text: Vec<String> = lines.iter().map(plain).collect();
            assert!(
                text[0].starts_with(" ── Landed · 3 due · 4 waiting · 1 done ─"),
                "{text:#?}"
            );
            let labels = [
                "due staging",
                "due production",
                "due",
                "staging",
                "production",
                "Fri 18:12",
                "other work",
                "done",
            ];
            for (line, label) in lines[1..].iter().zip(labels) {
                let spans = &line.line.spans;
                let at = spans
                    .iter()
                    .position(|span| span.content.trim() == label)
                    .unwrap_or_else(|| panic!("{label}: {}", plain(line)));
                // Right-aligned: only the glyph cluster and the margin follow.
                assert!(spans[at - 1].content.trim().is_empty(), "{}", plain(line));
                let color = match label {
                    "due production" => theme::ERR,
                    "due staging" | "due" => theme::WARN,
                    _ => theme::FG_DIM,
                };
                assert_eq!(spans[at].style.fg, Some(color), "{label}");
                assert!(line.line.width() <= width, "{}", plain(line));
            }
            // Every label ends in the same column.
            let ends: std::collections::BTreeSet<usize> = lines[1..]
                .iter()
                .zip(labels)
                .map(|(line, label)| {
                    let text = line.line.to_string();
                    text.find(label)
                        .map(|at| text[..at].width() + label.width())
                        .unwrap()
                })
                .collect();
            assert_eq!(ends.len(), 1, "{text:#?}");
        }
        // A label gives way to a title that needs the room.
        let narrow = drawn(&board, 24);
        assert!(
            !plain(&narrow[1]).contains("due staging"),
            "{}",
            plain(&narrow[1])
        );
        // Zero counts are left out.
        let mut some = board.clone();
        some.rows.truncate(3);
        some.sections[0].rows = vec![0, 1, 2];
        assert!(plain(&drawn(&some, 70)[0]).starts_with(" ── Landed · 3 due ─"));
        // Folded, the header carries the same counts.
        let mut folded = board.clone();
        folded.sections[0].folded = true;
        let lines = drawn(&folded, 70);
        assert_eq!(lines.len(), 1);
        assert!(
            plain(&lines[0])
                .trim_end()
                .ends_with("  3 due · 4 waiting · 1 done")
                && plain(&lines[0]).starts_with(" [×08] Landed  "),
            "{}",
            plain(&lines[0])
        );
        // Outside the block a row says nothing of the kind.
        let mut filed = board;
        filed.sections[0].key = "Batch".into();
        filed.sections[0].title = "Batch".into();
        let lines = drawn(&filed, 70);
        assert!(plain(&lines[0]).starts_with(" ── Batch ─"));
        assert!(lines[1..].iter().all(|line| !plain(line).contains("due")));
    }

    #[test]
    fn deep_rails_keep_their_trailing_columns() {
        assert_eq!(rail("││└", 2), "│└");
        assert_eq!(rail("┌", 3), "┌");
    }

    #[test]
    fn section_rules_stop_one_cell_short_of_the_scrollbar_gutter() {
        let line = divider("Active", 40);
        let cells: usize = line
            .line
            .spans
            .iter()
            .map(|span| span.content.width())
            .sum();
        assert_eq!(cells, 38);
    }

    fn state_row(state: wt_core::WorkState) -> BoardRow {
        BoardRow {
            work: Some(crate::WorkPresentation {
                effective_state: Some(state),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn fold_of(rows: &[BoardRow], policy: &crate::DisplayPolicy) -> FoldSummary {
        FoldSummary::of(&rows.iter().collect::<Vec<_>>(), policy)
    }

    #[test]
    fn a_narrow_folded_header_drops_counts_before_its_name() {
        let mut rows = [
            wt_core::WorkState::Ready,
            wt_core::WorkState::NeedsHuman,
            wt_core::WorkState::Working,
            wt_core::WorkState::Review,
        ]
        .map(state_row);
        rows[0].pr = Some(crate::PrPresentation {
            checks: crate::CheckState::Fail,
            ..Default::default()
        });
        let fold = fold_of(&rows, &Default::default());
        let line = folded_header("Statuses", 8, &fold, false, 30, None);
        let text: String = line
            .line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("Statuses"), "{text:?}");
        assert!(
            text.contains(&format!("{} 1", glyphs::CHECK_FAIL)),
            "{text:?}"
        );
    }

    #[test]
    fn folded_header_reads_most_urgent_first() {
        let rows = [
            wt_core::WorkState::Working,
            wt_core::WorkState::NeedsTesting,
            wt_core::WorkState::NeedsHuman,
            wt_core::WorkState::Ready,
        ]
        .map(state_row);
        let fold = fold_of(&rows, &Default::default());
        let line = folded_header("Batch", 4, &fold, false, 60, None);
        let colors = line
            .line
            .spans
            .iter()
            .filter(|span| span.content.ends_with("1 "))
            .map(|span| span.style.fg)
            .collect::<Vec<_>>();
        let expected = [
            wt_core::WorkState::Ready,
            wt_core::WorkState::NeedsHuman,
            wt_core::WorkState::NeedsTesting,
            wt_core::WorkState::Working,
        ]
        .map(|state| Some(badges::work_state_color(state)));
        assert_eq!(colors, expected);
    }

    /// The folded header may only show what its rows show: every counted
    /// glyph is some member's marker, and every member with a coloured
    /// marker is counted once.
    #[test]
    fn a_folded_header_counts_exactly_the_markers_its_rows_wear() {
        let mut rows = Vec::new();
        for state in wt_core::WORK_STATES {
            for variant in 0..7 {
                let mut row = state_row(state);
                let work = row.work.as_mut().unwrap();
                match variant {
                    1 => work.blocked = true,
                    2 => work.stale = Some(true),
                    3 => {
                        let mut check = crate::OwedView::new(
                            wt_core::OwedKind::Check,
                            wt_core::OwedState::Due,
                            "check",
                        );
                        check.overdue = true;
                        row.owes = vec![check];
                        row.owed_phase = Some(wt_core::OwedPhase::Due);
                    }
                    4 => row.git.landed_on = Some(crate::LandingKind::Base),
                    5 => row.git.landed_on = Some(crate::LandingKind::Production),
                    6 => row.archived = true,
                    _ => {}
                }
                rows.push(row);
            }
        }
        rows.push(BoardRow::default());
        for production in [false, true] {
            let policy = crate::DisplayPolicy {
                production,
                ..Default::default()
            };
            let fold = fold_of(&rows, &policy);
            let drawn: Vec<_> = rows
                .iter()
                .map(|row| badges::marker(row, &policy))
                .filter(|badge| badge.color != theme::FG_DIM)
                .collect();
            assert_eq!(
                fold.markers.iter().map(|entry| entry.1).sum::<usize>(),
                drawn.len()
            );
            for (badge, count) in &fold.markers {
                assert_eq!(
                    drawn.iter().filter(|marker| *marker == badge).count(),
                    *count,
                    "{badge:?}"
                );
            }
            let line = folded_header("All", rows.len(), &fold, false, 400, None);
            for (badge, count) in &fold.markers {
                assert!(line.line.spans.iter().any(|span| {
                    span.content == format!("{} {count} ", badge.glyph)
                        && span.style.fg == Some(badge.color)
                }));
            }
        }
    }
}
