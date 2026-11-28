//! The details pane for the selected row, review request, or folded section.
//!
//! Layout follows a churn-rate order: stable identity at the top (title,
//! work status), then one definition row per configured `[ui].rows` group
//! with right-aligned dim labels, then the dynamic blocks (rebase and
//! conflicts, paused automations, session summary, prose details, PR
//! comments). Every line is pre-wrapped to the pane width here so the
//! paragraph never re-wraps, which keeps cell accounting and scroll exact.
//!
//! Glyphs and colors come from `badges` so the same fact wears the same
//! icon here and in the list.

use ratatui::{
    Frame,
    layout::{Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use unicode_width::UnicodeWidthStr;
use wt_core::WorkState;

use super::text::{age, now_ms, truncate_end, truncate_start, wrap};
use crate::{
    BoardRow, BoardSection, CheckState, DisplayPolicy, GuideView, LocalStamp, Model, OwedView,
    PrPresentation, PreparedDetailGroup, ReviewRequestRow, ReviewState, SessionView,
    WatchActionKind, WatchConditionView, WatchTruth, WatchView, badges, glyphs,
    model::ARCHIVED_SECTION, theme,
};

/// Conflicting files listed before collapsing into a `+N more` tail.
const MAX_CONFLICT_FILES: usize = 8;
/// Lines each blocker note may spend in a folded-section summary.
const SECTION_NOTE_LINES: usize = 4;
/// Labels recognized at the start of a structured ready note. A label must
/// be followed by a colon; `OPS` only as `OPS: none`.
const NOTE_LABELS: [&str; 4] = ["IF WRONG", "UNTESTED", "REVERT", "OPS"];

/// Fewest cells a note section's body needs beside its label.
const NOTE_BODY_MIN_CELLS: usize = 8;

/// Most cells of a step's prompt, a check's first line or a notification
/// text shown on its line.
const OWED_HEADLINE_CELLS: usize = 80;
/// Fewest worth showing on the item's own line; with less left the line
/// wraps and the text takes the most.
const OWED_HEADLINE_MIN_CELLS: usize = 16;
/// Cells before an owed item's "when": its mark and the gap. Everything an
/// item says starts in this column.
const OWED_LEAD_CELLS: usize = 2;
/// Fewest cells an item's "when" keeps beside a right-aligned detail; with
/// less, the detail follows the "when" instead.
const OWED_WHEN_MIN_CELLS: usize = 14;
/// Notifications the pane lists for one row; `wt watch` lists the rest.
const WATCH_LINES_MAX: usize = 6;

type Spans = Vec<Span<'static>>;
/// The controller's local labels for instants (`Board::history_times`).
type Times = std::collections::BTreeMap<u64, LocalStamp>;

pub(crate) fn render(frame: &mut Frame<'_>, model: &mut Model, area: Rect) {
    let title = pane_title(model);
    let pending = model
        .selected_row()
        .map(|row| super::status::row_pending(model, &row.key))
        .unwrap_or_default();
    let pending = super::status::pending_chip(&pending, ratatui::style::Style::new());
    let pending_width: usize = pending
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()) + 1)
        .sum();
    let title = truncate_end(
        &title,
        usize::from(area.width).saturating_sub(6 + pending_width),
    );
    // A row's details reflect only its own host's sources.
    let block = match model.selected_row() {
        Some(row) => super::status::row_pane(
            title,
            pending,
            row.host.as_deref(),
            super::status::DETAILS,
            model,
        ),
        None => super::status::pane_with(title, pending, super::status::DETAILS, model),
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 3 || inner.height == 0 {
        return;
    }
    // One cell of padding each side; the right one doubles as the
    // scrollbar's gutter so a thumb never sits on text. A blank first line
    // keeps the title off the border however long the content is; it
    // scrolls away with the content, and a pane of three rows or fewer
    // spends them all on content.
    let mut content = inner.inner(Margin {
        vertical: 0,
        horizontal: 1,
    });
    let width = usize::from(content.width).max(1);
    let mut lines = build(model, width);
    if content.height > 3 {
        lines.insert(0, Line::default());
    }
    // A section's key hint is its last line; when the summary overflows,
    // pin the hint to the bottom row so it never scrolls out of sight.
    let section_view = model.selected_row().is_none()
        && model.selected_review().is_none()
        && model.selected_section().is_some();
    if section_view && lines.len() > usize::from(content.height) && content.height > 2 {
        let hint = lines.pop().unwrap_or_default();
        if lines.last().is_some_and(|line| line.width() == 0) {
            lines.pop();
        }
        content.height -= 1;
        frame.render_widget(
            Paragraph::new(hint),
            Rect {
                y: content.y + content.height,
                height: 1,
                ..content
            },
        );
    }
    let height = usize::from(content.height);
    let max_scroll = lines.len().saturating_sub(height);
    let scroll = usize::from(model.details_scroll).min(max_scroll);
    model.details_scroll = u16::try_from(scroll).unwrap_or(u16::MAX);
    let total = lines.len();
    frame.render_widget(
        Paragraph::new(lines).scroll((model.details_scroll, 0)),
        content,
    );
    if let Some(url) = model
        .selected_row()
        .filter(|_| model.board.display.guides)
        .and_then(|row| row.guide.as_ref())
        .map(|guide| guide.url.clone())
        && let Some(link) = guide_link(frame.buffer_mut(), content)
    {
        model.hyperlinks.push(super::Hyperlink { area: link, url });
    }
    if total > height {
        let mut state = ScrollbarState::new(max_scroll).position(scroll);
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

fn pane_title(model: &Model) -> String {
    if let Some(row) = model.selected_row() {
        return if row.slug.is_empty() {
            "details".into()
        } else {
            row.slug.clone()
        };
    }
    if let Some(review) = model.selected_review() {
        return format!("review #{}", review.number);
    }
    if model.selected_section().is_some() {
        return "section".into();
    }
    "details".into()
}

fn build(model: &Model, width: usize) -> Vec<Line<'static>> {
    if let Some(row) = model.selected_row() {
        return row_lines(row, model, width);
    }
    if let Some(review) = model.selected_review() {
        return review_lines(review, width);
    }
    if let Some(section) = model.selected_section() {
        return section_lines(section, model, width);
    }
    vec![Line::styled(
        if model.board.review_requests.is_empty() {
            "No worktree selected."
        } else {
            "Review requests · Tab to fold or expand"
        },
        theme::dim(),
    )]
}

// ---------------------------------------------------------------------------
// Worktree rows

fn row_lines(row: &BoardRow, model: &Model, width: usize) -> Vec<Line<'static>> {
    let policy = &model.board.display;
    let now = now_ms();
    // Nearly every title is an agent's; only a person's is worth marking.
    let source = (row.title_source == crate::TitleSource::User).then_some("user");
    let mut lines = title_lines(&row.title, source, width);
    let times = &model.board.history_times;
    let work = work_status_block(row, width);
    // The facts follow the title directly, as in TS; a status block gets a
    // blank line after it.
    if !work.is_empty() {
        lines.extend(work);
        lines.push(Line::default());
    }

    // The path says nothing the slug in the border does not. A host on an
    // older build still sends the group.
    let groups: Vec<&PreparedDetailGroup> = row
        .detail_groups
        .iter()
        .filter(|group| group.id != "path")
        .collect();
    let label_width = label_column(&groups);
    for group in groups {
        lines.extend(definition(row, group, policy, label_width, width, now));
    }

    // What the row owes follows its facts, in full: the pane scrolls.
    let owes = owes_block(row, times, width);
    if !owes.is_empty() {
        if lines.last().is_some_and(|line| line.width() > 0) {
            lines.push(Line::default());
        }
        lines.extend(owes);
    }

    let leftovers = row
        .details
        .iter()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if !leftovers.is_empty() {
        lines.push(Line::default());
        for detail in leftovers {
            lines.extend(
                wrap(detail, width)
                    .into_iter()
                    .map(|line| Line::styled(line, theme::fg(theme::FG_MID))),
            );
        }
    }

    let rebase = rebase_block(row, policy, width);
    if !rebase.is_empty() {
        lines.push(Line::default());
        lines.extend(rebase);
    }

    let others = other_prs_block(row, width);
    if !others.is_empty() {
        lines.push(Line::default());
        lines.extend(others);
    }

    if row.automations_paused && (policy.automations || !row.watches.is_empty()) {
        lines.push(Line::default());
        lines.push(fit_line(
            vec![
                Span::styled(format!("{} ", glyphs::PAUSE), theme::fg(theme::WARN)),
                Span::styled(
                    "automations paused for this worktree (ctrl+a resumes)",
                    theme::dim(),
                ),
            ],
            width,
        ));
    }

    if let Some(summary) = badges::active_session(row, policy)
        .or_else(|| row.sessions.first())
        .and_then(|session| session.summary.as_deref())
        .filter(|summary| !summary.trim().is_empty())
    {
        lines.push(Line::default());
        lines.extend(
            wrap(summary.trim(), width)
                .into_iter()
                .map(|line| Line::styled(line, theme::fg(theme::FG_MID))),
        );
    }

    if let Some(pr) = &row.pr {
        lines.extend(comment_lines(pr, width));
    }
    lines
}

/// The full title, wrapped to the pane, with a dim `(source)` tag on the
/// last line when it fits and on its own line when it does not.
fn title_lines(title: &str, source: Option<&str>, width: usize) -> Vec<Line<'static>> {
    let text = if title.trim().is_empty() {
        "(untitled)"
    } else {
        title
    };
    let mut wrapped = wrap(text, width);
    // Regular weight, like the TS pane: the border already names the slug,
    // and a bold title outshouts the status banner beneath it.
    let style = Style::new().fg(theme::FG_BRIGHT);
    let tag = source.map(|source| format!(" ({source})"));
    let last = wrapped.pop().unwrap_or_default();
    let mut lines = wrapped
        .into_iter()
        .map(|line| Line::styled(line, style))
        .collect::<Vec<_>>();
    match tag {
        Some(tag) if last.width() + tag.width() <= width => {
            lines.push(Line::from(vec![
                Span::styled(last, style),
                Span::styled(tag, theme::dim()),
            ]));
        }
        Some(tag) => {
            lines.push(Line::styled(last, style));
            lines.push(Line::styled(
                truncate_end(tag.trim_start(), width),
                theme::dim(),
            ));
        }
        None => lines.push(Line::styled(last, style)),
    }
    lines
}

fn group_label(group: &PreparedDetailGroup) -> String {
    match group.id.as_str() {
        "claude" => "ai".into(),
        "branch" | "issue" | "stage" | "dev" | "pr" | "guide" | "git" => group.id.clone(),
        _ => group.label.to_lowercase(),
    }
}

/// Label column: the widest label present plus a one-cell gap.
fn label_column(groups: &[&PreparedDetailGroup]) -> usize {
    groups
        .iter()
        .map(|group| group_label(group).width())
        .max()
        .unwrap_or(0)
        .max(3)
        + 1
}

fn label_span(label: &str, label_width: usize) -> Span<'static> {
    let label = truncate_end(label, label_width.saturating_sub(1));
    let pad = label_width.saturating_sub(label.width() + 1);
    Span::styled(format!("{}{label} ", " ".repeat(pad)), theme::dim())
}

/// One labeled definition row, plus any continuation lines aligned to the
/// value column.
fn definition(
    row: &BoardRow,
    group: &PreparedDetailGroup,
    policy: &DisplayPolicy,
    label_width: usize,
    width: usize,
    now: u64,
) -> Vec<Line<'static>> {
    let value_width = width.saturating_sub(label_width).max(1);
    let mut values: Vec<Spans> = match group.id.as_str() {
        "branch" => vec![branch_value(row, group, value_width)],
        "issue" => vec![issue_value(row)],
        "stage" => vec![stage_value(row)],
        "dev" => vec![dev_value(row)],
        "guide" => vec![guide_value(row)],
        "pr" => pr_value(row, policy, value_width),
        "claude" => session_values(row, policy),
        "git" => vec![git_value(row, value_width, now)],
        _ => group
            .lines
            .iter()
            .flat_map(|line| wrap(line, value_width))
            .map(|line| vec![Span::styled(line, theme::fg(theme::FG))])
            .collect(),
    };
    if let Some(error) = &group.error {
        values.extend(
            wrap(error, value_width)
                .into_iter()
                .map(|line| vec![Span::styled(line, theme::fg(theme::ERR))]),
        );
    }
    if values.is_empty() {
        values.push(vec![Span::styled("—", theme::dim())]);
    }
    let label = group_label(group);
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let mut spans = vec![if index == 0 {
                label_span(&label, label_width)
            } else {
                Span::raw(" ".repeat(label_width))
            }];
            spans.extend(value);
            fit_line(spans, width)
        })
        .collect()
}

/// `<branch> → <base>`. The branch gives up cells, never the base: a cut
/// branch is still recognizable (the slug is the pane title) while a cut
/// target says nothing about where the work lands.
fn branch_value(row: &BoardRow, group: &PreparedDetailGroup, width: usize) -> Spans {
    let branch = if row.branch.is_empty() {
        "(none)"
    } else {
        row.branch.as_str()
    };
    let base = group
        .lines
        .get(1)
        .map(String::as_str)
        .filter(|base| !base.is_empty())
        .or_else(|| row_base(row));
    let Some(base) = base else {
        return vec![Span::styled(
            truncate_end(branch, width),
            theme::fg(theme::FG),
        )];
    };
    let forked = group.lines.get(2).is_some_and(|tag| tag == "forked");
    let tail = format!(" → {base}{}", if forked { " (forked)" } else { "" });
    let branch = truncate_end(branch, width.saturating_sub(tail.width()).max(8));
    let mut spans = vec![
        Span::styled(branch, theme::fg(theme::FG)),
        Span::styled(" → ", theme::dim()),
        Span::styled(base.to_owned(), theme::fg(theme::FG)),
    ];
    if forked {
        spans.push(Span::styled(" (forked)", theme::dim()));
    }
    spans
}

/// The base the row forks from and lands on: the branch group's prepared
/// base, else the row's recorded base. `None` when neither is known.
fn row_base(row: &BoardRow) -> Option<&str> {
    row.detail_groups
        .iter()
        .find(|group| group.id == "branch")
        .and_then(|group| group.lines.get(1))
        .map(String::as_str)
        .or(row.base_branch.as_deref())
        .filter(|base| !base.is_empty())
}

fn issue_value(row: &BoardRow) -> Spans {
    let github = row
        .github_issue_url
        .as_deref()
        .and_then(|url| url.rsplit('/').next())
        .filter(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()));
    let Some(id) = row.issue_id.as_deref().filter(|id| !id.is_empty()) else {
        return match github {
            Some(number) => vec![Span::styled(format!("#{number}"), theme::fg(theme::FG))],
            None => vec![Span::styled("—", theme::dim())],
        };
    };
    let badges::Badge { glyph, color } =
        badges::issue_status_badge(row.issue_status.as_deref().unwrap_or_default());
    let mut spans = vec![Span::styled(format!("{glyph}  #{id}"), theme::fg(color))];
    if let Some(number) = github {
        spans.push(Span::styled(" ← ", theme::dim()));
        spans.push(Span::styled(format!("#{number}"), theme::fg(theme::FG)));
    }
    if let Some(status) = row.issue_status.as_deref().filter(|s| !s.is_empty()) {
        spans.push(Span::styled(" · ", theme::dim()));
        spans.push(Span::styled(status.to_owned(), theme::fg(color)));
    }
    spans
}

/// Display text of the guide link in its own row; [`guide_link`] finds it
/// on screen.
const GUIDE_LINK: &str = "change guide";
/// The guide link's text at the end of the PR line.
const GUIDE_WORD: &str = "guide";

/// Where the guide link landed: its own row, else the PR line's book glyph
/// and word, else the bare glyph when the pane is narrow.
fn guide_link(buf: &ratatui::buffer::Buffer, area: Rect) -> Option<Rect> {
    find_text(buf, area, GUIDE_LINK)
        .or_else(|| find_text(buf, area, &format!("{}  {GUIDE_WORD}", glyphs::BOOK)))
        .or_else(|| find_text(buf, area, glyphs::BOOK))
}

/// The recorded guide and whether it still describes this HEAD.
fn guide_value(row: &BoardRow) -> Spans {
    let Some(guide) = &row.guide else {
        return vec![Span::styled("none recorded", theme::dim())];
    };
    let (freshness, color) = match guide.current {
        Some(true) => ("current", theme::OK),
        Some(false) => ("outdated: commits since", theme::WARN),
        None => ("freshness unknown", theme::FG_DIM),
    };
    vec![
        Span::styled(
            GUIDE_LINK,
            theme::fg(theme::INFO).add_modifier(ratatui::style::Modifier::UNDERLINED),
        ),
        Span::styled(format!(" · {freshness}"), theme::fg(color)),
    ]
}

/// Where `text` first appears inside `area`, as a one-row rectangle.
fn find_text(buf: &ratatui::buffer::Buffer, area: Rect, text: &str) -> Option<Rect> {
    let chars = text.chars().collect::<Vec<_>>();
    let width = u16::try_from(chars.len()).ok()?;
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right().saturating_sub(width.saturating_sub(1)) {
            if chars.iter().enumerate().all(|(i, ch)| {
                buf[(x + i as u16, y)]
                    .symbol()
                    .chars()
                    .eq(std::iter::once(*ch))
            }) {
                return Some(Rect::new(x, y, width, 1));
            }
        }
    }
    None
}

