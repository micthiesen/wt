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
    BoardRow, BoardSection, Model, ReviewRequestRow, SectionRollup, badges, glyphs,
    model::{ARCHIVED_SECTION, REMOVING_SECTION, VisualItem},
    theme,
};

/// Room kept for a split-stack reference, and the label width it must
/// leave behind: identity beats relationship when there is no room for both.
const SECTION_REF_CELLS: usize = 20;
const SECTION_REF_MIN: usize = 10;
const MIN_LABEL_CELLS: usize = 24;
const REF_ARROW: &str = " → ";
/// The deepest rail drawn; a pathological chain cannot eat the title.
const MAX_GUTTER: usize = 3;

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
    let selected_line = model
        .selected
        .and_then(|selected| lines.iter().position(|line| line.item == Some(selected)));
    // A wheel-scrolled viewport stays put until the cursor moves.
    let follow = if model.list_free_scroll {
        None
    } else {
        selected_line
    };
    model.offset = scroll_offset(model.offset, follow, lines.len(), height);
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

/// Keep the selected line in view with three lines of context (vim's
/// scrolloff), and never scroll past the end.
fn scroll_offset(offset: usize, selected: Option<usize>, total: usize, height: usize) -> usize {
    let mut offset = offset;
    if let Some(selected) = selected {
        let margin = 3.min(height.saturating_sub(1) / 2);
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
                "Review Requests",
                board.review_requests.len(),
                None,
                item == model.selected && item.is_some(),
                width,
                item,
            ));
        } else {
            bottom.push(divider("Review Requests", width));
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
        if rows.is_empty() {
            return;
        }
        if leading_blank {
            lines.push(blank());
        }
        if section.folded {
            let item = positions.get(&VisualItem::Section(index)).copied();
            lines.push(folded_header(
                &section.title,
                rows.len(),
                Some(&section.rollup),
                item.is_some() && item == self.model.selected,
                self.width,
                item,
            ));
            return;
        }
        lines.push(divider(&section.title, self.width));
        for (row, data) in rows {
            let item = positions.get(&VisualItem::Row(row)).copied();
            lines.push(self.row(data, item));
        }
    }

    fn row(&self, row: &BoardRow, item: Option<usize>) -> ListLine {
        let selected = item.is_some() && item == self.model.selected;
        let policy = &self.model.board.display;
        let background = selected.then_some(theme::SELECTED_BG);
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
                format!("{} ", glyphs::REMOTE),
                style(if row.archived {
                    theme::FG_DIM
                } else {
                    theme::REMOTE
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
        } else if selected {
            title_style = title_style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(label, title_style));
        if !reference.is_empty() {
            spans.push(Span::styled(reference, style(theme::FG_DIM)));
        }
        spans.push(Span::styled(" ".repeat(filler), style(theme::FG)));
        if let Some((glyph, color)) = pending {
            spans.push(Span::styled(format!(" {glyph} "), style(color)));
        }
        spans.extend(badges::cluster_spans(&cluster, background));
        spans.push(Span::styled(" ", style(theme::FG)));
        ListLine {
            line: Line::from(spans),
            item,
        }
    }
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

/// `── Label ──────`: a quiet rule with a dim label.
fn divider(label: &str, width: usize) -> ListLine {
    // One cell of right margin past the leading space, like the TS rule,
    // which stops short of the scrollbar gutter.
    let inner = width.saturating_sub(3);
    let label = truncate_end(&format!(" {label} "), inner.saturating_sub(4));
    let trail = inner.saturating_sub(2 + label.width());
    ListLine {
        line: Line::from(vec![
            Span::raw(" "),
            Span::styled("──", theme::fg(theme::BORDER_DIM)),
            Span::styled(label, theme::dim()),
            Span::styled("─".repeat(trail), theme::fg(theme::BORDER_DIM)),
        ]),
        item: None,
    }
}

/// A folded group collapsed to one stop: a `[×NN]` count chip, the label,
/// and on the right a compact rollup of the hidden rows' work states, so
/// folding a section never hides that something inside needs attention.
fn folded_header(
    label: &str,
    count: usize,
    rollup: Option<&SectionRollup>,
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
    let mut states: Vec<Span<'static>> = Vec::new();
    let mut alarms: Vec<Span<'static>> = Vec::new();
    if let Some(rollup) = rollup {
        for entry in super::details::ranked_states(&rollup.states).filter(|entry| {
            entry
                .state
                .is_some_and(|state| state != wt_core::WorkState::Todo)
        }) {
            let state = entry.state.expect("filtered to asserted states");
            states.push(Span::styled(
                format!("{} {} ", badges::work_state_glyph(state), entry.count),
                style(badges::work_state_color(state)),
            ));
        }
        if rollup.verification_overdue > 0 {
            alarms.push(Span::styled(
                format!("{} {} ", glyphs::DOT, rollup.verification_overdue),
                style(theme::ERR),
            ));
        }
        if rollup.conflicted > 0 {
            alarms.push(Span::styled(
                format!("{} {} ", glyphs::CONFLICT, rollup.conflicted),
                style(theme::ERR),
            ));
        }
        if rollup.failing_checks > 0 {
            alarms.push(Span::styled(
                format!("{} {} ", glyphs::CHECK_FAIL, rollup.failing_checks),
                style(theme::ERR),
            ));
        }
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
mod tests {
    use super::*;

    #[test]
    fn pending_removal_title_is_dim_and_struck_through_even_when_selected() {
        let model = Model::default();
        let context = RowContext {
            model: &model,
            width: 60,
            gutter: 0,
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
                        ..Default::default()
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
        assert!(!text[position("Failed task")].contains("Removal failed"));
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

    #[test]
    fn a_narrow_folded_header_drops_counts_before_its_name() {
        let rollup = SectionRollup {
            states: [
                wt_core::WorkState::Ready,
                wt_core::WorkState::NeedsHuman,
                wt_core::WorkState::Working,
                wt_core::WorkState::Review,
            ]
            .into_iter()
            .map(|state| crate::WorkStateCount {
                state: Some(state),
                count: 1,
            })
            .collect(),
            failing_checks: 1,
            ..Default::default()
        };
        let line = folded_header("Statuses", 8, Some(&rollup), false, 30, None);
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
    fn folded_header_rollup_reads_most_urgent_first() {
        let rollup = SectionRollup {
            states: [
                wt_core::WorkState::Working,
                wt_core::WorkState::NeedsTesting,
                wt_core::WorkState::NeedsHuman,
                wt_core::WorkState::Ready,
            ]
            .into_iter()
            .map(|state| crate::WorkStateCount {
                state: Some(state),
                count: 1,
            })
            .collect(),
            ..Default::default()
        };
        let line = folded_header("Batch", 4, Some(&rollup), false, 60, None);
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
}