fn stage_value(row: &BoardRow) -> Spans {
    match row.stage_url.as_deref() {
        Some(url) => vec![Span::styled(
            format!("{}  {url}", glyphs::BOLT),
            theme::fg(theme::WARN),
        )],
        None => vec![Span::styled(
            format!("{}  not deployed", glyphs::BAN),
            theme::dim(),
        )],
    }
}

/// Dev server: the prepared summary when the host provides one, else the
/// URL of a live server, else quiet.
fn dev_value(row: &BoardRow) -> Spans {
    let live = row.environment_live || row.dev_url.is_some();
    let Some(text) = row.dev_status.clone().or_else(|| row.dev_url.clone()) else {
        // Without a typed fact the state is unknown, not stopped; the
        // host's prose line (below the rows) carries what it knows.
        return vec![Span::styled("—", theme::dim())];
    };
    if live {
        glyph_text(glyphs::BOLT, &text, theme::WARN)
    } else {
        glyph_text(glyphs::BAN, &text, theme::FG_DIM)
    }
}

/// The PR line, ending with the change guide when one is recorded and no
/// `guide` row of its own shows it. The guide drops first on a narrow pane.
fn pr_value(row: &BoardRow, policy: &DisplayPolicy, width: usize) -> Vec<Spans> {
    let mut segments = pr_segments(row.pr.as_ref(), policy);
    let standalone = row.detail_groups.iter().any(|group| group.id == "guide");
    if policy.guides
        && !standalone
        && let Some(guide) = &row.guide
    {
        let segments = segments.get_or_insert_with(|| {
            vec![Segment::new(1, vec![vec![Span::styled("—", theme::dim())]])]
        });
        segments.push(guide_segment(guide));
    }
    // Only an error is known; the caller renders it from the group.
    segments.map_or_else(Vec::new, |segments| vec![fit_segments(segments, width)])
}

/// Linked `guide`, colored by whether it still describes this HEAD.
fn guide_segment(guide: &GuideView) -> Segment {
    let color = match guide.current {
        Some(true) => theme::OK,
        Some(false) => theme::WARN,
        None => theme::FG_DIM,
    };
    let glyph = Span::styled(format!("{}  ", glyphs::BOOK), theme::fg(color));
    Segment::new(
        7,
        vec![
            vec![
                glyph.clone(),
                Span::styled(
                    GUIDE_WORD,
                    theme::fg(color).add_modifier(ratatui::style::Modifier::UNDERLINED),
                ),
            ],
            vec![glyph],
        ],
    )
}

/// `None` only when an error is all that is known about the PR.
fn pr_segments(pr: Option<&PrPresentation>, policy: &DisplayPolicy) -> Option<Vec<Segment>> {
    let dash = || {
        Some(vec![Segment::new(
            1,
            vec![vec![Span::styled("—", theme::dim())]],
        )])
    };
    let Some(pr) = pr else {
        return dash();
    };
    let Some(number) = pr.number else {
        return if pr.error.is_some() { None } else { dash() };
    };
    let state = badges::pr_state_badge(pr);
    let id = format!("#{number}");
    let mut segments = vec![Segment::new(
        1,
        vec![
            glyph_text(state.glyph, &id, state.color),
            vec![Span::styled(id.clone(), theme::fg(state.color))],
        ],
    )];
    if let Some(queue) = &pr.merge_queue {
        let (label, color) = badges::merge_queue_state(&queue.state);
        segments.push(Segment::new(
            2,
            vec![
                glyph_text(
                    glyphs::MERGE_QUEUE,
                    &format!("#{} {label}", queue.position),
                    color,
                ),
                glyph_text(glyphs::MERGE_QUEUE, &format!("#{}", queue.position), color),
            ],
        ));
    } else if pr.auto_merge_armed && pr.is_open() {
        segments.push(Segment::new(
            6,
            vec![
                glyph_text(glyphs::MERGE_QUEUE, "auto-merge", theme::INFO),
                glyph_text(glyphs::MERGE_QUEUE, "auto", theme::INFO),
            ],
        ));
    }
    // GitHub dropped the merge wt armed and nothing holds it now. The same
    // words `wt wait` and `wt fleet` give, from the same rule.
    if let Some(why) = &pr.queue_removal {
        segments.push(Segment::new(
            2,
            vec![
                glyph_text(glyphs::MERGE_QUEUE, why, theme::WARN),
                glyph_text(glyphs::MERGE_QUEUE, "removed", theme::WARN),
            ],
        ));
    }
    if pr.is_open() {
        if let Some(checks) = badges::check_badge(pr.checks) {
            let text = if pr.checks == CheckState::Pending {
                "checks pending"
            } else {
                "checks"
            };
            let mut modes = Vec::new();
            if pr.checks == CheckState::Fail && !pr.failed_checks.is_empty() {
                modes.push(glyph_text(
                    checks.glyph,
                    &format!("checks: {}", pr.failed_checks.join(", ")),
                    checks.color,
                ));
            }
            modes.push(glyph_text(checks.glyph, text, checks.color));
            segments.push(Segment::new(3, modes));
        }
        if !pr.draft
            && let Some(review) = badges::review_badge(pr.review, policy)
        {
            let text = match pr.review {
                ReviewState::Approved => "approved",
                ReviewState::ChangesRequested => "changes requested",
                ReviewState::Pending => "review pending",
                _ => "no reviewers",
            };
            segments.push(Segment::new(
                4,
                vec![glyph_text(review.glyph, text, review.color)],
            ));
        }
        if let Some(bot) = badges::review_bot_badge(pr, policy)
            && let Some(view) = &pr.review_bot
        {
            let whimsy = policy.review_bot_carrot;
            let stale = if view.stale { " (old head)" } else { "" };
            let modes = match view.state.as_str() {
                "unresolved" => {
                    let noun = if whimsy { "carrot" } else { "issue" };
                    let plural = if view.unresolved == 1 { "" } else { "s" };
                    vec![
                        glyph_text(
                            bot.glyph,
                            &format!("{} {noun}{plural}{stale}", view.unresolved),
                            bot.color,
                        ),
                        glyph_text(bot.glyph, &view.unresolved.to_string(), bot.color),
                    ]
                }
                "pending" => vec![glyph_text(
                    bot.glyph,
                    if whimsy { "grazing" } else { "reviewing" },
                    bot.color,
                )],
                _ => vec![glyph_text(
                    bot.glyph,
                    &format!("{}{stale}", if whimsy { "resting" } else { "reviewed" }),
                    bot.color,
                )],
            };
            segments.push(Segment::new(5, modes));
        }
    }
    Some(segments)
}

/// The row's sessions, the one F12 attaches to first. With none, the
/// primary harness and how to start one.
fn session_values(row: &BoardRow, policy: &DisplayPolicy) -> Vec<Spans> {
    if row.sessions.is_empty() {
        let harness = if policy.primary_harness.is_empty() {
            "Claude"
        } else {
            policy.primary_harness.as_str()
        };
        return vec![vec![
            Span::styled(
                format!("{}  ", badges::harness_glyph(harness)),
                theme::fg(badges::harness_color(harness)),
            ),
            Span::styled(format!("primary: {harness} · F12 to start"), theme::dim()),
        ]];
    }
    let active = badges::active_session(row, policy).map(|session| session.id.as_str());
    let mut sessions = row.sessions.iter().collect::<Vec<_>>();
    sessions.sort_by_key(|session| Some(session.id.as_str()) != active);
    sessions.into_iter().map(session_spans).collect()
}

fn session_spans(session: &SessionView) -> Spans {
    let known = !session.state.is_empty() && session.state != "unknown";
    let state = if known {
        session.state.clone()
    } else if session.live {
        "live".into()
    } else {
        "dead".into()
    };
    let color = if known {
        badges::session_state_color(&session.harness, &session.state)
    } else if session.live {
        badges::harness_color(&session.harness)
    } else {
        theme::FG_DIM
    };
    let mut spans = vec![
        Span::styled(
            format!("{}  ", badges::harness_glyph(&session.harness)),
            theme::fg(color),
        ),
        Span::styled(state, theme::fg(color)),
    ];
    if !session.name.is_empty() {
        spans.push(Span::styled(" · ", theme::dim()));
        spans.push(Span::styled(session.name.clone(), theme::fg(theme::FG)));
    }
    if let Some(percent) = session.context_percent {
        spans.push(Span::styled(format!(" · {percent}% context"), theme::dim()));
    }
    if session.queued > 0 {
        spans.push(Span::styled(
            format!(" · {} queued", session.queued),
            theme::fg(theme::WARN),
        ));
    }
    spans
}

/// `verb · +N −M (K files) · committed 5m · created 3d · (↑1 ↓2) [↑3 ↓4]`,
/// compacted by priority: the verb is sticky, change state (diff, sync)
/// outranks ages, and `created` drops before `committed`.
fn git_value(row: &BoardRow, width: usize, now: u64) -> Spans {
    let (verb, mut text) = badges::status_verb(row);
    // Name where it landed, using the base the branch line shows.
    if row.busy.is_none()
        && !row.path_missing
        && !row.branch_gone
        && row.git.landed_on == Some(crate::LandingKind::Base)
        && let Some(base) = row_base(row)
    {
        text = format!("merged into {base}");
    }
    let mut segments = vec![Segment::new(
        1,
        vec![glyph_text(verb.glyph, &text, verb.color)],
    )];
    let inspectable = row.busy.is_none() && !row.path_missing;
    if inspectable
        && let Some(diff) = row
            .git
            .diff
            .filter(|diff| diff.added > 0 || diff.removed > 0)
    {
        let counts = vec![
            Span::styled(format!("+{}", diff.added), theme::fg(theme::WARN)),
            Span::raw(" "),
            Span::styled(format!("−{}", diff.removed), theme::fg(theme::ERR)),
        ];
        let mut full = counts.clone();
        if diff.files > 0 {
            full.push(Span::styled(
                format!(
                    " ({} {})",
                    diff.files,
                    if diff.files == 1 { "file" } else { "files" }
                ),
                theme::dim(),
            ));
        }
        segments.push(Segment::new(2, vec![full, counts]));
    }
    if inspectable && let Some(at) = row.git.last_commit_ms {
        let age = age(at, now);
        segments.push(Segment::new(
            3,
            vec![
                vec![Span::styled(format!("committed {age}"), theme::dim())],
                vec![Span::styled(age, theme::dim())],
            ],
        ));
    }
    if inspectable && let Some(at) = row.git.created_ms {
        let age = age(at, now);
        segments.push(Segment::new(
            4,
            // No compact form: a bare `+2h` beside `+2 −0` reads as a diff.
            vec![vec![Span::styled(format!("created {age}"), theme::dim())]],
        ));
    }
    if inspectable {
        let upstream = row
            .git
            .upstream
            .as_ref()
            .map(|_| (row.git.ahead, row.git.behind));
        // A side with no counts says nothing: no upstream, or not read.
        let remote = upstream
            .and_then(|(a, b)| a.zip(b))
            .map(|counts| sync_group("(", ")", counts));
        let base = row
            .git
            .base_ahead
            .zip(row.git.base_behind)
            .map(|counts| sync_group("[", "]", counts));
        let modes = match (remote, base) {
            (Some(remote), Some(base)) => {
                let mut both = remote.clone();
                both.push(Span::raw(" "));
                both.extend(base);
                vec![both, remote]
            }
            (Some(one), None) | (None, Some(one)) => vec![one],
            (None, None) => Vec::new(),
        };
        if !modes.is_empty() {
            segments.push(Segment::new(2, modes));
        }
    }
    fit_segments(segments, width)
}

/// `(↑a ↓b)`: ahead warns, behind errs, zero is dim.
fn sync_group(open: &str, close: &str, (ahead, behind): (u32, u32)) -> Spans {
    if ahead == 0 && behind == 0 {
        return vec![Span::styled(format!("{open}↑0 ↓0{close}"), theme::dim())];
    }
    let mut spans = vec![Span::styled(open.to_owned(), theme::dim())];
    if ahead > 0 {
        spans.push(Span::styled(format!("↑{ahead}"), theme::fg(theme::WARN)));
    }
    if ahead > 0 && behind > 0 {
        spans.push(Span::raw(" "));
    }
    if behind > 0 {
        spans.push(Span::styled(format!("↓{behind}"), theme::fg(theme::ERR)));
    }
    spans.push(Span::styled(close.to_owned(), theme::dim()));
    spans
}

/// Cells a block's glyph slot takes; its wrapped header and its file list
/// both start past it.
const HANG: usize = 3;

/// `glyph  text`, wrapped so continuation lines start under the text.
fn hanging_header(badge: badges::Badge, text: &str, width: usize) -> Vec<Line<'static>> {
    wrap(text, width.saturating_sub(HANG))
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let line = if index == 0 {
                format!("{}  {line}", badge.glyph)
            } else {
                format!("{:HANG$}{line}", "")
            };
            Line::styled(line, theme::fg(badge.color))
        })
        .collect()
}

/// The rebase lifecycle as a block: restacking, mid-rebase, or a branch
/// that will not rebase cleanly onto its base (with the clashing files).
fn rebase_block(row: &BoardRow, policy: &DisplayPolicy, width: usize) -> Vec<Line<'static>> {
    let session_state = badges::row_session_activity(row, policy).map(|(_, state)| state);
    let Some(badge) = badges::rebase_badge(row, session_state) else {
        return Vec::new();
    };
    let header = |text: String| hanging_header(badge, &text, width);
    let files = |files: &[String], lines: &mut Vec<Line<'static>>| {
        for file in files.iter().take(MAX_CONFLICT_FILES) {
            lines.push(Line::styled(
                format!(
                    "{:HANG$}{}",
                    "",
                    truncate_start(file, width.saturating_sub(HANG))
                ),
                theme::dim(),
            ));
        }
        if files.len() > MAX_CONFLICT_FILES {
            lines.push(Line::styled(
                format!("   +{} more", files.len() - MAX_CONFLICT_FILES),
                theme::dim(),
            ));
        }
    };
    if let Some(busy) = row.busy.as_ref().filter(|busy| busy.op == "restack") {
        let phase = if busy.label.is_empty() {
            "running"
        } else {
            busy.label.as_str()
        };
        return header(format!("Restacking ({phase})"));
    }
    if row.git.rebasing || !row.git.conflict_files.is_empty() {
        let mut lines =
            header("Mid-rebase: resolve and continue in this worktree (/restack)".into());
        files(&row.git.conflict_files, &mut lines);
        return lines;
    }
    let base = row_base(row).unwrap_or("its base");
    let base = base.strip_prefix("origin/").unwrap_or(base);
    let resolving = badge.glyph != glyphs::CONFLICT;
    let head = if resolving {
        format!("Resolving conflict with {base} in the session")
    } else {
        format!("Won't rebase cleanly onto {base}")
    };
    let conflicts = row.git.base_conflicts.as_deref().unwrap_or_default();
    let count = conflicts.len();
    let mut lines = header(format!(
        "{head} · {count} conflicting file{}",
        if count == 1 { "" } else { "s" }
    ));
    files(conflicts, &mut lines);
    lines
}

/// The worktree holds more than its own pull request: the other branches
/// its checkout has been on that carry, or may carry, one with no row.
fn other_prs_block(row: &BoardRow, width: usize) -> Vec<Line<'static>> {
    let others: Vec<String> = row
        .other_prs
        .iter()
        .filter(|other| other.unsettled())
        .map(crate::OtherPrView::label)
        .collect();
    if others.is_empty() {
        return Vec::new();
    }
    wrap(
        &format!(
            "{} Holds more than one PR: also {}",
            glyphs::PAUSE,
            others.join(", ")
        ),
        width,
    )
    .into_iter()
    .map(|line| Line::styled(line, theme::fg(theme::WARN)))
    .collect()
}

/// The PR's human conversation, newest first, then a count of unresolved
/// review threads whose bodies are not inlined.
fn comment_lines(pr: &PrPresentation, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for comment in &pr.comments {
        lines.push(Line::default());
        let mut meta = vec![Span::styled(
            format!("@{}", comment.author),
            Style::new().fg(theme::FG).add_modifier(Modifier::BOLD),
        )];
        if let Some(age) = &comment.age {
            meta.push(Span::styled(format!(" · {age} ago"), theme::dim()));
        }
        lines.push(fit_line(meta, width));
        lines.extend(
            wrap(comment.body.trim(), width)
                .into_iter()
                .map(|line| Line::styled(line, theme::fg(theme::FG))),
        );
    }
    if pr.unresolved_threads > 0 {
        lines.push(Line::default());
        lines.push(Line::styled(
            format!(
                "+{}{} unresolved {}",
                pr.unresolved_threads,
                if pr.unresolved_threads_more {
                    " or more"
                } else {
                    ""
                },
                if pr.unresolved_threads == 1 && !pr.unresolved_threads_more {
                    "thread"
                } else {
                    "threads"
                }
            ),
            theme::dim(),
        ));
    }
    lines
}

// ---------------------------------------------------------------------------
// Work status

/// The status banner and the owed block for tests that read them without a
/// pane width.
#[cfg(test)]
pub(crate) fn work_status_lines(row: &BoardRow) -> Vec<Line<'static>> {
    let mut lines = work_status_block(row, 200);
    lines.extend(owes_block(row, &Times::new(), 200));
    lines
}

/// Full-width status banner: the header, then the note behind its rail.
/// What the row owes is listed after the facts ([`owes_block`]); the
/// header only counts it.
fn work_status_block(row: &BoardRow, width: usize) -> Vec<Line<'static>> {
    let header = status_header(row);
    let mut lines = header
        .as_ref()
        .map(|(items, _)| wrap_items(items.clone(), width))
        .unwrap_or_default();
    if let Some((_, color)) = header
        && let Some(note) = row
            .work
            .as_ref()
            .and_then(|work| work.record.as_ref())
            .and_then(|record| record.note.as_deref())
    {
        lines.extend(note_lines(note, color, width));
    }
    lines
}

/// The one list of what the row owes, then the watches that only tell the
/// person. Always in full: nothing folds, the pane scrolls.
fn owes_block(row: &BoardRow, times: &Times, width: usize) -> Vec<Line<'static>> {
    let mut lines = owes_lines(row, times, width);
    lines.extend(notify_lines(row, times, width));
    lines
}

/// The banner's first line, as the items its ` · ` separators join, and its
/// color. A landed row reads from where it stands with what it owes; until
/// then the asserted state and the gate say where the row is.
fn status_header(row: &BoardRow) -> Option<(Vec<Spans>, ratatui::style::Color)> {
    let work = row.work.as_ref();
    let record = work.and_then(|work| work.record.as_ref());
    let effective = work.and_then(|work| work.effective_state);
    let derived = work.is_some_and(|work| work.derived);
    let (glyph, text, color) = match row.owed_phase {
        Some(phase) => phase_header(phase, row.owed_block, &row.owes),
        None => {
            let (work, effective) = (work?, effective?);
            if work.blocked {
                let state = record.map_or(effective, |record| record.state);
                (
                    glyphs::OWED_GATE,
                    format!("blocked · {}", state.as_str()),
                    theme::WARN,
                )
            } else if derived {
                (
                    banner_glyph(effective),
                    format!("{} · live", effective.as_str()),
                    badges::work_state_color(effective),
                )
            } else {
                (
                    banner_glyph(effective),
                    effective.as_str().to_owned(),
                    badges::work_state_color(effective),
                )
            }
        }
    };
    let mut header = vec![vec![
        Span::styled(format!("{glyph} "), theme::fg(color)),
        Span::styled(text, theme::bold(color)),
    ]];
    // Someone is still at work on a landed row, or is asked something.
    if row.owed_phase.is_some()
        && let Some(state @ (WorkState::NeedsHuman | WorkState::Working | WorkState::Review)) =
            effective
    {
        header.push(vec![Span::styled(
            state.as_str(),
            theme::fg(badges::work_state_color(state)),
        )]);
    }
    // The review stamp trails the line, hollow once the head has moved.
    let stamp = badges::stamp(row).map(|(stamp, current)| {
        let glyph = if current {
            wt_core::STAMP_CURRENT
        } else {
            wt_core::STAMP_MOVED
        };
        vec![Span::styled(
            format!("{glyph} {}", stamp.skill),
            theme::dim(),
        )]
    });
    // What a landed row owes is all it has left to say: the claim's risk
    // and age were about a merge that has happened.
    if row.owed_phase.is_some() {
        header.extend(stamp);
        return Some((header, color));
    }
    if let Some(risk) = record.and_then(|record| record.risk) {
        header.push(vec![
            Span::styled("risk ", theme::dim()),
            Span::styled(risk.as_str(), theme::fg(badges::risk_color(risk))),
        ]);
    }
    if let Some(age) = work
        .and_then(|work| work.age.as_ref())
        .filter(|_| !derived || record.is_some())
    {
        header.push(vec![Span::styled(format!("{age} ago"), theme::dim())]);
    }
    if work.is_some_and(|work| work.stale == Some(true)) && !derived {
        header.push(vec![Span::styled("commits since", theme::fg(theme::WARN))]);
    }
    // The list itself is below the facts: say that there is one.
    if !row.owes.is_empty() {
        header.push(vec![Span::styled(
            format!("owes {}", row.owes.len()),
            theme::dim(),
        )]);
    }
    header.extend(stamp);
    Some((header, color))
}

/// A landed row's header: that something is due and how much, what it
/// waits on and how much it owes, or that it is done.
fn phase_header(
    phase: wt_core::OwedPhase,
    block: Option<crate::OwedBlock>,
    owes: &[OwedView],
) -> (&'static str, String, ratatui::style::Color) {
    let overdue = owes.iter().any(|item| item.overdue);
    match phase {
        // The list right below says what is due; the header counts.
        wt_core::OwedPhase::Due => {
            let due = owes.iter().filter(|item| item.due()).count();
            let waiting = owes.len() - due;
            let text = match (due, waiting) {
                (0 | 1, 0) => "due".to_owned(),
                (0 | 1, _) => format!("due · {waiting} waiting"),
                (_, 0) => format!("{due} due"),
                _ => format!("{due} due · {waiting} waiting"),
            };
            (
                glyphs::OWED_DUE,
                text,
                if overdue { theme::ERR } else { theme::WARN },
            )
        }
        wt_core::OwedPhase::Waiting => {
            // What it waits on, as the list's blocks name it; the items
            // below say the rest.
            let what = block.map_or("waiting", waiting_words);
            (
                glyphs::OWED_CLOCK,
                format!("{what} · {} owed", owes.len()),
                if overdue { theme::ERR } else { theme::FG_MID },
            )
        }
        wt_core::OwedPhase::Done => (glyphs::OWED_DONE, "done".to_owned(), theme::OK),
    }
}

/// What a landed row with nothing due waits on, as its header and the
/// Landed block's pane both name it.
fn waiting_words(block: crate::OwedBlock) -> &'static str {
    match block {
        crate::OwedBlock::WaitingStaging => "waiting for staging",
        crate::OwedBlock::WaitingProduction => "waiting for production",
        crate::OwedBlock::Scheduled => "scheduled",
        crate::OwedBlock::WaitingOther => "waiting on other work",
        crate::OwedBlock::Due | crate::OwedBlock::Done => "waiting",
    }
}

/// A condition's words, with a time in the controller's clock when it
/// labelled the instant and the host's otherwise.
fn instant_text(cond: &WatchConditionView, times: &Times) -> String {
    cond.at_ms
        .and_then(|at_ms| stamp_text(at_ms, times))
        .unwrap_or_else(|| cond.text.clone())
}

fn stamp_text(at_ms: u64, times: &Times) -> Option<String> {
    times.get(&at_ms).map(|stamp| match stamp.day.as_str() {
        "today" => stamp.clock.clone(),
        day @ ("tomorrow" | "yesterday") => format!("{} {day}", stamp.clock),
        day => format!("{day} {}", stamp.clock),
    })
}

/// One condition: a mark for whether it holds, its words, how far off a
/// time is, and dimly why it cannot be read. A lone time needs no mark.
fn condition_spans(cond: &WatchConditionView, times: &Times, marked: bool) -> Spans {
    let mut spans = condition_words(cond, times, marked);
    if let Some(reason) = cond.reason.as_deref().filter(|reason| !reason.is_empty()) {
        spans.push(Span::styled(format!(" ({reason})"), theme::dim()));
    }
    spans
}

/// [`condition_spans`] without the reason, for a line that says it elsewhere.
fn condition_words(cond: &WatchConditionView, times: &Times, marked: bool) -> Spans {
    let (mark, color) = match cond.state {
        WatchTruth::Holds => ("✓", theme::OK),
        WatchTruth::NotYet => (glyphs::OWED_WAITING, theme::FG_DIM),
        WatchTruth::Unknown => (glyphs::OWED_UNKNOWN, theme::WARN),
    };
    let mut spans = Vec::new();
    if marked {
        // No-break, so a wrap never strands the mark. The words themselves
        // wrap: a long condition kept whole would be cut instead.
        spans.push(Span::styled(format!("{mark}\u{a0}"), theme::fg(color)));
    }
    spans.push(Span::styled(
        instant_text(cond, times),
        theme::fg(theme::FG_MID),
    ));
    if let Some(due_in) = cond
        .due_in
        .as_deref()
        .filter(|_| cond.state == WatchTruth::NotYet)
    {
        spans.push(Span::styled(format!(" (in\u{a0}{due_in})"), theme::dim()));
    }
    spans
}

fn conditions_spans(conditions: &[WatchConditionView], times: &Times, marked: bool) -> Spans {
    let mut spans = Vec::new();
    for (index, cond) in conditions.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" and ", theme::dim()));
        }
        spans.extend(condition_spans(cond, times, marked));
    }
    spans
}

/// Whether the conditions are one instant and nothing else: that reads "at
/// 17:00" where anything else reads "when merged".
fn timed(conditions: &[WatchConditionView]) -> bool {
    matches!(conditions, [only] if only.at_ms.is_some())
}

/// That a met item is being delivered, or is held while automations are
/// paused.
fn delivery_tail(met: bool, held: bool) -> Option<Span<'static>> {
    if met && held {
        Some(Span::styled(
            " ·\u{a0}held:\u{a0}automations\u{a0}paused",
            theme::fg(theme::WARN),
        ))
    } else if met {
        Some(Span::styled(" ·\u{a0}delivering", theme::fg(theme::OK)))
    } else {
        None
    }
}

/// A headline cut to what is left of the line after `used` cells and the
/// `suffix`, so an item stays on one line wherever it can.
fn headline_span(
    headline: &str,
    used: usize,
    suffix: usize,
    width: usize,
    color: ratatui::style::Color,
) -> Span<'static> {
    let left = width.saturating_sub(used + suffix);
    // With no room worth using the line wraps anyway, and a wrapped line
    // may as well say the whole thing.
    let room = if left < OWED_HEADLINE_MIN_CELLS {
        OWED_HEADLINE_CELLS
    } else {
        left.min(OWED_HEADLINE_CELLS)
    };
    Span::styled(truncate_end(headline, room), theme::fg(color))
}

/// The mark an owed item wears: a filled dot once it is due (red when
/// overdue), the gate's slash, a question mark while a condition it waits
/// for cannot be read, and a hollow dot while it waits.
fn owed_mark(item: &OwedView) -> (&'static str, ratatui::style::Color) {
    if item.overdue {
        (glyphs::OWED_DUE, theme::ERR)
    } else if item.due() {
        (glyphs::OWED_DUE, theme::WARN)
    } else if item.unreadable() {
        (glyphs::OWED_UNKNOWN, theme::WARN)
    } else if item.kind == wt_core::OwedKind::Gate {
        (glyphs::OWED_GATE, theme::WARN)
    } else {
        (glyphs::OWED_WAITING, theme::FG_DIM)
    }
}

/// The "when" column of an owed item, in as few words as place it:
/// `staging`, `production +7d`, `#3889 merged`, `Fri 18:12`, `since 19:34`,
/// `before merge`. That an item is due is its mark and color, not a word.
fn owed_label(item: &OwedView, times: &Times) -> String {
    let terse = |at_ms: u64| {
        times
            .get(&at_ms)
            .map(|stamp| stamp.terse.clone())
            .filter(|terse| !terse.is_empty())
    };
    match item.kind {
        wt_core::OwedKind::Gate => "before merge".to_owned(),
        wt_core::OwedKind::Check => item
            .stage
            .map_or("check", wt_core::VerifyStage::as_str)
            .to_owned(),
        wt_core::OwedKind::Step if item.due() => item
            .due_at_ms
            .and_then(terse)
            .or_else(|| item.due_at.clone())
            .map_or_else(|| "due".to_owned(), |at| format!("since {at}")),
        wt_core::OwedKind::Step if item.conditions.is_empty() => item.when.clone(),
        wt_core::OwedKind::Step => item
            .conditions
            .iter()
            .map(|cond| {
                cond.at_ms
                    .and_then(terse)
                    .or_else(|| (!cond.label.is_empty()).then(|| cond.label.clone()))
                    .unwrap_or_else(|| cond.text.clone())
            })
            .collect::<Vec<_>>()
            .join(" & "),
    }
}

/// The one thing the right edge says about an item: that it is overdue,
/// that its deploy is awaited, what closes a due step, how long until a
/// known time, how many steps it has, or that it must precede a release.
fn owed_detail(item: &OwedView) -> Option<(String, ratatui::style::Color)> {
    let dim = |text: String| Some((text, theme::FG_DIM));
    if item.kind == wt_core::OwedKind::Gate {
        // Its own line says what clears it and when.
        return None;
    }
    if item.overdue {
        return Some(("overdue".to_owned(), theme::ERR));
    }
    if item.awaiting_deploy && !item.due() {
        return dim("awaiting deploy".to_owned());
    }
    if item.due() && item.kind == wt_core::OwedKind::Step {
        return item
            .id
            .as_deref()
            .and_then(|id| dim(format!("wt done {id}")));
    }
    let until = item.due_in.as_deref().or(item.not_before_in.as_deref()).or(
        match item.conditions.as_slice() {
            [only] => only.due_in.as_deref(),
            _ => None,
        },
    );
    if let Some(until) = until.filter(|_| !item.due()) {
        return dim(format!("in {until}"));
    }
    let steps = parse_verify_steps(&item.what).1.len();
    if steps > 0 {
        return dim(format!("{steps} step{}", if steps == 1 { "" } else { "s" }));
    }
    if item.before_release {
        return dim(BEFORE_RELEASE.to_owned());
    }
    None
}

const BEFORE_RELEASE: &str = "before release";

/// What else is worth a few dim words after an item's "when": who set it
/// when not the row itself, and how its delivery stands. `detail` is what
/// the right edge already says.
fn owed_tails(item: &OwedView, own: &str, detail: Option<&str>) -> Spans {
    let mut tails = Vec::new();
    if item.kind != wt_core::OwedKind::Step {
        return tails;
    }
    if item.before_release && detail != Some(BEFORE_RELEASE) {
        tails.push(Span::styled(" ·\u{a0}before\u{a0}release", theme::dim()));
    }
    if let Some(by) = item
        .by
        .as_deref()
        .filter(|by| !by.is_empty() && *by != own && *by != "human")
    {
        tails.push(Span::styled(format!(" ·\u{a0}by\u{a0}{by}"), theme::dim()));
    }
    if item.unconfirmed {
        tails.push(Span::styled(
            " ·\u{a0}delivery not confirmed",
            theme::fg(theme::WARN),
        ));
    }
    // Why a condition cannot be read. What a holding one still waits out
    // is the countdown at the right edge.
    for reason in item
        .conditions
        .iter()
        .filter(|cond| cond.state == WatchTruth::Unknown)
        .filter_map(|cond| cond.reason.as_deref())
        .filter(|reason| !reason.is_empty())
    {
        tails.push(Span::styled(format!(" · {reason}"), theme::dim()));
    }
    tails.extend(delivery_tail(item.met, item.held));
    tails
}

/// What clears a gate, as its own line under the gate's text.
fn gate_clears(item: &OwedView, times: &Times) -> Spans {
    let mut spans = Vec::new();
    if item.conditions.is_empty() {
        spans.push(Span::styled("a person clears it", theme::dim()));
        return spans;
    }
    let lone = timed(&item.conditions);
    spans.push(Span::styled(
        if lone { "clears at " } else { "clears when " },
        theme::dim(),
    ));
    spans.extend(conditions_spans(&item.conditions, times, !lone));
    if item.wakes {
        spans.push(Span::styled(", then wakes the session", theme::dim()));
    }
    spans.extend(delivery_tail(item.met, item.held));
    spans
}

/// Where an item stands in the block's order: what is due, then what waits
/// for staging, for production, for a known time (soonest first), for
/// anything else, and the merge gate last.
fn owed_rank(item: &OwedView) -> (u8, u64) {
    use crate::OwedBlock;
    if item.kind == wt_core::OwedKind::Gate {
        return (5, 0);
    }
    if item.due() {
        return (0, 0);
    }
    // A host on an older build sends no block: read it from the view.
    let block = item.block.unwrap_or(match (item.next_at_ms, item.stage) {
        (Some(_), _) => OwedBlock::Scheduled,
        (None, Some(wt_core::VerifyStage::Staging)) => OwedBlock::WaitingStaging,
        (None, Some(wt_core::VerifyStage::Production)) => OwedBlock::WaitingProduction,
        (None, None) => OwedBlock::WaitingOther,
    });
    match block {
        OwedBlock::Due => (0, 0),
        OwedBlock::WaitingStaging => (1, 0),
        OwedBlock::WaitingProduction => (2, 0),
        OwedBlock::Scheduled => (3, item.next_at_ms.unwrap_or(u64::MAX)),
        OwedBlock::WaitingOther | OwedBlock::Done => (4, 0),
    }
}

/// One owed item, everything in one column after its mark: its "when" with
/// its one detail at the right edge, all of its text, its numbered steps,
/// and for a gate what clears it.
fn owed_item_lines(item: &OwedView, own: &str, times: &Times, width: usize) -> Vec<Line<'static>> {
    let (glyph, mark_color) = owed_mark(item);
    let gate = item.kind == wt_core::OwedKind::Gate;
    let label_color = if item.due() || gate {
        mark_color
    } else {
        theme::FG_MID
    };
    let inner = width.saturating_sub(OWED_LEAD_CELLS).max(1);
    let indent = || Span::raw(" ".repeat(OWED_LEAD_CELLS));
    let mut detail = owed_detail(item);
    let mut head = vec![Span::styled(
        owed_label(item, times),
        theme::fg(label_color),
    )];
    head.extend(owed_tails(
        item,
        own,
        detail.as_ref().map(|(text, _)| text.as_str()),
    ));
    // Too narrow to set the detail apart, it follows the "when".
    if let Some((text, color)) = detail
        .as_ref()
        .filter(|(text, _)| inner < text.width() + 2 + OWED_WHEN_MIN_CELLS)
    {
        head.push(Span::styled(" · ", theme::dim()));
        head.push(Span::styled(text.clone(), theme::fg(*color)));
        detail = None;
    }
    let detail_cells = detail.as_ref().map_or(0, |(text, _)| text.width() + 2);
    let mut lines = wrap_under(
        vec![Span::styled(format!("{glyph} "), theme::fg(mark_color))],
        head,
        width.saturating_sub(detail_cells),
    );
    if let Some((text, color)) = detail
        && let Some(first) = lines.first_mut()
    {
        let pad = width.saturating_sub(first.width() + text.width());
        first.spans.push(Span::raw(" ".repeat(pad)));
        first.spans.push(Span::styled(text, theme::fg(color)));
    }
    let (preamble, steps) = if gate {
        (item.what.clone(), Vec::new())
    } else {
        parse_verify_steps(&item.what)
    };
    let color = if gate {
        theme::WARN
    } else if item.due() {
        theme::FG
    } else {
        theme::FG_MID
    };
    // The author's own lines are kept: a prompt is often a short list.
    for paragraph in preamble
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        for line in wrap(paragraph, inner) {
            lines.push(Line::from(vec![
                indent(),
                Span::styled(line, theme::fg(color)),
            ]));
        }
    }
    lines.extend(step_lines(&steps, item.due(), OWED_LEAD_CELLS, width));
    if gate {
        for line in wrap_at_hyphens(gate_clears(item, times), inner) {
            let mut spans = vec![indent()];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// The one list of what the row owes, in [`owed_rank`] order, each item in
/// full. Nothing owed is nothing to list; a landed row's header says it is
/// done.
fn owes_lines(row: &BoardRow, times: &Times, width: usize) -> Vec<Line<'static>> {
    let mut items: Vec<&OwedView> = row.owes.iter().collect();
    items.sort_by_key(|item| owed_rank(item));
    items
        .into_iter()
        .flat_map(|item| owed_item_lines(item, &row.slug, times, width))
        .collect()
}

/// [`wrap_spans`] for a line that names things by slug: a long word that
/// does not fit where it falls breaks after a hyphen, instead of leaving
/// the line before it short or being cut.
fn wrap_at_hyphens(spans: Spans, width: usize) -> Vec<Line<'static>> {
    /// Shortest word worth breaking inside.
    const LONG: usize = 16;
    if spans_width(&spans) <= width {
        return vec![Line::from(spans)];
    }
    // Words as wrap_spans reads them: split at spaces, with adjacent spans
    // (a mark and its words) one word.
    let mut words: Vec<Spans> = Vec::new();
    for span in spans {
        for (index, word) in span.content.split(' ').enumerate() {
            if index > 0 || words.is_empty() {
                words.push(Vec::new());
            }
            if !word.is_empty()
                && let Some(fragments) = words.last_mut()
            {
                fragments.push(Span::styled(word.to_owned(), span.style));
            }
        }
    }
    let mut lines = Vec::new();
    let mut current: Spans = Vec::new();
    let mut used = 0;
    for word in words {
        let cells = spans_width(&word);
        let gap = usize::from(used > 0);
        let pieces: Spans = if used + gap + cells <= width || cells < LONG {
            word
        } else {
            word.iter()
                .flat_map(|fragment| {
                    fragment
                        .content
                        .split_inclusive('-')
                        .map(|piece| Span::styled(piece.to_owned(), fragment.style))
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        let whole = pieces.len() == 1 || used + gap + cells <= width;
        for (part, piece) in pieces.into_iter().enumerate() {
            let gap = usize::from(part == 0 && used > 0);
            let needed = if whole && part == 0 {
                cells
            } else {
                piece.content.width()
            };
            if used + gap + needed > width && used > 0 {
                lines.push(Line::from(std::mem::take(&mut current)));
                used = 0;
            } else if gap == 1 {
                current.push(Span::raw(" "));
                used += 1;
            }
            let text = truncate_end(&piece.content, width.saturating_sub(used));
            used += text.width();
            current.push(Span::styled(text, piece.style));
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

/// An item's numbered steps from `indent`, each wrapped under its own text.
fn step_lines(steps: &[String], due: bool, indent: usize, width: usize) -> Vec<Line<'static>> {
    let (head_color, body_color) = if due {
        (theme::WARN, theme::FG_MID)
    } else {
        (theme::FG_DIM, theme::FG_DIM)
    };
    let inner = width.saturating_sub(indent).max(1);
    let mut lines = Vec::new();
    let gutter = steps.len().max(1).to_string().len() + 2;
    for (index, step) in steps.iter().enumerate() {
        for (part, line) in wrap(step, inner.saturating_sub(gutter).max(1))
            .into_iter()
            .enumerate()
        {
            let lead = if part == 0 {
                Span::styled(
                    format!("{:<gutter$}", format!("{}.", index + 1)),
                    theme::fg(head_color),
                )
            } else {
                Span::raw(" ".repeat(gutter))
            };
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                lead,
                Span::styled(line, theme::fg(body_color)),
            ]));
        }
    }
    lines
}

/// One dim line per watch that only tells the person. The row does not owe
/// these, so they follow its list instead of sitting in it.
fn notify_lines(row: &BoardRow, times: &Times, width: usize) -> Vec<Line<'static>> {
    let owed = |watch: &WatchView| {
        !watch.id.is_empty()
            && row
                .owes
                .iter()
                .any(|item| item.id.as_deref() == Some(watch.id.as_str()))
    };
    let told: Vec<&WatchView> = row
        .watches
        .iter()
        .filter(|watch| watch.does(WatchActionKind::Notify) && !owed(watch))
        .collect();
    let mut lines = Vec::new();
    for watch in told.iter().take(WATCH_LINES_MAX) {
        let Some(badge) = badges::watch_badge(std::slice::from_ref(*watch)) else {
            continue;
        };
        // One cell after the bell, so its text starts under the header's.
        let bell = Span::styled(format!("{} ", badge.glyph), theme::fg(badge.color));
        let lone = timed(&watch.conditions);
        let mut spans = vec![Span::styled(
            if lone {
                "tells you at "
            } else {
                "tells you when "
            },
            theme::dim(),
        )];
        spans.extend(conditions_spans(&watch.conditions, times, !lone));
        let mut suffix = Vec::new();
        if !watch.by.is_empty() && watch.by != row.slug && watch.by != "human" {
            suffix.push(Span::styled(
                format!(" ·\u{a0}by\u{a0}{}", watch.by),
                theme::dim(),
            ));
        }
        suffix.extend(delivery_tail(watch.met, watch.held));
        if let Some(headline) = watch
            .headline
            .as_deref()
            .map(str::trim)
            .filter(|headline| !headline.is_empty())
        {
            let separator = " · ";
            let used = bell.content.width() + spans_width(&spans) + separator.width();
            spans.push(Span::styled(separator, theme::dim()));
            spans.push(headline_span(
                headline,
                used,
                spans_width(&suffix),
                width,
                theme::FG_DIM,
            ));
        }
        spans.extend(suffix);
        lines.extend(wrap_under(vec![bell], spans, width));
    }
    if told.len() > WATCH_LINES_MAX {
        lines.push(Line::styled(
            format!("  +{} more (wt watch)", told.len() - WATCH_LINES_MAX),
            theme::dim(),
        ));
    }
    lines
}

/// Base-font circles center like the note rail's `│`, unlike the Nerd Font
/// dot, so the banner glyph lines up with the rail beneath it.
fn banner_glyph(state: WorkState) -> &'static str {
    if state == WorkState::Todo {
        glyphs::OWED_WAITING
    } else {
        glyphs::OWED_DUE
    }
}

/// The note behind a state-colored rail. Structured `LABEL:` sections line
/// up under a hanging indent; `REVERT` reads green when safe and red when
/// not, `OPS: none` recedes, and `UNTESTED` warns.
fn note_lines(note: &str, color: ratatui::style::Color, width: usize) -> Vec<Line<'static>> {
    let sections = split_note_sections(note);
    let gutter = sections
        .iter()
        .filter_map(|(label, _)| label.map(str::width))
        .max()
        .map_or(0, |widest| widest + 2);
    let inner = width.saturating_sub(2).max(1);
    // A pane too narrow for the gutter gives each label its own line.
    let stacked = inner < gutter + NOTE_BODY_MIN_CELLS;
    let rail = || Span::styled("│ ", theme::fg(color));
    let mut lines = Vec::new();
    for (label, body) in sections {
        let indent = match label {
            Some(_) if stacked => 2,
            Some(_) => gutter,
            None => 0,
        };
        let body_color = match label {
            Some("REVERT") => {
                let head = body.trim().to_ascii_lowercase();
                if head.starts_with("safe") {
                    theme::OK
                } else if head.starts_with("no:") || head.starts_with("no ") {
                    theme::ERR
                } else {
                    theme::FG_MID
                }
            }
            Some("OPS") => theme::FG_DIM,
            _ => theme::FG_MID,
        };
        let label_color = if label == Some("UNTESTED") {
            theme::WARN
        } else {
            theme::FG_DIM
        };
        if let (Some(label), true) = (label, stacked) {
            lines.push(Line::from(vec![
                rail(),
                Span::styled(truncate_end(label, inner), theme::fg(label_color)),
            ]));
        }
        for (index, line) in wrap(&body, inner.saturating_sub(indent).max(1))
            .into_iter()
            .enumerate()
        {
            let lead = match label {
                Some(label) if index == 0 && !stacked => {
                    Span::styled(format!("{label:<gutter$}"), theme::fg(label_color))
                }
                _ => Span::raw(" ".repeat(indent)),
            };
            lines.push(Line::from(vec![
                rail(),
                lead,
                Span::styled(line, theme::fg(body_color)),
            ]));
        }
    }
    lines
}

/// Split a ready note at `IF WRONG:`, `UNTESTED:`, `REVERT:` and `OPS: none`,
/// where the label starts a word and is followed by whitespace. Text before
/// the first label is the unlabeled lead. What a worktree owes is tracked
/// in its owed list, so any other `OPS` text is prose like the rest.
fn split_note_sections(note: &str) -> Vec<(Option<&'static str>, String)> {
    let note = note.trim();
    let mut cuts = Vec::new();
    for (index, _) in note.char_indices() {
        let starts_word = index == 0 || note[..index].ends_with(char::is_whitespace);
        if !starts_word {
            continue;
        }
        for label in NOTE_LABELS {
            let rest = &note[index..];
            if let Some(after) = rest.strip_prefix(label).and_then(|r| r.strip_prefix(':'))
                && after.starts_with(char::is_whitespace)
                && (label != "OPS" || after.trim_start().to_ascii_lowercase().starts_with("none"))
            {
                cuts.push((index, label));
            }
        }
    }
    let mut sections = Vec::new();
    let lead_end = cuts.first().map_or(note.len(), |(index, _)| *index);
    if !note[..lead_end].trim().is_empty() {
        sections.push((None, note[..lead_end].trim().to_owned()));
    }
    for (position, (start, label)) in cuts.iter().enumerate() {
        let end = cuts
            .get(position + 1)
            .map_or(note.len(), |(index, _)| *index);
        let body = note[start + label.len() + 1..end].trim();
        sections.push((Some(*label), body.to_owned()));
    }
    sections
}

/// Split verification text into a preamble and numbered steps. Steps are
/// `1.`, `2.`, ... in sequence (after a `STEPS:` marker when present), so a
/// stray number in prose does not start a step.
fn parse_verify_steps(text: &str) -> (String, Vec<String>) {
    let text = text.trim();
    let (before, region) = match find_word(text, "STEPS:") {
        Some(index) => (
            text[..index].trim(),
            text[index + "STEPS:".len()..].trim_start(),
        ),
        None => ("", text),
    };
    let mut offsets = Vec::new();
    let mut want = 1u32;
    let bytes = region.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let boundary = index == 0 || bytes[index - 1].is_ascii_whitespace();
        if boundary && bytes[index].is_ascii_digit() {
            let mut end = index;
            while end < bytes.len() && end - index < 2 && bytes[end].is_ascii_digit() {
                end += 1;
            }
            let dotted = bytes.get(end) == Some(&b'.')
                && bytes.get(end + 1).is_some_and(u8::is_ascii_whitespace);
            if dotted && region[index..end].parse::<u32>() == Ok(want) {
                offsets.push(index);
                want += 1;
                index = end + 1;
                continue;
            }
        }
        index += 1;
    }
    if offsets.is_empty() {
        return (text.to_owned(), Vec::new());
    }
    let steps = offsets
        .iter()
        .enumerate()
        .map(|(position, start)| {
            let end = offsets.get(position + 1).copied().unwrap_or(region.len());
            let step = &region[*start..end];
            step.trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches('.')
                .trim()
                .to_owned()
        })
        .collect();
    let preamble = if before.is_empty() {
        region[..offsets[0]].trim()
    } else {
        before
    };
    (preamble.to_owned(), steps)
}

fn find_word(text: &str, word: &str) -> Option<usize> {
    text.match_indices(word)
        .map(|(index, _)| index)
        .find(|index| {
            *index == 0
                || text[..*index]
                    .chars()
                    .next_back()
                    .is_some_and(|c| !c.is_alphanumeric())
        })
}

/// Wrap and keep at most `max` lines, marking a cut with an ellipsis.
fn clip_lines(text: &str, width: usize, max: usize) -> Vec<String> {
    let mut lines = wrap(text, width);
    if lines.len() > max {
        lines.truncate(max);
        if let Some(last) = lines.last_mut() {
            *last = truncate_end(&format!("{last}{}", super::text::ELLIPSIS), width);
            if !last.ends_with(super::text::ELLIPSIS) {
                last.push(super::text::ELLIPSIS);
            }
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// Review requests

fn review_lines(review: &ReviewRequestRow, width: usize) -> Vec<Line<'static>> {
    let mut lines = title_lines(&review.title, None, width);
    lines.push(Line::default());
    let label_width = "branch".len() + 2;
    let value_width = width.saturating_sub(label_width).max(1);
    let row = |label: &str, value: Spans| {
        let mut spans = vec![label_span(label, label_width)];
        spans.extend(value);
        fit_line(spans, width)
    };
    let (glyph, color, state) = if review.draft {
        (glyphs::PR_DRAFT, theme::FG_DIM, "draft")
    } else {
        (glyphs::PR_OPEN, theme::ACCENT_ALT, "ready")
    };
    lines.push(row("state", glyph_text(glyph, state, color)));
    if !review.branch.is_empty() {
        lines.push(row(
            "branch",
            vec![Span::styled(
                truncate_end(&review.branch, value_width),
                theme::fg(theme::FG),
            )],
        ));
    }
    if !review.author.is_empty() {
        lines.push(row(
            "author",
            vec![Span::styled(
                format!("@{}", review.author),
                theme::fg(theme::FG),
            )],
        ));
    }
    if let Some(checks) = badges::check_badge(review.checks) {
        let text = match review.checks {
            CheckState::Pass => "passing",
            CheckState::Fail => "failing",
            _ => "pending",
        };
        lines.push(row("checks", glyph_text(checks.glyph, text, checks.color)));
    }
    for (index, detail) in review.details.iter().enumerate() {
        for (part, line) in wrap(detail, value_width).into_iter().enumerate() {
            let label = if index == 0 && part == 0 { "info" } else { "" };
            lines.push(row(
                label,
                vec![Span::styled(line, theme::fg(theme::FG_MID))],
            ));
        }
    }
    lines.push(Line::default());
    lines.push(Line::styled(truncate_end(&review.url, width), theme::dim()));
    lines.push(Line::styled(
        truncate_end("w checkout · p open PR · d dismiss", width),
        theme::dim(),
    ));
    lines
}

// ---------------------------------------------------------------------------
// Folded sections

/// The Landed block's pane says what its header counts and nothing else:
/// the rows with something due and what that is, the rows that wait under
/// what they wait for, and the rows that are done. What each row asserted
/// before it merged is not this block's business.
fn landed_section_lines(section: &BoardSection, model: &Model, width: usize) -> Vec<Line<'static>> {
    use crate::OwedBlock;
    let times = &model.board.history_times;
    let members: Vec<&BoardRow> = section
        .rows
        .iter()
        .filter_map(|&index| model.board.rows.get(index))
        .collect();
    let mut lines = vec![Line::styled(
        truncate_end(&section.title, width),
        Style::new()
            .fg(theme::FG_BRIGHT)
            .add_modifier(Modifier::BOLD),
    )];
    let overdue = |row: &&BoardRow| row.owes.iter().any(|item| item.overdue);
    let due_color = if members.iter().any(overdue) {
        theme::ERR
    } else {
        theme::WARN
    };
    let counts = super::list::landed_counts(&members);
    let counted: Vec<Spans> = counts
        .into_iter()
        .zip([
            ("due", due_color),
            ("waiting", theme::FG_MID),
            ("done", theme::OK),
        ])
        .filter(|(count, _)| *count > 0)
        .map(|(count, (word, color))| {
            vec![Span::styled(format!("{count} {word}"), theme::fg(color))]
        })
        .collect();
    if counted.is_empty() {
        lines.push(Line::styled("no worktrees", theme::dim()));
    }
    lines.extend(wrap_items(counted, width));
    // A row under its group: its title, and at the right edge what the
    // group's name leaves unsaid.
    let row_line = |row: &BoardRow, right: Option<String>| {
        let right = right.filter(|right| width >= right.width() + 3 + NOTE_BODY_MIN_CELLS);
        let right_cells = right.as_ref().map_or(0, |right| right.width() + 1);
        let room = width.saturating_sub(2 + right_cells);
        let label = truncate_end(&super::list::row_label(row), room);
        let pad = room.saturating_sub(label.width());
        let mut spans = vec![Span::raw("  "), Span::styled(label, theme::fg(theme::FG))];
        if let Some(right) = right {
            spans.push(Span::raw(" ".repeat(pad + 1)));
            spans.push(Span::styled(right, theme::dim()));
        }
        Line::from(spans)
    };
    let in_block = |block: OwedBlock| -> Vec<&BoardRow> {
        members
            .iter()
            .copied()
            .filter(|row| row.owed_block == Some(block))
            .collect()
    };
    let group = |glyph: &str, text: &str, color| {
        [
            Line::default(),
            fit_line(
                vec![
                    Span::styled(format!("{glyph} "), theme::fg(color)),
                    Span::styled(text.to_owned(), theme::bold(color)),
                ],
                width,
            ),
        ]
    };
    let due = in_block(OwedBlock::Due);
    if !due.is_empty() {
        lines.extend(group(glyphs::OWED_DUE, "due", due_color));
    }
    for row in due {
        lines.push(row_line(row, None));
        // In the list's own order: due checks by stage, then due steps.
        for item in row.owes.iter().filter(|item| item.due()) {
            let (_, color) = owed_mark(item);
            lines.push(fit_line(
                vec![
                    Span::raw("    "),
                    Span::styled(owed_label(item, times), theme::fg(color)),
                    Span::raw("  "),
                    Span::styled(item.headline.clone(), theme::fg(theme::FG_MID)),
                ],
                width,
            ));
        }
    }
    for block in [
        OwedBlock::WaitingStaging,
        OwedBlock::WaitingProduction,
        OwedBlock::Scheduled,
        OwedBlock::WaitingOther,
    ] {
        let rows = in_block(block);
        if rows.is_empty() {
            continue;
        }
        lines.extend(group(
            glyphs::OWED_CLOCK,
            waiting_words(block),
            theme::FG_MID,
        ));
        for row in rows {
            // The stage is the group's name; a time or other work is not.
            let next = matches!(block, OwedBlock::Scheduled | OwedBlock::WaitingOther)
                .then(|| row.owes.iter().min_by_key(|item| owed_rank(item)))
                .flatten()
                .map(|item| owed_label(item, times));
            lines.push(row_line(row, next));
        }
    }
    let done = in_block(OwedBlock::Done);
    if !done.is_empty() {
        lines.extend(group(glyphs::OWED_DONE, "done · c cleans up", theme::OK));
    }
    for row in done {
        lines.push(row_line(row, None));
    }
    lines.push(Line::default());
    let hint = if section.folded {
        "TAB expand · y yank"
    } else {
        "TAB fold · y yank"
    };
    lines.push(Line::styled(truncate_end(hint, width), theme::dim()));
    lines
}

fn section_lines(section: &BoardSection, model: &Model, width: usize) -> Vec<Line<'static>> {
    if section.key == crate::LANDED_SECTION {
        return landed_section_lines(section, model, width);
    }
    let policy = &model.board.display;
    let mut lines = vec![Line::styled(
        truncate_end(&section.title, width),
        Style::new()
            .fg(theme::FG_BRIGHT)
            .add_modifier(Modifier::BOLD),
    )];
    let members = section
        .rows
        .iter()
        .filter_map(|&index| model.board.rows.get(index))
        .collect::<Vec<_>>();
    let (summary, blocked) = section_summary(section, &members, width);
    lines.extend(summary);
    lines.push(Line::default());
    if members.is_empty() {
        lines.push(Line::styled("no worktrees", theme::dim()));
    }
    let risk_cells = members
        .iter()
        .filter_map(|row| member_risk(row))
        .map(|risk| risk.as_str().len() + 2)
        .max()
        .unwrap_or(0);
    for row in members {
        let marker = badges::marker(row, policy);
        let slots = badges::cluster(row, policy);
        let cluster_cells = badges::cluster_width(&slots);
        let label_cells = width.saturating_sub(3 + risk_cells + cluster_cells);
        let label = truncate_end(&super::list::row_label(row), label_cells);
        let pad = label_cells.saturating_sub(label.width());
        let mut spans = vec![
            Span::styled(format!("{}  ", marker.glyph), theme::fg(marker.color)),
            Span::styled(
                label,
                theme::fg(if row.archived {
                    theme::FG_DIM
                } else {
                    theme::FG
                }),
            ),
            Span::raw(" ".repeat(pad)),
        ];
        if risk_cells > 0 {
            let risk = member_risk(row);
            let text = risk.map_or(String::new(), |risk| format!(" {}", risk.as_str()));
            spans.push(Span::styled(
                format!("{text:<risk_cells$}"),
                risk.map_or(theme::dim(), |risk| theme::fg(badges::risk_color(risk))),
            ));
        }
        spans.extend(badges::cluster_spans(&slots, None));
        lines.push(fit_line(spans, width));
    }
    lines.extend(blocked);
    lines.push(Line::default());
    // The archived block is pinned and named by wt, so rename and move do
    // nothing there; advertise only the keys it has.
    let hint = match (
        section.folded,
        section.key == ARCHIVED_SECTION || section.owed_block(),
        section.rows.is_empty(),
    ) {
        (_, false, true) => "L rename · D delete · J/K move",
        (true, true, _) => "TAB expand · y yank",
        (true, false, _) => "TAB expand · y yank · L rename · D delete · J/K move",
        (false, true, _) => "TAB fold · y yank",
        (false, false, _) => "TAB fold · y yank · L rename · D delete · J/K move",
    };
    lines.push(Line::styled(truncate_end(hint, width), theme::dim()));
    lines
}

/// Risk is the merge decision, so a member shows it only once ready.
fn member_risk(row: &BoardRow) -> Option<wt_core::WorkRisk> {
    let work = row.work.as_ref()?;
    (work.effective_state == Some(WorkState::Ready))
        .then(|| work.record.as_ref().and_then(|record| record.risk))
        .flatten()
}

/// Rollup lines for tests that read them without a pane width.
#[cfg(test)]
pub(crate) fn section_rollup_lines(
    section: &BoardSection,
    members: &[&BoardRow],
) -> Vec<Line<'static>> {
    let (mut lines, blocked) = section_summary(section, members, 200);
    lines.extend(blocked);
    lines
}

/// The batch view: work-state rollup (most urgent first, glyph per state),
/// risk counts, mechanical facts that decide whether the batch can move,
/// and blocker notes: the needs-human members' notes (what is asked of
/// you) and then the external gates. Returned as (summary, blocked block).
fn section_summary(
    section: &BoardSection,
    members: &[&BoardRow],
    width: usize,
) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    let rollup = &crate::SectionRollup::of(members);
    let count = section.rows.len();
    let mut states = vec![Span::styled(
        format!("{count} worktree{}", if count == 1 { "" } else { "s" }),
        theme::dim(),
    )];
    for entry in ranked_states(&rollup.states) {
        states.push(Span::styled(" · ", theme::dim()));
        let (glyph, color, name) = match entry.state {
            Some(state) => (
                badges::work_state_glyph(state),
                badges::work_state_color(state),
                state.as_str(),
            ),
            None => (glyphs::DOT_OUTLINE, theme::FG_DIM, "unset"),
        };
        states.push(Span::styled(glyph, theme::fg(color)));
        // No-break spaces keep a glyph, its count, and its state together
        // when the summary wraps.
        states.push(Span::styled(
            format!("\u{a0}\u{a0}{}\u{a0}{name}", entry.count),
            theme::dim(),
        ));
    }
    let mut lines = wrap_spans(states, width);
    if !rollup.risks.is_empty() {
        let mut risks = vec![Span::styled("risk ", theme::dim())];
        for (index, entry) in rollup.risks.iter().enumerate() {
            if index > 0 {
                risks.push(Span::styled(" · ", theme::dim()));
            }
            risks.push(Span::styled(
                format!("{} {}", entry.count, entry.risk.as_str()),
                theme::fg(badges::risk_color(entry.risk)),
            ));
        }
        lines.extend(wrap_spans(risks, width));
    }
    let mut facts: Vec<(String, ratatui::style::Color)> = Vec::new();
    let mut fact = |count: usize, text: &str, color| {
        if count > 0 {
            facts.push((format!("{count} {text}"), color));
        }
    };
    if rollup.open_prs > 0 {
        let drafts = if rollup.draft_prs > 0 {
            format!(" ({} draft)", rollup.draft_prs)
        } else {
            String::new()
        };
        fact(
            rollup.open_prs,
            &format!(
                "PR{} open{drafts}",
                if rollup.open_prs == 1 { "" } else { "s" }
            ),
            theme::FG_DIM,
        );
    }
    fact(rollup.queued_prs, "queued", theme::INFO);
    fact(rollup.failing_checks, "checks failing", theme::ERR);
    fact(rollup.dirty_worktrees.unwrap_or(0), "dirty", theme::WARN);
    fact(rollup.unknown_git, "Git unknown", theme::FG_DIM);
    fact(rollup.upstream_ahead, "ahead upstream", theme::FG_DIM);
    fact(rollup.upstream_behind, "behind upstream", theme::FG_DIM);
    fact(rollup.rebasing, "rebasing", theme::WARN);
    fact(rollup.conflicted, "conflicted", theme::ERR);
    fact(
        rollup.stale_statuses,
        if rollup.stale_statuses == 1 {
            "stale status"
        } else {
            "stale statuses"
        },
        theme::WARN,
    );
    fact(rollup.owed_due, "due", theme::WARN);
    fact(
        rollup.owed - rollup.owed_due,
        "owed, waiting",
        theme::FG_DIM,
    );
    fact(rollup.owed_overdue, "overdue", theme::ERR);
    fact(rollup.paused_automations, "paused", theme::WARN);
    fact(
        rollup.needs_attention,
        if rollup.needs_attention == 1 {
            "needs attention"
        } else {
            "need attention"
        },
        theme::WARN,
    );
    if !facts.is_empty() {
        let mut spans = Vec::new();
        for (index, (text, color)) in facts.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", theme::dim()));
            }
            spans.push(Span::styled(text, theme::fg(color)));
        }
        lines.extend(wrap_spans(spans, width));
    }
    let mut blocked = Vec::new();
    let asking = members
        .iter()
        .filter_map(|row| {
            let work = row.work.as_ref()?;
            let note = work.record.as_ref()?.note.as_deref()?.trim();
            (work.effective_state == Some(WorkState::NeedsHuman) && !note.is_empty())
                .then_some((*row, note))
        })
        .collect::<Vec<_>>();
    if !asking.is_empty() {
        blocked.push(Line::default());
        blocked.push(fit_line(
            glyph_text(glyphs::CONFLICT, "blocked on you", theme::ERR),
            width,
        ));
        for (row, note) in asking {
            blocked.push(Line::styled(
                truncate_end(&format!("  {}", super::list::row_label(row)), width),
                theme::dim(),
            ));
            blocked.extend(
                clip_lines(note, width.saturating_sub(4).max(1), SECTION_NOTE_LINES)
                    .into_iter()
                    .map(|line| Line::styled(format!("    {line}"), theme::fg(theme::FG))),
            );
        }
    }
    if !rollup.blocked_notes.is_empty() {
        blocked.push(Line::default());
        blocked.push(fit_line(
            glyph_text(glyphs::CONFLICT, "blocked on", theme::WARN),
            width,
        ));
        for note in &rollup.blocked_notes {
            blocked.extend(
                clip_lines(note, width.saturating_sub(2).max(1), SECTION_NOTE_LINES)
                    .into_iter()
                    .map(|line| Line::styled(format!("  {line}"), theme::fg(theme::FG))),
            );
        }
    }
    (lines, blocked)
}

/// The rollup's states, most urgent first by the shared section
/// rank (ready, needs-human, needs-testing, review, working, unset, todo,
/// then the terminal states), so the pane and the folded list header agree.
pub(crate) fn ranked_states(
    states: &[crate::WorkStateCount],
) -> impl Iterator<Item = &crate::WorkStateCount> {
    let mut ranked = states.iter().collect::<Vec<_>>();
    ranked.sort_by_key(|entry| wt_core::work_state_rank(entry.state));
    ranked.into_iter()
}

// ---------------------------------------------------------------------------
// Cell-accurate helpers

/// `glyph  text` in one color; the glyph's slot is two cells (see `glyphs`).
fn glyph_text(glyph: &str, text: &str, color: ratatui::style::Color) -> Spans {
    vec![Span::styled(format!("{glyph}  {text}"), theme::fg(color))]
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

/// Clip spans to `width` cells, marking the cut with an ellipsis.
fn fit_line(spans: Spans, width: usize) -> Line<'static> {
    if spans_width(&spans) <= width {
        return Line::from(spans);
    }
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let cells = span.content.width();
        if used + cells < width {
            used += cells;
            out.push(span);
            continue;
        }
        // A span that exactly fills the row still has spans cut after it.
        let room = width - used;
        let text = if cells == room {
            truncate_end(&format!("{}{}", span.content, super::text::ELLIPSIS), room)
        } else {
            truncate_end(&span.content, room)
        };
        out.push(Span::styled(text, span.style));
        break;
    }
    Line::from(out)
}

/// `prefix` then `spans`, wrapped so that later lines start under the text
/// after the prefix.
fn wrap_under(prefix: Spans, spans: Spans, width: usize) -> Vec<Line<'static>> {
    let indent = spans_width(&prefix);
    let mut prefix = Some(prefix);
    wrap_spans(spans, width.saturating_sub(indent).max(1))
        .into_iter()
        .map(|line| {
            let mut spans = prefix
                .take()
                .unwrap_or_else(|| vec![Span::raw(" ".repeat(indent))]);
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Join `items` with dim ` · ` separators, wrapping only between items: a
/// line never starts with a separator, and later lines start under the
/// first item's text, past its glyph. An item wider than a whole line
/// wraps at its own spaces.
fn wrap_items(items: Vec<Spans>, width: usize) -> Vec<Line<'static>> {
    const SEPARATOR: &str = " · ";
    let hang = items
        .first()
        .and_then(|item| item.first().filter(|_| item.len() > 1))
        .map_or(0, |glyph| glyph.content.width());
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Spans = Vec::new();
    let mut used = 0;
    // Whether `current` holds an item, not only its indent.
    let mut said = false;
    for item in items {
        let cells = spans_width(&item);
        if said && used + SEPARATOR.width() + cells <= width {
            current.push(Span::styled(SEPARATOR, theme::dim()));
            used += SEPARATOR.width();
        } else if said {
            lines.push(Line::from(std::mem::take(&mut current)));
            current.push(Span::raw(" ".repeat(hang)));
            used = hang;
        }
        if used + cells <= width {
            current.extend(item);
            used += cells;
        } else {
            let mut wrapped = wrap_under(std::mem::take(&mut current), item, width);
            let last = wrapped.pop().unwrap_or_default();
            lines.extend(wrapped);
            used = last.width();
            current = last.spans;
        }
        said = true;
    }
    if said {
        lines.push(Line::from(current));
    }
    lines
}

/// Wrap a styled line at span boundaries and spaces.
pub(crate) fn wrap_spans(spans: Spans, width: usize) -> Vec<Line<'static>> {
    if spans_width(&spans) <= width {
        return vec![Line::from(spans)];
    }
    // Words break only at spaces; adjacent spans with none between them
    // (a glyph and its count) stay one word.
    let mut words: Vec<(bool, Spans)> = Vec::new();
    for span in spans {
        for (index, word) in span.content.split(' ').enumerate() {
            if index > 0 || words.is_empty() {
                words.push((index > 0, Vec::new()));
            }
            if !word.is_empty()
                && let Some((_, fragments)) = words.last_mut()
            {
                fragments.push(Span::styled(word.to_owned(), span.style));
            }
        }
    }
    let mut lines = Vec::new();
    let mut current: Spans = Vec::new();
    let mut used = 0;
    for (spaced, fragments) in words {
        let cells = spans_width(&fragments) + usize::from(spaced);
        if used + cells > width && used > 0 {
            lines.push(Line::from(std::mem::take(&mut current)));
            used = 0;
        } else if spaced && used > 0 {
            current.push(Span::raw(" "));
            used += 1;
        }
        for fragment in fragments {
            let room = width.saturating_sub(used);
            let text = truncate_end(&fragment.content, room);
            used += text.width();
            current.push(Span::styled(text, fragment.style));
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

/// One `·`-separated piece of a dense line: modes from most verbose to most
/// compact, dropped entirely after the last.
struct Segment {
    tier: u8,
    modes: Vec<Spans>,
    mode: usize,
}

impl Segment {
    fn new(tier: u8, modes: Vec<Spans>) -> Self {
        Self {
            tier,
            modes,
            mode: 0,
        }
    }

    fn width(&self) -> usize {
        self.modes
            .get(self.mode)
            .map_or(0, |mode| spans_width(mode))
    }

    fn dropped(&self) -> bool {
        self.mode >= self.modes.len()
    }
}

/// Step down the least important segment (highest tier, then widest) until
/// the joined line fits, so a narrow pane loses ages before sync counts.
fn fit_segments(mut segments: Vec<Segment>, width: usize) -> Spans {
    loop {
        let alive = segments.iter().filter(|segment| !segment.dropped()).count();
        let total =
            segments.iter().map(Segment::width).sum::<usize>() + 3 * alive.saturating_sub(1);
        if total <= width {
            break;
        }
        let Some(victim) = segments
            .iter_mut()
            .filter(|segment| !segment.dropped() && segment.tier > 1)
            .max_by_key(|segment| (segment.tier, segment.width()))
        else {
            break;
        };
        victim.mode += 1;
    }
    let mut spans = Vec::new();
    for segment in segments.into_iter().filter(|segment| !segment.dropped()) {
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        let mode = segment.mode;
        spans.extend(segment.modes.into_iter().nth(mode).unwrap_or_default());
    }
    spans
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::{Terminal, backend::TestBackend};
    use wt_core::{WorkRisk, WorkStatusRecord};

    use super::*;
    use crate::{Board, DiffStat, GitPresentation, PrCommentView, TitleSource, WorkPresentation};

    #[test]
    fn pr_line_guide_shrinks_to_its_glyph_then_drops_before_pr_facts() {
        let row = BoardRow {
            pr: Some(PrPresentation {
                number: Some(1234),
                state: Some("OPEN".into()),
                checks: CheckState::Pass,
                ..Default::default()
            }),
            guide: Some(GuideView {
                url: "https://guides.example/g".into(),
                current: Some(false),
            }),
            ..Default::default()
        };
        let policy = DisplayPolicy {
            guides: true,
            ..Default::default()
        };
        let text = |row: &BoardRow, policy: &DisplayPolicy, width| {
            pr_value(row, policy, width)
                .concat()
                .iter()
                .map(|span| span.content.to_string())
                .collect::<String>()
        };
        let book = glyphs::BOOK;
        let wide = text(&row, &policy, 80);
        assert!(wide.contains("#1234") && wide.ends_with(&format!("{book}  guide")));
        let narrow = text(&row, &policy, wide.width() - 2);
        assert!(narrow.contains("checks") && narrow.ends_with(&format!("{book}  ")));
        let tight = text(&row, &policy, narrow.width() - 4);
        assert!(tight.contains("checks") && !tight.contains(book));
        // Off, or shown in its own row, the PR line carries no guide.
        let off = DisplayPolicy::default();
        assert!(!text(&row, &off, 80).contains(book));
        let mut own_row = row.clone();
        own_row.detail_groups.push(PreparedDetailGroup {
            id: "guide".into(),
            ..Default::default()
        });
        assert!(!text(&own_row, &policy, 80).contains(book));
    }

    #[test]
    fn a_pull_request_dropped_from_the_merge_queue_says_why_on_its_line() {
        let why = "removed from the merge queue: merge_conflict";
        let row = BoardRow {
            pr: Some(PrPresentation {
                number: Some(1234),
                state: Some("OPEN".into()),
                checks: CheckState::Pass,
                queue_removal: Some(why.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let line = |row: &BoardRow, width| pr_value(row, &DisplayPolicy::default(), width).concat();
        let text = |row: &BoardRow, width| {
            line(row, width)
                .iter()
                .map(|span| span.content.to_string())
                .collect::<String>()
        };
        let wide = line(&row, 100);
        let said = wide
            .iter()
            .find(|span| span.content.contains(why))
            .expect("the reason is on the PR line");
        assert_eq!(said.style.fg, Some(theme::WARN));
        // A narrow pane keeps the fact and drops the reason.
        let narrow = text(&row, 30);
        assert!(
            narrow.contains("removed") && !narrow.contains("merge_conflict"),
            "{narrow}"
        );
        let mut queued = row.clone();
        queued.pr.as_mut().unwrap().queue_removal = None;
        assert!(!text(&queued, 100).contains("removed"));
    }

    #[test]
    fn a_span_that_exactly_fills_the_row_still_marks_the_cut() {
        let line = fit_line(vec![Span::raw("abcd"), Span::raw("ef"), Span::raw("gh")], 6);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "abcde…");
        assert_eq!(
            fit_line(vec![Span::raw("abcd"), Span::raw("ef")], 6).width(),
            6,
            "a line that fits is untouched"
        );
    }

    fn group(id: &str, lines: &[&str]) -> PreparedDetailGroup {
        PreparedDetailGroup {
            id: id.into(),
            label: id.into(),
            lines: lines.iter().map(|line| (*line).to_owned()).collect(),
            error: None,
        }
    }

    fn row() -> BoardRow {
        BoardRow {
            key: "one".into(),
            slug: "eng-12-fix".into(),
            title: "Fix the flaky login test".into(),
            title_source: TitleSource::Agent,
            branch: "feature/eng-12".into(),
            path: "/very/long/path/to/the/worktrees/eng-12-fix".into(),
            issue_id: Some("ENG-12".into()),
            issue_status: Some("In Progress".into()),
            work: Some(WorkPresentation {
                record: Some(WorkStatusRecord {
                    risk: Some(WorkRisk::Low),
                    note: Some("Lead sentence. OPS: none REVERT: safe".into()),
                    ..WorkStatusRecord::new(WorkState::Ready, "2026-10-09T00:00:00Z")
                }),
                effective_state: Some(WorkState::Ready),
                age: Some("2h".into()),
                ..Default::default()
            }),
            git: GitPresentation {
                tracked_changes: Some(1),
                untracked_files: Some(0),
                upstream: Some("origin/feature/eng-12".into()),
                ahead: Some(2),
                behind: Some(0),
                base_ahead: Some(3),
                base_behind: Some(1),
                diff: Some(DiffStat {
                    files: 2,
                    added: 10,
                    removed: 4,
                }),
                base_conflicts: Some(vec!["src/login.rs".into()]),
                ..Default::default()
            },
            pr: Some(PrPresentation {
                number: Some(42),
                state: Some("OPEN".into()),
                checks: CheckState::Fail,
                failed_checks: vec!["lint".into()],
                comments: vec![PrCommentView {
                    author: "reviewer".into(),
                    body: "Please add a test.".into(),
                    age: Some("3h".into()),
                }],
                unresolved_threads: 2,
                ..Default::default()
            }),
            detail_groups: vec![
                group("branch", &["feature/eng-12", "main"]),
                group("issue", &[]),
                group("pr", &[]),
                group("claude", &[]),
                group("git", &[]),
            ],
            ..Default::default()
        }
    }

    fn screen(row: BoardRow, width: u16, height: u16) -> Vec<String> {
        let mut model = Model {
            board: Arc::new(Board {
                rows: vec![row],
                display: DisplayPolicy {
                    primary_harness: "Codex".into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
            selected: Some(0),
            ..Model::default()
        };
        model.rebuild_items();
        model.selected = Some(0);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut model, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect()
    }

    fn find<'a>(lines: &'a [String], needle: &str) -> &'a str {
        lines
            .iter()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} missing from:\n{}", lines.join("\n")))
    }

    #[test]
    fn title_carries_its_source_and_the_border_names_the_slug() {
        let lines = screen(row(), 70, 40);
        assert!(lines[0].contains("eng-12-fix"), "{}", lines[0]);
        assert!(!find(&lines, "Fix the flaky login test").contains("(agent)"));
    }

    #[test]
    fn labels_are_right_aligned_in_one_column_with_shared_glyphs() {
        let lines = screen(row(), 70, 40);
        // Widest label "branch" plus one gap: values start at the same cell.
        let branch = find(&lines, "branch feature/eng-12 → main");
        let git = find(&lines, "   git ");
        let pr = find(&lines, "    pr ");
        let ai = find(&lines, "    ai ");
        let column = |line: &str, label: &str| line.find(label).unwrap() + label.len();
        let start = column(branch, "branch ");
        assert_eq!(column(git, "   git "), start);
        assert_eq!(column(pr, "    pr "), start);
        assert_eq!(column(ai, "    ai "), start);
        assert!(pr.contains(&format!("{}  #42", glyphs::PR_OPEN)), "{pr}");
        assert!(
            pr.contains(&format!("{}  checks: lint", glyphs::CHECK_FAIL)),
            "{pr}"
        );
        assert!(git.contains(&format!("{}  dirty", glyphs::PENCIL)), "{git}");
        assert!(git.contains("+10 −4 (2 files)"), "{git}");
        assert!(git.contains("(↑2) [↑3 ↓1]"), "{git}");
        assert!(ai.contains("primary: Codex · F12 to start"), "{ai}");
        assert!(find(&lines, "issue").contains("#ENG-12 · In Progress"));
    }

    #[test]
    fn work_status_conflicts_and_comments_render_as_blocks() {
        let lines = screen(row(), 70, 40);
        assert!(find(&lines, "● ready").contains("risk low · 2h ago"));
        assert!(find(&lines, "│ Lead sentence.").starts_with("│ │ Lead"));
        assert!(find(&lines, "OPS").contains("OPS     none"));
        assert!(find(&lines, "Won't rebase cleanly onto main").contains("1 conflicting file"));
        assert!(find(&lines, "src/login.rs").contains("   src/login.rs"));
        assert!(find(&lines, "@reviewer").contains("@reviewer · 3h ago"));
        find(&lines, "+2 unresolved threads");
        // A count GitHub cut short is a lower bound.
        let mut more = row();
        more.pr.as_mut().unwrap().unresolved_threads_more = true;
        find(&screen(more, 70, 40), "+2 or more unresolved threads");
    }

    #[test]
    fn a_row_holding_another_pr_says_so_in_warn_colour() {
        let mut holding = row();
        assert!(other_prs_block(&holding, 80).is_empty());
        holding.other_prs = vec![
            crate::OtherPrView {
                branch: "michael/retire-x".into(),
                number: Some(12),
                state: wt_core::OtherPrState::Open,
                draft: true,
            },
            crate::OtherPrView {
                branch: "michael/landed".into(),
                number: Some(9),
                state: wt_core::OtherPrState::Merged,
                draft: false,
            },
            crate::OtherPrView {
                branch: "michael/pushed".into(),
                ..crate::OtherPrView::default()
            },
        ];
        let block = other_prs_block(&holding, 200);
        assert_eq!(block.len(), 1);
        let text = block[0].to_string();
        assert!(
            text.ends_with(
                "Holds more than one PR: also draft #12 on michael/retire-x, michael/pushed (PR unknown)"
            ),
            "{text}"
        );
        assert_eq!(block[0].style.fg, Some(theme::WARN));
        find(&screen(holding, 120, 40), "Holds more than one PR");
    }

    #[test]
    fn conflict_files_keep_their_name_and_headers_wrap_past_the_glyph() {
        let mut clashing = row();
        clashing.git.base_conflicts = Some(vec![
            "crates/app/src/features/authentication/login_form.rs".into(),
        ]);
        let policy = DisplayPolicy::default();
        let text = |lines: Vec<Line<'static>>| -> Vec<String> {
            lines.iter().map(ToString::to_string).collect()
        };
        let block = text(rebase_block(&clashing, &policy, 30));
        assert!(block.iter().all(|line| line.width() <= 30), "{block:?}");
        let file = block.last().unwrap();
        assert!(
            file.starts_with("   …") && file.ends_with("login_form.rs"),
            "{file}"
        );
        // The count is the tail of the header: wrapped, never cut.
        assert!(block.len() > 2, "{block:?}");
        assert!(block[0].starts_with(&format!("{}  Won't", glyphs::CONFLICT)));
        let header = &block[..block.len() - 1];
        assert!(header.join(" ").contains("1 conflicting file"), "{block:?}");
        for line in &header[1..] {
            assert!(
                line.starts_with("   ") && !line.starts_with("    "),
                "{line:?}"
            );
        }

        let mut rebasing = row();
        rebasing.git.base_conflicts = None;
        rebasing.git.rebasing = true;
        rebasing.git.conflict_files = vec!["src/a.rs".into()];
        let block = text(rebase_block(&rebasing, &policy, 30));
        assert!(block[0].contains("  Mid-rebase:"), "{block:?}");
        assert!(block[1].starts_with("   ") && !block[1].starts_with("    "));
        assert_eq!(block.last().unwrap(), "   src/a.rs");
    }

    #[test]
    fn prose_blocks_are_body_gray_not_metadata_gray() {
        let mut prose = row();
        prose.details = vec!["A leftover detail line.".into()];
        prose.sessions = vec![SessionView {
            summary: Some("Session summary prose.".into()),
            ..Default::default()
        }];
        let model = Model {
            board: Arc::new(Board {
                rows: vec![prose],
                ..Default::default()
            }),
            ..Model::default()
        };
        let lines = row_lines(&model.board.rows[0], &model, 60);
        for needle in ["A leftover detail line.", "Session summary prose."] {
            let line = lines
                .iter()
                .find(|line| line.to_string() == needle)
                .unwrap_or_else(|| panic!("{needle} missing"));
            assert_eq!(line.style.fg, Some(theme::FG_MID), "{needle}");
        }
    }

    #[test]
    fn section_facts_agree_in_number() {
        let member = |flagged: bool| BoardRow {
            needs_attention: flagged,
            work: Some(WorkPresentation {
                effective_state: Some(WorkState::Working),
                stale: Some(flagged),
                ..Default::default()
            }),
            ..Default::default()
        };
        let one = section_screen(vec![member(true), member(false)], "manual").join("\n");
        assert!(one.contains("1 stale status · 1 needs attention"), "{one}");
        let two = section_screen(vec![member(true), member(true)], "manual").join("\n");
        assert!(two.contains("2 stale statuses · 2 need attention"), "{two}");
    }

    #[test]
    fn narrow_panes_drop_low_priority_git_segments_first() {
        let lines = screen(row(), 34, 40);
        let git = find(&lines, "   git ");
        // The sticky verb survives; ages and the base counts go first.
        assert!(git.contains(&format!("{}  dirty", glyphs::PENCIL)), "{git}");
        assert!(!git.contains("committed"), "{git}");
        for line in &lines {
            assert_eq!(line.width(), 34);
        }
    }

    #[test]
    fn note_sections_and_verify_steps_parse() {
        let sections =
            split_note_sections("Ships it. OPS: none REVERT: no: migration IF WRONG: 500s");
        assert_eq!(
            sections,
            [
                (None, "Ships it.".to_owned()),
                (Some("OPS"), "none".to_owned()),
                (Some("REVERT"), "no: migration".to_owned()),
                (Some("IF WRONG"), "500s".to_owned()),
            ]
        );
        // What a worktree owes is tracked in its list: the phased labels of
        // older notes are prose, and only "OPS: none" still stands apart.
        let phased = split_note_sections(
            "Ships it.\nOPS BEFORE MERGE: rotate key\nOPS: run the backfill\nREVERT: safe",
        );
        assert_eq!(
            phased,
            [
                (
                    None,
                    "Ships it.\nOPS BEFORE MERGE: rotate key\nOPS: run the backfill".to_owned()
                ),
                (Some("REVERT"), "safe".to_owned()),
            ]
        );
        let rendered: Vec<String> = note_lines("Ships it. OPS: none REVERT: safe", theme::OK, 40)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            rendered,
            ["│ Ships it.", "│ OPS     none", "│ REVERT  safe"]
        );
        let narrow = note_lines("UNTESTED: the rotation of the key", theme::OK, 12);
        assert!(narrow.iter().all(|line| line.width() <= 12), "{narrow:?}");
        let (preamble, steps) = parse_verify_steps(
            "Check prod after deploy. STEPS: 1. open /health 2. expect 200 within 30s",
        );
        assert_eq!(preamble, "Check prod after deploy.");
        assert_eq!(steps, ["open /health", "expect 200 within 30s"]);
        assert_eq!(truncate_start("/a/b/worktree", 8), "…orktree");
    }

    #[test]
    fn scroll_is_clamped_to_the_content() {
        let mut model = Model {
            board: Arc::new(Board {
                rows: vec![row()],
                ..Default::default()
            }),
            selected: Some(0),
            details_scroll: 500,
            ..Model::default()
        };
        model.rebuild_items();
        model.selected = Some(0);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut model, frame.area()))
            .unwrap();
        assert!(model.details_scroll > 0 && model.details_scroll < 500);
    }

    #[test]
    fn content_starts_below_a_blank_row_and_the_title_is_regular_weight() {
        let mut plain = row();
        plain.work = None;
        for row in [row(), plain] {
            let lines = screen(row, 70, 40);
            assert!(lines[1].trim_matches(|c| c == '│' || c == ' ').is_empty());
            assert!(
                lines[2].contains("Fix the flaky login test"),
                "{}",
                lines[2]
            );
        }
        let title = title_lines("Fix it", None, 40);
        assert!(!title[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(title[0].style.fg, Some(theme::FG_BRIGHT));
    }

    #[test]
    fn landed_rows_name_the_base_they_merged_into() {
        let mut landed = row();
        landed.git.landed_on = Some(crate::LandingKind::Base);
        landed.detail_groups[0] = group("branch", &["feature/eng-12", "origin/main"]);
        let lines = screen(landed, 90, 40);
        assert!(find(&lines, "   git ").contains("merged into origin/main"));
    }

    fn waits(key: &str, text: &str, state: WatchTruth) -> WatchConditionView {
        WatchConditionView {
            key: key.into(),
            text: text.into(),
            state,
            ..Default::default()
        }
    }

    /// Lines as text, with the no-break spaces that hold a mark to its words
    /// written as plain ones.
    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.to_string().replace('\u{a0}', " "))
            .collect()
    }

    use wt_core::{OwedKind, OwedPhase, OwedState, VerifyStage};

    fn check(stage: VerifyStage, state: OwedState, steps: &str) -> OwedView {
        let mut check = OwedView::new(OwedKind::Check, state, steps);
        check.stage = Some(stage);
        check
    }

    fn step(state: OwedState, what: &str) -> OwedView {
        let mut step = OwedView::new(OwedKind::Step, state, what);
        step.by = Some("eng-12-fix".into());
        step
    }

    fn gate(what: &str) -> OwedView {
        OwedView::new(OwedKind::Gate, OwedState::Waiting, what)
    }

    fn landed(phase: OwedPhase, owes: Vec<OwedView>) -> BoardRow {
        let mut row = row();
        row.git.landed_on = Some(crate::LandingKind::Base);
        row.owed_phase = Some(phase);
        row.owes = owes;
        row
    }

    /// The status banner, then what the row owes as the pane lists it
    /// after the facts.
    fn banner(row: &BoardRow, times: &Times, width: usize) -> Vec<Line<'static>> {
        let mut lines = work_status_block(row, width);
        lines.extend(owes_block(row, times, width));
        lines
    }

    fn block(row: &BoardRow, width: usize) -> Vec<String> {
        plain(&banner(row, &Times::new(), width))
    }

    /// The color of the first span holding `needle`.
    fn color_of(lines: &[Line<'_>], needle: &str) -> Option<ratatui::style::Color> {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains(needle))
            .and_then(|span| span.style.fg)
    }

    /// A row owing one of everything, as the pane's examples show it.
    fn owing_row() -> BoardRow {
        let labelled = |key: &str, text: &str, label: &str, state| WatchConditionView {
            label: label.into(),
            ..waits(key, text, state)
        };
        let mut clears = gate("prod secret missing from the vault the worker reads");
        clears.id = Some("gate22".into());
        clears.conditions = vec![waits(
            "#3806:production",
            "#3806 in production",
            WatchTruth::NotYet,
        )];
        let mut merged = step(
            OwedState::Waiting,
            "Before the release carrying #3879: confirm api-prod's add-on route answers 200 for a brand with the add-on, and 404 without it.",
        );
        merged.before_release = true;
        merged.conditions = vec![labelled("merged", "merged", "merged", WatchTruth::NotYet)];
        let mut soon = labelled("at:1800000000000", "17:00", "17:00", WatchTruth::NotYet);
        soon.at_ms = Some(1_800_000_000_000);
        let mut timed = step(OwedState::Waiting, "Run the smoke test against staging");
        timed.by = Some("manager".into());
        timed.conditions = vec![soon];
        timed.next_at_ms = Some(1_800_000_000_000);
        timed.due_in = Some("2h 10m".into());
        let mut due = step(
            OwedState::Due,
            "confirm prod logs, then open the PR retiring the old path",
        );
        due.id = Some("93co6q".into());
        due.due_at = Some("11:36".into());
        let mut row = row();
        let work = row.work.as_mut().unwrap();
        work.blocked = true;
        work.record.as_mut().unwrap().blocked_on = Some("prod secret missing".into());
        // A host on an older build still sends the path.
        row.detail_groups.insert(1, group("path", &[]));
        // As the host orders them: due, the gate, then what waits.
        row.owes = vec![
            due,
            clears,
            check(
                VerifyStage::Staging,
                OwedState::Waiting,
                "Chat topic sweep and queue run on the Worker crons. STEPS: 1. post three 50+ char messages in one channel of a brand with topics 2. after the next :07/:22/:37/:52, entity_topics rows exist for the brand and the queue table is empty again 3. the Worker log shows one sweep and no retries",
            ),
            check(
                VerifyStage::Production,
                OwedState::Waiting,
                "STEPS: 1. curl the endpoint 2. read the Worker log",
            ),
            merged,
            timed,
        ];
        row.watches = vec![WatchView {
            id: "tell44".into(),
            by: "human".into(),
            conditions: vec![waits("production", "in production", WatchTruth::NotYet)],
            actions: vec![WatchActionKind::Notify],
            ..Default::default()
        }];
        row
    }

    /// `left` with `right` ending at the pane's edge.
    fn edge(left: &str, right: &str, width: usize) -> String {
        format!(
            "{left}{}{right}",
            " ".repeat(width - left.width() - right.width())
        )
    }

    #[test]
    fn what_a_row_owes_is_listed_in_full_in_one_column_after_its_mark() {
        let row = owing_row();
        let bell = glyphs::WATCH;
        let styled = owes_block(&row, &Times::new(), 86);
        let lines = plain(&styled);
        assert_eq!(
            lines,
            [
                // Due first, then staging, production, a known time, other
                // work and the gate; what only tells the person comes last.
                edge("● since 11:36", "wt done 93co6q", 86),
                "  confirm prod logs, then open the PR retiring the old path".to_owned(),
                edge("○ staging", "3 steps", 86),
                "  Chat topic sweep and queue run on the Worker crons.".to_owned(),
                "  1. post three 50+ char messages in one channel of a brand with topics".to_owned(),
                "  2. after the next :07/:22/:37/:52, entity_topics rows exist for the brand and the"
                    .to_owned(),
                "     queue table is empty again".to_owned(),
                "  3. the Worker log shows one sweep and no retries".to_owned(),
                edge("○ production", "2 steps", 86),
                "  1. curl the endpoint".to_owned(),
                "  2. read the Worker log".to_owned(),
                edge("○ 17:00 · by manager", "in 2h 10m", 86),
                "  Run the smoke test against staging".to_owned(),
                edge("○ merged", "before release", 86),
                "  Before the release carrying #3879: confirm api-prod's add-on route answers 200 for a"
                    .to_owned(),
                "  brand with the add-on, and 404 without it.".to_owned(),
                "⊘ before merge".to_owned(),
                "  prod secret missing from the vault the worker reads".to_owned(),
                "  clears when ○ #3806 in production".to_owned(),
                format!("{bell} tells you when ○ in production"),
            ]
        );
        // Due is the mark and the color of the "when", not a word.
        assert_eq!(color_of(&styled[..1], "●"), Some(theme::WARN));
        assert_eq!(color_of(&styled[..1], "since 11:36"), Some(theme::WARN));
        assert_eq!(color_of(&styled[..1], "wt done"), Some(theme::FG_DIM));
        assert_eq!(color_of(&styled[1..2], "confirm"), Some(theme::FG));
        assert_eq!(color_of(&styled[2..3], "○"), Some(theme::FG_DIM));
        assert_eq!(color_of(&styled[2..3], "staging"), Some(theme::FG_MID));
        assert_eq!(color_of(&styled[3..4], "Chat"), Some(theme::FG_MID));
        assert_eq!(color_of(&styled[16..17], "⊘"), Some(theme::WARN));
        assert_eq!(
            color_of(&styled[18..19], "clears when"),
            Some(theme::FG_DIM)
        );
        assert_eq!(color_of(&styled[19..], "tells you"), Some(theme::FG_DIM));
        // Every width keeps the one column and the right edge.
        for width in [196, 116, 86, 56, 30, 20] {
            let lines = plain(&owes_block(&row, &Times::new(), width));
            assert!(lines.iter().all(|line| line.width() <= width), "{lines:#?}");
            for line in &lines {
                let mut chars = line.chars();
                let (mark, gap) = (chars.next(), chars.next());
                assert!(
                    gap == Some(' ') && (mark == Some(' ') || chars.next() != Some(' ')),
                    "{line:?}"
                );
            }
            if width >= 56 {
                assert_eq!(lines[0], edge("● since 11:36", "wt done 93co6q", width));
            }
        }
        // Too narrow to set a detail apart, it follows the "when".
        let narrow = plain(&owes_block(&row, &Times::new(), 20));
        assert_eq!(narrow[..2], ["● since 11:36 · wt", "  done 93co6q"]);
        // Nothing folds and nothing is left for wt owed to list.
        let mut many = owing_row();
        many.owes = (0..12)
            .map(|index| step(OwedState::Waiting, &format!("step {index}")))
            .collect();
        let lines = block(&many, 86);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("  step "))
                .count(),
            12
        );
        let said = lines.join("\n");
        assert!(!said.contains("more (wt owed)") && !said.contains("steps\n  V "));
    }

    #[test]
    fn owed_items_say_who_set_them_and_how_delivery_stands() {
        let labelled = |key: &str, text: &str, label: &str, state| WatchConditionView {
            label: label.into(),
            ..waits(key, text, state)
        };
        let mut sealed = labelled(
            "check:vault=X",
            "check vault=X",
            "check vault",
            WatchTruth::Unknown,
        );
        sealed.reason = Some("exited 2: vault sealed".into());
        let mut unknown = step(OwedState::Waiting, "rotate the key");
        unknown.conditions = vec![sealed];
        let mut both = step(OwedState::Waiting, "retire the flag\nthen report");
        both.before_release = true;
        both.due_in = Some("6d".into());
        both.conditions = vec![
            labelled(
                "#38:staging",
                "#38 on staging",
                "#38 staging",
                WatchTruth::Holds,
            ),
            labelled("merged", "merged", "merged", WatchTruth::NotYet),
        ];
        let mut unsure = step(OwedState::Due, "confirm the dashboard");
        unsure.id = Some("abc234".into());
        unsure.unconfirmed = true;
        let mut awaiting = check(VerifyStage::Staging, OwedState::Waiting, "open /health");
        awaiting.awaiting_deploy = true;
        let mut overdue = check(VerifyStage::Production, OwedState::Due, "open /health");
        overdue.overdue = true;
        let mut clearing = gate("QA sign-off");
        clearing.conditions = vec![waits("merged", "merged", WatchTruth::Holds)];
        clearing.met = true;
        clearing.wakes = true;
        let mut row = row();
        // The pane orders them, whatever order the host sent.
        row.owes = vec![
            gate("QA sign-off"),
            clearing,
            unknown,
            both,
            awaiting,
            unsure,
            overdue,
        ];
        let styled = owes_block(&row, &Times::new(), 100);
        assert_eq!(
            plain(&styled),
            [
                edge("● due · delivery not confirmed", "wt done abc234", 100),
                "  confirm the dashboard".to_owned(),
                edge("● production", "overdue", 100),
                "  open /health".to_owned(),
                edge("○ staging", "awaiting deploy", 100),
                "  open /health".to_owned(),
                edge("? check vault · exited 2: vault sealed", "", 100)
                    .trim_end()
                    .to_owned(),
                "  rotate the key".to_owned(),
                // A detail of its own leaves "before release" beside the
                // "when"; the author's lines are kept.
                edge("○ #38 staging & merged · before release", "in 6d", 100),
                "  retire the flag".to_owned(),
                "  then report".to_owned(),
                "⊘ before merge".to_owned(),
                "  QA sign-off".to_owned(),
                "  a person clears it".to_owned(),
                "⊘ before merge".to_owned(),
                "  QA sign-off".to_owned(),
                "  clears when ✓ merged, then wakes the session · delivering".to_owned(),
            ]
        );
        assert_eq!(color_of(&styled[2..3], "overdue"), Some(theme::ERR));
        assert_eq!(color_of(&styled[2..3], "production"), Some(theme::ERR));
        // A known time orders what is scheduled, soonest first, ahead of
        // other work; the host's own filing wins over the view's.
        let timed = |what: &str, at_ms: u64| {
            let mut item = step(OwedState::Waiting, what);
            item.next_at_ms = Some(at_ms);
            item
        };
        let mut deploy = timed("after the deploy", 5);
        deploy.block = Some(crate::OwedBlock::WaitingProduction);
        row.owes = vec![
            step(OwedState::Waiting, "other"),
            timed("later", 9_000),
            timed("sooner", 2_000),
            deploy,
        ];
        let order: Vec<String> = plain(&owes_block(&row, &Times::new(), 100))
            .into_iter()
            .filter_map(|line| line.strip_prefix("  ").map(str::to_owned))
            .collect();
        assert_eq!(order, ["after the deploy", "sooner", "later", "other"]);
    }

    #[test]
    fn the_header_counts_what_is_owed_and_a_landed_row_reads_from_its_phase() {
        // Unlanded: the claim, then a pointer to the list below the facts.
        let lines = block(&owing_row(), 120);
        assert_eq!(
            lines[..2],
            [
                "⊘ blocked · ready · risk low · 2h ago · owes 6",
                "│ Lead sentence."
            ]
        );
        assert_eq!(block(&row(), 200)[0], "● ready · risk low · 2h ago");
        let mut live = row();
        live.work.as_mut().unwrap().derived = true;
        assert!(block(&live, 200)[0].starts_with("● ready · live"));
        // A row with no status at all still lists what it owes.
        let mut bare = row();
        bare.work = None;
        assert!(block(&bare, 200).is_empty());
        bare.owes = vec![step(OwedState::Waiting, "rebase onto main")];
        bare.owes[0].conditions = vec![waits("#12", "#12 merged", WatchTruth::NotYet)];
        assert_eq!(block(&bare, 200), ["○ #12 merged", "  rebase onto main"]);
        // Landed: the phase and the counts. The claim's risk and age were
        // about a merge that has happened.
        let mut due = step(OwedState::Due, "confirm prod logs");
        due.id = Some("93co6q".into());
        let waiting = check(VerifyStage::Production, OwedState::Waiting, "a");
        let mut row = landed(OwedPhase::Due, vec![due, waiting]);
        let styled = banner(&row, &Times::new(), 120);
        assert_eq!(
            plain(&styled)[..2],
            ["● due · 1 waiting", "│ Lead sentence."]
        );
        assert_eq!(styled[0].spans[1].style.fg, Some(theme::WARN));
        // An overdue check turns the header red.
        row.owes.insert(
            0,
            check(VerifyStage::Staging, OwedState::Due, "open /health"),
        );
        row.owes[0].overdue = true;
        let styled = banner(&row, &Times::new(), 120);
        assert_eq!(plain(&styled)[0], "● 2 due · 1 waiting");
        assert_eq!(styled[0].spans[1].style.fg, Some(theme::ERR));
        let done = landed(OwedPhase::Done, Vec::new());
        let styled = banner(&done, &Times::new(), 200);
        assert_eq!(
            plain(&styled),
            [
                "✓ done",
                "│ Lead sentence.",
                "│ OPS     none",
                "│ REVERT  safe"
            ]
        );
        assert_eq!(styled[0].spans[1].style.fg, Some(theme::OK));
        // Waiting: what it waits on, as its row in the Landed block says.
        let timed = step(OwedState::Waiting, "rerun the renewal check");
        for (block, expected) in [
            (
                crate::OwedBlock::WaitingStaging,
                "◷ waiting for staging · 1 owed",
            ),
            (
                crate::OwedBlock::WaitingProduction,
                "◷ waiting for production · 1 owed",
            ),
            (crate::OwedBlock::Scheduled, "◷ scheduled · 1 owed"),
            (
                crate::OwedBlock::WaitingOther,
                "◷ waiting on other work · 1 owed",
            ),
        ] {
            let mut row = landed(OwedPhase::Waiting, vec![timed.clone()]);
            row.owed_block = Some(block);
            let styled = banner(&row, &Times::new(), 200);
            assert_eq!(plain(&styled)[0], expected);
            // Nothing is asked of anyone yet.
            assert_eq!(styled[0].spans[1].style.fg, Some(theme::FG_MID));
        }
        // Someone still at work on a landed row is not hidden by its phase,
        // and a host that does not file its rows still says it waits.
        let mut busy = landed(OwedPhase::Waiting, vec![timed]);
        busy.work.as_mut().unwrap().effective_state = Some(WorkState::Working);
        assert_eq!(block(&busy, 200)[0], "◷ waiting · 1 owed · working");
    }

    #[test]
    fn a_step_due_since_a_time_reads_the_controllers_clock() {
        let at_ms = 1_800_000_000_000;
        let mut due = step(OwedState::Due, "confirm prod logs");
        due.due_at_ms = Some(at_ms);
        due.due_at = Some("11:36".into());
        let row = landed(OwedPhase::Due, vec![due]);
        assert!(plain(&owes_block(&row, &Times::new(), 80))[0].starts_with("● since 11:36"));
        let times: Times = [(
            at_ms,
            LocalStamp {
                day: "today".into(),
                clock: "12:36".into(),
                terse: "12:36".into(),
            },
        )]
        .into();
        assert!(plain(&owes_block(&row, &times, 80))[0].starts_with("● since 12:36"));
    }

    #[test]
    fn the_pane_reads_the_same_at_every_width_and_scrolls_to_its_end() {
        for width in [200u16, 120, 90] {
            let lines = screen(owing_row(), width, 60);
            let at = |needle: &str| {
                lines
                    .iter()
                    .position(|line| line.contains(needle))
                    .unwrap_or_else(|| panic!("{needle:?} missing from:\n{}", lines.join("\n")))
            };
            // The banner and its note, the facts, one blank line, then all
            // of what is owed and what the person is told.
            let order = [
                at("Fix the flaky login test"),
                at("⊘ blocked · ready · risk low · 2h ago · owes 6"),
                at("REVERT"),
                at("branch feature/eng-12"),
                at("   git "),
                at("● since 11:36"),
                at("○ staging"),
                at("3. the Worker log shows one sweep"),
                at("○ production"),
                at("○ 17:00"),
                at("○ merged"),
                at("⊘ before merge"),
                at("clears when"),
                at("tells you when"),
                at("Won't rebase cleanly"),
            ];
            assert!(order.is_sorted(), "{order:?}\n{}", lines.join("\n"));
            assert_eq!(at("   git ") + 2, at("● since 11:36"));
            let blank = |line: &str| line.trim_matches(|c| c == '│' || c == ' ').is_empty();
            assert!(blank(&lines[at("   git ") + 1]));
            // Right-hand details end one cell inside the border.
            for detail in ["wt done 93co6q", "3 steps", "in 2h 10m", "before release"] {
                let line = find(&lines, detail);
                assert!(
                    line.trim_end_matches('│').ends_with(&format!("{detail} ")),
                    "{line}"
                );
            }
            let said = lines.join("\n");
            assert!(!said.contains("/very/long/path"), "{said}");
            assert!(!said.contains("(agent)") && !said.contains("V shows"));
        }
        // Scrolled as far as it goes, a short pane ends on the last line.
        let mut model = Model {
            board: Arc::new(Board {
                rows: vec![owing_row()],
                ..Default::default()
            }),
            selected: Some(0),
            details_scroll: u16::MAX,
            ..Model::default()
        };
        model.rebuild_items();
        model.selected = Some(0);
        let mut terminal = Terminal::new(TestBackend::new(90, 14)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut model, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let last: String = (0..90).map(|x| buffer[(x, 12)].symbol()).collect();
        assert!(last.contains("+2 unresolved threads"), "{last}");
    }

    #[test]
    fn the_title_sits_one_row_below_the_border_however_it_wraps() {
        let long = "Sweep chat topics on the Worker crons and drain the queue after every run so nothing is left behind";
        let blank = |line: &str| {
            line.trim_matches(|c| c == '│' || c == '┃' || c == ' ')
                .is_empty()
        };
        for (title, source) in [
            ("Fix the flaky login test", TitleSource::Agent),
            (long, TitleSource::Agent),
            (long, TitleSource::User),
        ] {
            // Content that fits, and content that overflows the pane.
            for height in [60, 12] {
                let mut row = owing_row();
                row.title = title.into();
                row.title_source = source;
                let lines = screen(row, 60, height);
                assert!(blank(&lines[1]), "{}", lines.join("\n"));
                assert!(lines[2].contains(&title[..20]), "{}", lines.join("\n"));
                assert!(!lines.join("\n").contains("(agent)"));
            }
        }
        // Only a person's title says whose it is.
        let mut named = row();
        named.title_source = TitleSource::User;
        let lines = screen(named, 70, 40);
        assert!(find(&lines, "Fix the flaky login test").contains("(user)"));
        // A pane too short to spare a row spends them all on content.
        let lines = screen(row(), 70, 5);
        assert!(
            lines[1].contains("Fix the flaky login test"),
            "{}",
            lines[1]
        );
    }

    #[test]
    fn notifications_are_dim_lines_after_the_block_and_six_are_listed() {
        let at_ms = 1_800_000_000_000;
        let mut due = waits("at:1800000000000", "17:00", WatchTruth::NotYet);
        due.at_ms = Some(at_ms);
        due.due_in = Some("2h10m".into());
        let tell = |id: &str, conditions| WatchView {
            id: id.into(),
            by: "human".into(),
            conditions,
            actions: vec![WatchActionKind::Notify],
            ..Default::default()
        };
        let mut row = row();
        row.work = None;
        row.watches = vec![
            tell(
                "tell44",
                vec![waits("merged", "merged", WatchTruth::NotYet)],
            ),
            WatchView {
                by: "manager".into(),
                headline: Some("time to cut the release".into()),
                ..tell("tell55", vec![due])
            },
            // Its wake is in the owed list; it is not said again here.
            WatchView {
                id: "wake33".into(),
                actions: vec![WatchActionKind::Wake],
                ..Default::default()
            },
        ];
        let bell = glyphs::WATCH;
        let styled = notify_lines(&row, &Times::new(), 200);
        assert_eq!(
            plain(&styled),
            [
                format!("{bell} tells you when ○ merged"),
                format!(
                    "{bell} tells you at 17:00 (in 2h10m) · time to cut the release · by manager"
                ),
            ]
        );
        assert_eq!(styled[0].spans[0].style.fg, Some(badges::WAITING));
        // Met under paused automations: held, not delivering.
        let mut held = row.clone();
        held.watches.truncate(1);
        held.watches[0].conditions[0].state = WatchTruth::Holds;
        held.watches[0].met = true;
        held.watches[0].held = true;
        assert_eq!(
            plain(&notify_lines(&held, &Times::new(), 200)),
            [format!(
                "{bell} tells you when ✓ merged · held: automations paused"
            )]
        );
        held.watches[0].held = false;
        let styled = notify_lines(&held, &Times::new(), 200);
        assert!(plain(&styled)[0].ends_with("✓ merged · delivering"));
        assert_eq!(styled[0].spans[0].style.fg, Some(theme::OK));
        // Unreadable: the bell warns and the reason follows dimly.
        held.watches[0].met = false;
        held.watches[0].conditions[0].state = WatchTruth::Unknown;
        held.watches[0].conditions[0].reason = Some("no current data".into());
        let styled = notify_lines(&held, &Times::new(), 200);
        assert_eq!(
            plain(&styled),
            [format!("{bell} tells you when ? merged (no current data)")]
        );
        assert_eq!(styled[0].spans[0].style.fg, Some(theme::WARN));
        let mut busy = row.clone();
        busy.watches = (0..8)
            .map(|index| {
                tell(
                    &format!("tell{index}"),
                    vec![waits("merged", "merged", WatchTruth::NotYet)],
                )
            })
            .collect();
        let lines = plain(&notify_lines(&busy, &Times::new(), 200));
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[6], "  +2 more (wt watch)");
        // Narrow panes wrap under the words, not under the bell.
        let narrow = notify_lines(&row, &Times::new(), 30);
        assert!(narrow.iter().all(|line| line.width() <= 30), "{narrow:?}");
        let narrow = plain(&narrow);
        let hang = " ".repeat(bell.width() + 1);
        assert!(narrow.len() > 2);
        assert!(
            narrow[2].starts_with(&hang) && !narrow[2][hang.len()..].starts_with(' '),
            "{narrow:?}"
        );
    }

    #[test]
    fn the_pane_orders_header_note_facts_then_what_is_owed() {
        let mut waiting = row();
        waiting.owes = vec![check(
            VerifyStage::Staging,
            OwedState::Waiting,
            "Look. STEPS: 1. open /health",
        )];
        waiting.watches = vec![WatchView {
            id: "tell44".into(),
            by: "human".into(),
            conditions: vec![waits("merged", "merged", WatchTruth::NotYet)],
            actions: vec![WatchActionKind::Notify],
            ..Default::default()
        }];
        let lines = screen(waiting, 70, 40);
        let at = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} missing from:\n{}", lines.join("\n")))
        };
        let order = [
            at("● ready · risk low · 2h ago · owes 1"),
            at("REVERT"),
            at("feature/eng-12"),
            at("○ staging"),
            at("1. open /health"),
            at("tells you when"),
        ];
        assert!(order.is_sorted(), "{order:?}\n{}", lines.join("\n"));
        // A row with no saved status still shows what it waits for.
        let mut bare = row();
        bare.work = None;
        bare.watches = vec![WatchView {
            actions: vec![WatchActionKind::Notify],
            conditions: vec![waits("ready", "ready", WatchTruth::NotYet)],
            ..Default::default()
        }];
        let lines = screen(bare, 70, 40);
        assert!(find(&lines, "tells you when").contains("ready"));
    }

    fn section_screen(rows: Vec<BoardRow>, key: &str) -> Vec<String> {
        let count = rows.len();
        let board = Board {
            rows,
            sections: vec![BoardSection {
                key: key.into(),
                title: "To Merge".into(),
                folded: true,
                rows: (0..count).collect(),
            }],
            ..Default::default()
        };
        let mut model = Model {
            board: Arc::new(board),
            ..Model::default()
        };
        model.rebuild_items();
        model.selected = Some(0);
        assert!(
            model.selected_row().is_none(),
            "the folded header is selected"
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut model, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..40)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    /// The status line breaks only between its items, so no line starts or
    /// ends with a separator, and later lines start under the first one's
    /// text.
    #[test]
    fn the_status_line_wraps_between_items_and_never_starts_with_a_separator() {
        let mut row = owing_row();
        row.work.as_mut().unwrap().stale = Some(true);
        row.work.as_mut().unwrap().record.as_mut().unwrap().extra.insert(
            "stamp".into(),
            serde_json::json!({"skill": "fable-review", "sha": "abc", "at": "2026-10-09T00:00:00Z"}),
        );
        let header = |width| {
            plain(&work_status_block(&row, width))
                .into_iter()
                .take_while(|line| !line.starts_with('│'))
                .collect::<Vec<_>>()
        };
        let whole = header(200);
        assert_eq!(whole.len(), 1);
        assert!(whole[0].contains(" · commits since · owes 6"), "{whole:?}");
        for width in 20..whole[0].width() {
            let lines = header(width);
            assert!(lines.len() > 1, "{width}: {lines:?}");
            for line in &lines {
                assert!(!line.trim_start().starts_with('·'), "{width}: {lines:?}");
                assert!(!line.trim_end().ends_with('·'), "{width}: {lines:?}");
                assert!(line.width() <= width, "{width}: {lines:?}");
            }
            assert!(
                lines[1..].iter().all(|line| line.starts_with("  ")),
                "{width}: {lines:?}"
            );
            let joined = lines
                .iter()
                .map(|line| line.trim())
                .collect::<Vec<_>>()
                .join(" · ");
            assert_eq!(joined, whole[0], "{width}");
        }
    }

    /// The Landed block's pane says what its header counts: what is due
    /// and on which row, who waits for what, and who is done. No asserted
    /// states, risks, attention count or merge gates.
    #[test]
    fn the_landed_blocks_pane_lists_what_its_header_counts() {
        use crate::OwedBlock;
        let filed = |key: &str, block: OwedBlock, owes: Vec<OwedView>| {
            let phase = match block {
                OwedBlock::Due => OwedPhase::Due,
                OwedBlock::Done => OwedPhase::Done,
                _ => OwedPhase::Waiting,
            };
            let mut row = landed(phase, owes);
            row.key = key.into();
            row.slug = key.into();
            row.title = format!("Title of {key}");
            row.owed_block = Some(block);
            // A gate asserted before the merge is not what the block waits on.
            row.work
                .as_mut()
                .unwrap()
                .record
                .as_mut()
                .unwrap()
                .blocked_on = Some("needs the importer".into());
            row
        };
        let mut headed = check(VerifyStage::Staging, OwedState::Due, "x");
        headed.headline = "Import preview renders".into();
        let mut step_due = step(OwedState::Due, "x");
        step_due.headline = "delete the flag".into();
        step_due.due_at_ms = Some(1_000);
        let mut timed = step(OwedState::Waiting, "x");
        timed.next_at_ms = Some(2_000);
        timed.conditions = vec![WatchConditionView {
            at_ms: Some(2_000),
            ..waits("at", "at 09:00 tomorrow", WatchTruth::NotYet)
        }];
        let mut other = step(OwedState::Waiting, "x");
        other.conditions = vec![WatchConditionView {
            label: "u-work merged".into(),
            ..waits("u-work", "u-work merged", WatchTruth::NotYet)
        }];
        let rows = vec![
            filed("done", OwedBlock::Done, Vec::new()),
            filed(
                "due",
                OwedBlock::Due,
                vec![
                    headed,
                    step_due,
                    check(VerifyStage::Production, OwedState::Waiting, "x"),
                ],
            ),
            filed(
                "prod",
                OwedBlock::WaitingProduction,
                vec![check(VerifyStage::Production, OwedState::Waiting, "x")],
            ),
            filed("sched", OwedBlock::Scheduled, vec![timed]),
            filed("gate", OwedBlock::WaitingOther, vec![other]),
            filed(
                "stg",
                OwedBlock::WaitingStaging,
                vec![check(VerifyStage::Staging, OwedState::Waiting, "x")],
            ),
        ];
        let mut board = Board {
            sections: vec![BoardSection {
                key: crate::LANDED_SECTION.into(),
                title: crate::LANDED_TITLE.into(),
                folded: true,
                rows: (0..rows.len()).collect(),
            }],
            rows,
            ..Default::default()
        };
        for (at_ms, terse) in [(1_000, "19:34"), (2_000, "Fri 09:00")] {
            board.history_times.insert(
                at_ms,
                LocalStamp {
                    terse: terse.into(),
                    ..Default::default()
                },
            );
        }
        let model = Model {
            board: Arc::new(board),
            ..Model::default()
        };
        let lines = plain(&section_lines(&model.board.sections[0], &model, 40));
        let lines: Vec<&str> = lines.iter().map(|line| line.trim_end()).collect();
        assert_eq!(
            lines,
            [
                "Landed",
                "1 due · 4 waiting · 1 done",
                "",
                "● due",
                "  Title of due",
                "    staging  Import preview renders",
                "    since 19:34  delete the flag",
                "",
                "◷ waiting for staging",
                "  Title of stg",
                "",
                "◷ waiting for production",
                "  Title of prod",
                "",
                "◷ scheduled",
                "  Title of sched               Fri 09:00",
                "",
                "◷ waiting on other work",
                "  Title of gate            u-work merged",
                "",
                "✓ done · c cleans up",
                "  Title of done",
                "",
                "TAB expand · y yank",
            ]
        );
        for width in [12, 20, 30] {
            let narrow = section_lines(&model.board.sections[0], &model, width);
            assert!(narrow.iter().all(|line| line.width() <= width), "{width}");
        }
    }

    /// A side of the git line with no counts is left out, not drawn empty.
    #[test]
    fn the_git_line_omits_a_sync_group_it_has_no_counts_for() {
        let text = |row: &BoardRow| Line::from(git_value(row, 200, 0)).to_string();
        assert!(text(&row()).ends_with("(↑2) [↑3 ↓1]"), "{}", text(&row()));
        let mut local = row();
        local.git.upstream = None;
        assert!(text(&local).ends_with("· [↑3 ↓1]"), "{}", text(&local));
        local.git.base_ahead = None;
        assert!(!text(&local).contains('—'), "{}", text(&local));
        assert!(!text(&local).ends_with("· "), "{}", text(&local));
    }

    #[test]
    fn wrapping_never_splits_adjacent_spans_or_no_break_spaces() {
        let spans = vec![
            Span::raw("3 worktrees"),
            Span::raw(" · "),
            Span::raw("●"),
            Span::raw("\u{a0}\u{a0}12\u{a0}needs-human"),
        ];
        let lines = wrap_spans(spans, 20)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            lines.last().unwrap().trim(),
            "●\u{a0}\u{a0}12\u{a0}needs-human"
        );
    }

    #[test]
    fn folded_section_lists_needs_human_notes_and_ranks_states() {
        let mut asking = row();
        asking.slug = "asking".into();
        asking.title = "Needs a credential".into();
        asking.work = Some(WorkPresentation {
            record: Some(WorkStatusRecord {
                note: Some("Need the staging API key from ops".into()),
                ..WorkStatusRecord::new(WorkState::NeedsHuman, "2026-10-09T00:00:00Z")
            }),
            effective_state: Some(WorkState::NeedsHuman),
            ..Default::default()
        });
        let mut gated = row();
        gated.slug = "gated".into();
        gated
            .work
            .as_mut()
            .unwrap()
            .record
            .as_mut()
            .unwrap()
            .blocked_on = Some("release approval".into());
        let mut working = row();
        working.work.as_mut().unwrap().effective_state = Some(WorkState::Working);
        let lines = section_screen(vec![gated, asking, working], "manual");
        let you = lines
            .iter()
            .position(|line| line.contains("blocked on you"))
            .unwrap_or_else(|| panic!("{}", lines.join("\n")));
        assert!(
            lines[you + 1].contains("Needs a credential"),
            "{}",
            lines[you + 1]
        );
        assert!(lines[you + 2].contains("    Need the staging API key from ops"));
        find(&lines, "blocked on");
        find(&lines, "gated: release approval");
        let rollup = find(&lines, "3 worktrees");
        let ready = rollup.find("ready").unwrap();
        let human = rollup.find("needs-human").unwrap();
        let working = rollup.find("working").unwrap();
        assert!(ready < human && human < working, "{rollup}");
        find(
            &lines,
            "TAB expand · y yank · L rename · D delete · J/K move",
        );

        let archived = section_screen(vec![row()], ARCHIVED_SECTION);
        assert!(!archived.join("\n").contains("L rename"));
        find(&archived, "TAB expand · y yank");
        // A block wt derives is named and ordered by wt too.
        let derived = section_screen(vec![row()], crate::LANDED_SECTION);
        assert!(!derived.join("\n").contains("J/K move"));
        find(&derived, "TAB expand · y yank");
    }
}
