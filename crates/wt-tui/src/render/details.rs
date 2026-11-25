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
/// Widest "when" that sets the owes block's column; a longer one runs into
/// its own text instead of pushing every other item's text right.
const OWED_WHEN_CELLS: usize = 22;
/// Cells before an owed item's "when": the indent, its mark and the gap.
const OWED_LEAD_CELLS: usize = 5;
/// Items the pane lists for one row; `wt owed` lists the rest.
const OWES_LINES_MAX: usize = 8;
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
    // scrollbar's gutter so a thumb never sits on text. A blank row above
    // the content (outside the scroll region) matches the TS pane's top
    // padding, so the title never sits against the border; a pane too short
    // for everything gives that row to content instead.
    let mut content = inner.inner(Margin {
        vertical: 0,
        horizontal: 1,
    });
    let width = usize::from(content.width).max(1);
    let mut lines = build(model, width);
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
    if content.height > 3 && lines.len() < usize::from(content.height) {
        content.y += 1;
        content.height -= 1;
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
    let mut lines = title_lines(&row.title, Some(row.title_source.as_str()), width);
    let times = &model.board.history_times;
    let work = work_status_block(row, model.show_verification, times, width);
    // The facts follow the title directly, as in TS; a status block gets a
    // blank line after it.
    if !work.is_empty() {
        lines.extend(work);
        lines.push(Line::default());
    }

    let label_width = label_column(&row.detail_groups);
    for group in &row.detail_groups {
        lines.extend(definition(row, group, policy, label_width, width, now));
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
        "branch" | "path" | "issue" | "stage" | "dev" | "pr" | "guide" | "git" => group.id.clone(),
        _ => group.label.to_lowercase(),
    }
}

/// Label column: the widest label present plus a one-cell gap.
fn label_column(groups: &[PreparedDetailGroup]) -> usize {
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
        "path" => vec![vec![Span::styled(
            truncate_start(&row.path, value_width),
            theme::fg(theme::FG),
        )]],
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
        let remote = sync_group("(", ")", upstream.and_then(|(a, b)| a.zip(b)));
        let base = sync_group("[", "]", row.git.base_ahead.zip(row.git.base_behind));
        let mut both = remote.clone();
        both.push(Span::raw(" "));
        both.extend(base);
        segments.push(Segment::new(2, vec![both, remote]));
    }
    fit_segments(segments, width)
}

/// `(↑a ↓b)`: ahead warns, behind errs, zero is dim, unknown is `(—)`.
fn sync_group(open: &str, close: &str, counts: Option<(u32, u32)>) -> Spans {
    let Some((ahead, behind)) = counts else {
        return vec![Span::styled(format!("{open}—{close}"), theme::dim())];
    };
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
                "+{} unresolved {}",
                pr.unresolved_threads,
                if pr.unresolved_threads == 1 {
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

/// Work-status block for tests that read it without a pane width.
#[cfg(test)]
pub(crate) fn work_status_lines(row: &BoardRow, show_verification: bool) -> Vec<Line<'static>> {
    work_status_block(row, show_verification, &Times::new(), 200)
}

/// Full-width status banner: the header, the one list of what the row
/// owes, the watches that only tell the person, then the note. What is
/// owed comes before the note because it is tracked and the note is prose.
fn work_status_block(
    row: &BoardRow,
    show_verification: bool,
    times: &Times,
    width: usize,
) -> Vec<Line<'static>> {
    let header = status_header(row, times);
    let mut lines = header
        .as_ref()
        .map(|(spans, _)| wrap_spans(spans.clone(), width))
        .unwrap_or_default();
    lines.extend(owes_lines(row, show_verification, times, width));
    lines.extend(notify_lines(row, times, width));
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

/// The banner's first line and its color. A landed row reads from where it
/// stands with what it owes; until then the asserted state and the gate
/// say where the row is.
fn status_header(row: &BoardRow, times: &Times) -> Option<(Spans, ratatui::style::Color)> {
    let work = row.work.as_ref();
    let record = work.and_then(|work| work.record.as_ref());
    let effective = work.and_then(|work| work.effective_state);
    let derived = work.is_some_and(|work| work.derived);
    let (glyph, text, color) = match row.owed_phase {
        Some(phase) => phase_header(phase, &row.owes, times),
        None => {
            let (work, effective) = (work?, effective?);
            if work.blocked {
                let state = record.map_or(effective, |record| record.state);
                (
                    glyphs::SLASH,
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
    // A Nerd Font glyph draws into two cells (see `glyphs`).
    let gap = if glyph == glyphs::SLASH { "  " } else { " " };
    let mut header = vec![
        Span::styled(format!("{glyph}{gap}"), theme::fg(color)),
        Span::styled(text, theme::bold(color)),
    ];
    // Someone is still at work on a landed row, or is asked something.
    if row.owed_phase.is_some()
        && let Some(state @ (WorkState::NeedsHuman | WorkState::Working | WorkState::Review)) =
            effective
    {
        header.push(Span::styled(" · ", theme::dim()));
        header.push(Span::styled(
            state.as_str(),
            theme::fg(badges::work_state_color(state)),
        ));
    }
    if let Some(risk) = record.and_then(|record| record.risk) {
        header.push(Span::styled(" · risk ", theme::dim()));
        header.push(Span::styled(
            risk.as_str(),
            theme::fg(badges::risk_color(risk)),
        ));
    }
    if let Some(age) = work
        .and_then(|work| work.age.as_ref())
        .filter(|_| !derived || record.is_some())
    {
        header.push(Span::styled(format!(" · {age} ago"), theme::dim()));
    }
    if work.is_some_and(|work| work.stale == Some(true)) && !derived {
        header.push(Span::styled(" · commits since", theme::fg(theme::WARN)));
    }
    Some((header, color))
}

/// A landed row's header: the first thing due, what it waits for and how
/// much it owes, or that it is done.
fn phase_header(
    phase: wt_core::OwedPhase,
    owes: &[OwedView],
    times: &Times,
) -> (&'static str, String, ratatui::style::Color) {
    let overdue = owes.iter().any(|item| item.overdue);
    match phase {
        wt_core::OwedPhase::Due => {
            let headline = owes
                .iter()
                .find(|item| item.due())
                .map(|item| item.headline.trim())
                .filter(|headline| !headline.is_empty());
            (
                glyphs::OWED_DUE,
                headline.map_or_else(|| "due".to_owned(), |headline| format!("due · {headline}")),
                if overdue { theme::ERR } else { theme::WARN },
            )
        }
        wt_core::OwedPhase::Waiting => {
            let first = owes
                .iter()
                .find(|item| item.holds_cleanup())
                .or(owes.first());
            let what = first
                .map(|item| waiting_for(item, times))
                .filter(|what| !what.trim().is_empty())
                .map(|what| format!(" {what}"))
                .unwrap_or_default();
            (
                glyphs::OWED_CLOCK,
                format!("waiting{what} · {} owed", owes.len()),
                if overdue { theme::ERR } else { theme::FG_MID },
            )
        }
        wt_core::OwedPhase::Done => (glyphs::OWED_DONE, "done".to_owned(), theme::OK),
    }
}

/// What a waiting item waits for, after the word `waiting`: a stage, a
/// time, or its conditions.
fn waiting_for(item: &OwedView, times: &Times) -> String {
    if let Some(stage) = item.stage {
        return format!("for {}", stage.as_str());
    }
    match item.conditions.as_slice() {
        [] => item.when.clone(),
        [only] if only.at_ms.is_some() => format!("until {}", instant_text(only, times)),
        conditions => format!(
            "for {}",
            conditions
                .iter()
                .map(|cond| instant_text(cond, times))
                .collect::<Vec<_>>()
                .join(" and ")
        ),
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
        (glyphs::SLASH, theme::WARN)
    } else {
        (glyphs::OWED_WAITING, theme::FG_DIM)
    }
}

/// The "when" column of an owed item: `due 11:36`, `before merge`, `in staging`,
/// `at 17:00 (in 2h)`, `when #3806 in production`.
fn owed_when(item: &OwedView, times: &Times) -> Spans {
    let (_, color) = owed_mark(item);
    let stage = item.stage.map_or("", wt_core::VerifyStage::as_str);
    let plain = |text: String, color| vec![Span::styled(text, theme::fg(color))];
    match item.kind {
        wt_core::OwedKind::Gate => plain("before merge".to_owned(), color),
        wt_core::OwedKind::Check if item.overdue => plain(format!("overdue in {stage}"), color),
        wt_core::OwedKind::Check if item.due() => plain(format!("due in {stage}"), color),
        wt_core::OwedKind::Check => plain(
            match &item.not_before_in {
                Some(wait) => format!("in {stage} +{wait}"),
                None => format!("in {stage}"),
            },
            theme::FG_DIM,
        ),
        wt_core::OwedKind::Step if item.due() => {
            let at = item
                .due_at_ms
                .and_then(|at_ms| stamp_text(at_ms, times))
                .or_else(|| item.due_at.clone());
            plain(
                at.map_or_else(|| "due".to_owned(), |at| format!("due {at}")),
                color,
            )
        }
        wt_core::OwedKind::Step if item.conditions.is_empty() => {
            plain(item.when.clone(), theme::FG_DIM)
        }
        wt_core::OwedKind::Step => {
            let lone = timed(&item.conditions);
            let mut spans = vec![Span::styled(
                if lone { "at " } else { "when " },
                theme::dim(),
            )];
            // One condition's truth is the item's own mark.
            let marked = item.conditions.len() > 1;
            for (index, cond) in item.conditions.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(" and ", theme::dim()));
                }
                spans.extend(condition_words(cond, times, marked));
            }
            spans
        }
    }
}

/// What follows the "when" column: the gate and what clears it, a check
/// and how many steps it has, or a step's headline, then who set it and
/// how its delivery stands. `used` is the cells the line already holds.
fn owed_text(
    item: &OwedView,
    own: &str,
    expanded: bool,
    used: usize,
    width: usize,
    times: &Times,
) -> Spans {
    let mut spans = Vec::new();
    match item.kind {
        wt_core::OwedKind::Gate => {
            spans.push(Span::styled(
                item.what.split_whitespace().collect::<Vec<_>>().join(" "),
                theme::fg(theme::WARN),
            ));
            if !item.conditions.is_empty() {
                let lone = timed(&item.conditions);
                spans.push(Span::styled(
                    if lone {
                        " · clears at "
                    } else {
                        " · clears when "
                    },
                    theme::dim(),
                ));
                spans.extend(conditions_spans(&item.conditions, times, !lone));
            }
            if item.wakes {
                spans.push(Span::styled(", then wakes the session", theme::dim()));
            }
            spans.extend(delivery_tail(item.met, item.held));
        }
        wt_core::OwedKind::Check => {
            let (preamble, steps) = parse_verify_steps(&item.what);
            let color = if item.due() {
                theme::FG_MID
            } else {
                theme::FG_DIM
            };
            spans.push(Span::styled("check", theme::fg(color)));
            let mut suffix = vec![Span::styled(
                if expanded {
                    " · V collapses"
                } else {
                    " · V expands"
                },
                theme::dim(),
            )];
            // Overdue is the one state such a check shows: the deploy it
            // waits for has been owed for days.
            if item.awaiting_deploy && !item.overdue && !item.due() {
                suffix.push(Span::styled(" · awaiting deploy", theme::dim()));
            }
            if !steps.is_empty() {
                spans.push(Span::styled(
                    format!(
                        " · {} step{}",
                        steps.len(),
                        if steps.len() == 1 { "" } else { "s" }
                    ),
                    theme::dim(),
                ));
            } else if let Some(first) = preamble.lines().map(str::trim).find(|l| !l.is_empty()) {
                let separator = " · ";
                spans.push(Span::styled(separator, theme::dim()));
                let used = used + "check".width() + separator.width();
                spans.push(headline_span(
                    first,
                    used,
                    spans_width(&suffix),
                    width,
                    color,
                ));
            }
            spans.extend(suffix);
        }
        wt_core::OwedKind::Step => {
            let mut suffix = Vec::new();
            if item.before_release {
                suffix.push(Span::styled(" ·\u{a0}before\u{a0}release", theme::dim()));
            }
            if let Some(by) = item
                .by
                .as_deref()
                .filter(|by| !by.is_empty() && *by != own && *by != "human")
            {
                suffix.push(Span::styled(format!(" ·\u{a0}by\u{a0}{by}"), theme::dim()));
            }
            if item.unconfirmed {
                suffix.push(Span::styled(" ·\u{a0}unconfirmed", theme::fg(theme::WARN)));
            }
            for reason in item
                .conditions
                .iter()
                .filter_map(|cond| cond.reason.as_deref())
                .filter(|reason| !reason.is_empty())
            {
                suffix.push(Span::styled(format!(" · {reason}"), theme::dim()));
            }
            suffix.extend(delivery_tail(item.met, item.held));
            // What `wt done` takes.
            if let Some(id) = item.id.as_deref().filter(|_| item.due()) {
                suffix.push(Span::raw("\u{a0}\u{a0} "));
                suffix.push(Span::styled(id.to_owned(), theme::fg(theme::FG)));
            }
            // The first line, as its host prepared it.
            let headline = item.headline.as_str();
            if !headline.trim().is_empty() {
                let color = if item.due() { theme::FG } else { theme::FG_MID };
                spans.push(headline_span(
                    headline.trim(),
                    used,
                    spans_width(&suffix),
                    width,
                    color,
                ));
            }
            spans.extend(suffix);
        }
    }
    spans
}

/// The one list of what the row owes, most urgent first, one line an item
/// with the "when" column aligned. A landed row that owes nothing says so;
/// before landing, nothing owed is nothing to say.
fn owes_lines(row: &BoardRow, expanded: bool, times: &Times, width: usize) -> Vec<Line<'static>> {
    if row.owes.is_empty() {
        return if row.owed_phase.is_some() {
            vec![Line::styled("owes nothing", theme::dim())]
        } else {
            Vec::new()
        };
    }
    let shown = &row.owes[..row.owes.len().min(OWES_LINES_MAX)];
    let labels: Vec<Spans> = shown.iter().map(|item| owed_when(item, times)).collect();
    let column = labels
        .iter()
        .map(|label| spans_width(label))
        .filter(|cells| *cells <= OWED_WHEN_CELLS)
        .max()
        .unwrap_or(0);
    // Too narrow for two columns: each item wraps under its own mark.
    let aligned = width >= OWED_LEAD_CELLS + column + 2 + OWED_HEADLINE_MIN_CELLS;
    let mut lines = vec![Line::styled("owes", theme::dim())];
    for (item, label) in shown.iter().zip(labels) {
        let (glyph, color) = owed_mark(item);
        let mut prefix = vec![Span::styled(format!("  {glyph}  "), theme::fg(color))];
        let cells = spans_width(&label);
        let mut text = Vec::new();
        if aligned && cells <= column {
            prefix.extend(label);
            prefix.push(Span::raw(" ".repeat(column - cells + 2)));
        } else {
            text.extend(label);
            text.push(Span::raw("  "));
        }
        let used = spans_width(&prefix) + spans_width(&text);
        text.extend(owed_text(item, &row.slug, expanded, used, width, times));
        lines.extend(wrap_under(prefix, text, width));
        if expanded && item.kind == wt_core::OwedKind::Check {
            lines.extend(check_lines(&item.what, item.due(), width));
        }
    }
    if row.owes.len() > shown.len() {
        lines.push(Line::styled(
            format!("  +{} more (wt owed)", row.owes.len() - shown.len()),
            theme::dim(),
        ));
    }
    lines
}

/// A check's text under its line: the preamble, then its numbered steps.
fn check_lines(text: &str, due: bool, width: usize) -> Vec<Line<'static>> {
    let (preamble, steps) = parse_verify_steps(text);
    let (head_color, body_color) = if due {
        (theme::WARN, theme::FG_MID)
    } else {
        (theme::FG_DIM, theme::FG_DIM)
    };
    let indent = OWED_LEAD_CELLS;
    let inner = width.saturating_sub(indent).max(1);
    let mut lines: Vec<Line<'static>> = wrap(&preamble, inner)
        .into_iter()
        .filter(|line| !line.is_empty())
        .map(|line| {
            Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(line, theme::fg(body_color)),
            ])
        })
        .collect();
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
        let bell = Span::styled(format!("{}  ", badge.glyph), theme::fg(badge.color));
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
            format!("+{} more (wt watch)", told.len() - WATCH_LINES_MAX),
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

fn section_lines(section: &BoardSection, model: &Model, width: usize) -> Vec<Line<'static>> {
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
        assert!(find(&lines, "Fix the flaky login test").contains("(agent)"));
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

    fn block(row: &BoardRow, width: usize) -> Vec<String> {
        plain(&work_status_block(row, false, &Times::new(), width))
    }

    /// The color of the first span holding `needle`.
    fn color_of(lines: &[Line<'_>], needle: &str) -> Option<ratatui::style::Color> {
        lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.contains(needle))
            .and_then(|span| span.style.fg)
    }

    #[test]
    fn a_ready_row_lists_its_gate_and_both_checks_in_one_owes_block() {
        let mut ready = row();
        let work = ready.work.as_mut().unwrap();
        work.blocked = true;
        work.record.as_mut().unwrap().blocked_on = Some("prod secret missing".into());
        let mut clears = gate("prod secret missing");
        clears.id = Some("gate22".into());
        clears.conditions = vec![waits(
            "#3806:production",
            "#3806 in production",
            WatchTruth::NotYet,
        )];
        ready.owes = vec![
            clears,
            check(
                VerifyStage::Staging,
                OwedState::Waiting,
                "Check the worker. STEPS: 1. open /health 2. b 3. c 4. d 5. e",
            ),
            check(
                VerifyStage::Production,
                OwedState::Waiting,
                "STEPS: 1. a 2. b",
            ),
        ];
        let slash = glyphs::SLASH;
        let lines = block(&ready, 200);
        assert_eq!(
            lines[..5],
            [
                format!("{slash}  blocked · ready · risk low · 2h ago"),
                "owes".to_owned(),
                format!(
                    "  {slash}  before merge   prod secret missing · clears when ○ #3806 in production"
                ),
                "  ○  in staging     check · 5 steps · V expands".to_owned(),
                "  ○  in production  check · 2 steps · V expands".to_owned(),
            ]
        );
        // The note follows what is tracked.
        assert_eq!(lines[5], "│ Lead sentence.");
        let styled = work_status_block(&ready, false, &Times::new(), 200);
        assert_eq!(color_of(&styled[2..3], slash), Some(theme::WARN));
        assert_eq!(color_of(&styled[3..4], "○"), Some(theme::FG_DIM));
        // V opens every check in place, under its own line.
        let open = plain(&work_status_block(&ready, true, &Times::new(), 200));
        assert_eq!(
            open[3..7],
            [
                "  ○  in staging     check · 5 steps · V collapses",
                "     Check the worker.",
                "     1. open /health",
                "     2. b",
            ]
        );
        assert!(open.contains(&"     2. b".to_owned()));
        assert_eq!(open.len(), lines.len() + 8);
        // Nothing is said twice: no gate line, no verify block.
        let said = lines.join("\n");
        assert!(!said.contains("blocked on:") && !said.contains("verify in"));
    }

    #[test]
    fn a_landed_rows_header_is_what_is_due_and_its_notification_follows_the_block() {
        let at_ms = 1_800_000_000_000;
        let mut due = step(
            OwedState::Due,
            "confirm prod logs, then open the PR retiring the old path\nthen report",
        );
        due.id = Some("93co6q".into());
        due.due_at_ms = Some(at_ms);
        due.due_at = Some("11:36".into());
        let mut row = landed(
            OwedPhase::Due,
            vec![
                due,
                check(
                    VerifyStage::Production,
                    OwedState::Waiting,
                    "STEPS: 1. a 2. b",
                ),
            ],
        );
        row.watches = vec![WatchView {
            id: "tell44".into(),
            by: "human".into(),
            conditions: vec![waits("production", "in production", WatchTruth::NotYet)],
            actions: vec![WatchActionKind::Notify],
            ..Default::default()
        }];
        let bell = glyphs::WATCH;
        let lines = block(&row, 200);
        assert_eq!(
            lines[..5],
            [
                "● due · confirm prod logs, then open the PR retiring the old path · risk low · 2h ago"
                    .to_owned(),
                "owes".to_owned(),
                "  ●  due 11:36      confirm prod logs, then open the PR retiring the old path   93co6q"
                    .to_owned(),
                "  ○  in production  check · 2 steps · V expands".to_owned(),
                format!("{bell}  tells you when ○ in production"),
            ]
        );
        assert_eq!(lines[5], "│ Lead sentence.");
        let styled = work_status_block(&row, false, &Times::new(), 200);
        assert_eq!(styled[0].spans[1].style.fg, Some(theme::WARN));
        assert_eq!(color_of(&styled[2..3], "●"), Some(theme::WARN));
        // What the person is told is not owed: it reads dim.
        assert_eq!(color_of(&styled[4..5], "tells you"), Some(theme::FG_DIM));
        // The controller's clock for the instant; without one, the host's.
        let times: Times = [(
            at_ms,
            LocalStamp {
                day: "today".into(),
                clock: "12:36".into(),
            },
        )]
        .into();
        let local = plain(&work_status_block(&row, false, &times, 200));
        assert!(local[2].starts_with("  ●  due 12:36  "), "{}", local[2]);
        // An overdue check turns the header and its own line red.
        row.owes.insert(
            0,
            check(VerifyStage::Staging, OwedState::Due, "open /health"),
        );
        row.owes[0].overdue = true;
        let styled = work_status_block(&row, false, &Times::new(), 200);
        let lines = plain(&styled);
        assert!(
            lines[0].starts_with("● due · check: open /health"),
            "{}",
            lines[0]
        );
        assert_eq!(
            lines[2],
            "  ●  overdue in staging  check · open /health · V expands"
        );
        assert_eq!(styled[0].spans[1].style.fg, Some(theme::ERR));
        assert_eq!(color_of(&styled[2..3], "overdue"), Some(theme::ERR));
    }

    #[test]
    fn a_landed_row_that_owes_nothing_says_so_and_an_unlanded_one_says_nothing() {
        let done = landed(OwedPhase::Done, Vec::new());
        let styled = work_status_block(&done, false, &Times::new(), 200);
        let lines = plain(&styled);
        assert_eq!(
            lines[..3],
            [
                "✓ done · risk low · 2h ago",
                "owes nothing",
                "│ Lead sentence."
            ]
        );
        assert_eq!(styled[0].spans[1].style.fg, Some(theme::OK));
        assert_eq!(styled[1].style.fg, Some(theme::FG_DIM));
        // Before landing the asserted state heads the row, and a row that
        // owes nothing has no block at all.
        let lines = block(&row(), 200);
        assert_eq!(
            lines[..2],
            ["● ready · risk low · 2h ago", "│ Lead sentence."]
        );
        let mut live = row();
        live.work.as_mut().unwrap().derived = true;
        assert!(block(&live, 200)[0].starts_with("● ready · live"));
        // A row with no status at all still shows what it owes.
        let mut bare = row();
        bare.work = None;
        assert!(block(&bare, 200).is_empty());
        bare.owes = vec![step(OwedState::Waiting, "rebase onto main")];
        bare.owes[0].conditions = vec![waits("#12", "#12 merged", WatchTruth::NotYet)];
        assert_eq!(
            block(&bare, 200),
            ["owes", "  ○  when #12 merged  rebase onto main"]
        );
    }

    #[test]
    fn a_waiting_landed_row_says_what_it_waits_for_and_how_much_it_owes() {
        let at_ms = 1_800_000_000_000;
        let mut timed = step(OwedState::Waiting, "rerun the renewal check");
        let mut at = waits("at:1800000000000", "17:00", WatchTruth::NotYet);
        at.at_ms = Some(at_ms);
        at.due_in = Some("2h10m".into());
        timed.conditions = vec![at];
        let mut conditional = step(OwedState::Waiting, "retire the flag");
        conditional.conditions = vec![
            waits("#3806:production", "#3806 in production", WatchTruth::Holds),
            waits("merged", "merged", WatchTruth::NotYet),
        ];
        for (owes, expected) in [
            (
                vec![check(VerifyStage::Staging, OwedState::Waiting, "a")],
                "◷ waiting for staging · 1 owed · risk low · 2h ago",
            ),
            (
                vec![
                    check(VerifyStage::Production, OwedState::Waiting, "a"),
                    timed.clone(),
                ],
                "◷ waiting for production · 2 owed · risk low · 2h ago",
            ),
            (
                vec![timed.clone()],
                "◷ waiting until 17:00 · 1 owed · risk low · 2h ago",
            ),
            (
                vec![conditional.clone()],
                "◷ waiting for #3806 in production and merged · 1 owed · risk low · 2h ago",
            ),
        ] {
            let row = landed(OwedPhase::Waiting, owes);
            let styled = work_status_block(&row, false, &Times::new(), 200);
            assert_eq!(plain(&styled)[0], expected);
            // Nothing is asked of anyone yet.
            assert_eq!(styled[0].spans[1].style.fg, Some(theme::FG_MID));
        }
        // Someone still at work on a landed row is not hidden by its phase.
        let mut busy = landed(OwedPhase::Waiting, vec![timed]);
        busy.work.as_mut().unwrap().effective_state = Some(WorkState::Working);
        assert!(
            block(&busy, 200)[0].starts_with("◷ waiting until 17:00 · 1 owed · working · risk"),
            "{:?}",
            block(&busy, 200)
        );
    }

    #[test]
    fn every_kind_of_owed_line_reads_as_one_aligned_line() {
        let at_ms = 1_800_000_000_000;
        let mut soon = waits("at:1800000000000", "17:00", WatchTruth::NotYet);
        soon.at_ms = Some(at_ms);
        soon.due_in = Some("2h10m".into());
        let mut timed = step(OwedState::Waiting, "Run the smoke test against staging");
        timed.by = Some("manager".into());
        timed.conditions = vec![soon];
        let mut later = waits("at:1", "Nov 27 18:12", WatchTruth::Holds);
        later.at_ms = Some(1);
        let mut before_release = step(OwedState::Waiting, "rerun the renewal check");
        before_release.before_release = true;
        before_release.conditions = vec![later];
        let mut sealed = waits("check:vault=X", "check vault=X", WatchTruth::Unknown);
        sealed.reason = Some("exited 2: vault sealed".into());
        let mut unknown = step(OwedState::Waiting, "rotate the key");
        unknown.conditions = vec![sealed];
        let mut both = step(OwedState::Waiting, "retire the flag");
        both.conditions = vec![
            waits("#3806:production", "#3806 in production", WatchTruth::Holds),
            waits("merged", "merged", WatchTruth::NotYet),
        ];
        let mut unsure = step(OwedState::Due, "confirm the dashboard");
        unsure.id = Some("abc234".into());
        unsure.unconfirmed = true;
        let mut deferred = check(
            VerifyStage::Production,
            OwedState::Waiting,
            "STEPS: 1. a 2. b",
        );
        deferred.not_before_in = Some("1h".into());
        let mut awaiting = check(VerifyStage::Staging, OwedState::Waiting, "open /health");
        awaiting.awaiting_deploy = true;
        let mut wakes = gate("QA sign-off");
        wakes.conditions = vec![waits("merged", "merged", WatchTruth::Holds)];
        wakes.wakes = true;
        wakes.met = true;
        wakes.held = true;
        let mut row = row();
        row.owes = vec![
            unsure,
            check(VerifyStage::Staging, OwedState::Due, "open /health"),
            wakes,
            awaiting,
            deferred,
            both,
            unknown,
            timed,
        ];
        let slash = glyphs::SLASH;
        let styled = work_status_block(&row, false, &Times::new(), 200);
        let lines = plain(&styled);
        assert_eq!(
            lines[1..10],
            [
                "owes".to_owned(),
                "  ●  due                  confirm the dashboard · unconfirmed   abc234".to_owned(),
                "  ●  due in staging       check · open /health · V expands".to_owned(),
                format!(
                    "  {slash}  before merge         QA sign-off · clears when ✓ merged, then wakes the session · held: automations paused"
                ),
                "  ○  in staging           check · open /health · V expands · awaiting deploy"
                    .to_owned(),
                "  ○  in production +1h    check · 2 steps · V expands".to_owned(),
                "  ○  when ✓ #3806 in production and ○ merged  retire the flag".to_owned(),
                "  ?  when check vault=X   rotate the key · exited 2: vault sealed".to_owned(),
                "  ○  at 17:00 (in 2h10m)  Run the smoke test against staging · by manager"
                    .to_owned(),
            ]
        );
        assert_eq!(color_of(&styled[8..9], "?"), Some(theme::WARN));
        assert_eq!(color_of(&styled[2..3], "unconfirmed"), Some(theme::WARN));
        // The reason recedes: it explains, it does not ask.
        assert_eq!(color_of(&styled[8..9], "vault sealed"), Some(theme::FG_DIM));
        assert_eq!(color_of(&styled[4..5], "held"), Some(theme::WARN));
        // Eight are listed; the rest are counted.
        row.owes.push(before_release.clone());
        row.owes.push(before_release.clone());
        let lines = block(&row, 200);
        assert_eq!(lines[10], "  +2 more (wt owed)");
        // Overdue is one state: an overdue check awaiting its deploy does
        // not also say so.
        let mut overdue = check(VerifyStage::Staging, OwedState::Waiting, "open /health");
        overdue.overdue = true;
        overdue.awaiting_deploy = true;
        row.owes = vec![overdue, before_release];
        let styled = work_status_block(&row, false, &Times::new(), 200);
        assert_eq!(
            plain(&styled)[2..4],
            [
                "  ●  overdue in staging  check · open /health · V expands",
                "  ○  at Nov 27 18:12     rerun the renewal check · before release",
            ]
        );
        assert_eq!(color_of(&styled[2..3], "●"), Some(theme::ERR));
        // A gate only a person clears says nothing more; one that is being
        // cleared says it is delivering.
        let mut clearing = gate("QA sign-off");
        clearing.conditions = vec![waits("merged", "merged", WatchTruth::Holds)];
        clearing.met = true;
        row.owes = vec![gate("QA sign-off"), clearing];
        assert_eq!(
            block(&row, 200)[2..4],
            [
                format!("  {slash}  before merge  QA sign-off"),
                format!("  {slash}  before merge  QA sign-off · clears when ✓ merged · delivering"),
            ]
        );
    }

    #[test]
    fn the_owes_block_fits_narrow_and_wide_panes() {
        let mut due = step(
            OwedState::Due,
            "confirm prod logs, then open the PR retiring the old path",
        );
        due.id = Some("93co6q".into());
        due.due_at = Some("11:36".into());
        let mut clears = gate("prod secret missing");
        clears.conditions = vec![
            waits("#3806:production", "#3806 in production", WatchTruth::Holds),
            waits(
                "release:production",
                "next production release",
                WatchTruth::NotYet,
            ),
        ];
        let mut row = landed(
            OwedPhase::Due,
            vec![
                due,
                clears,
                check(
                    VerifyStage::Staging,
                    OwedState::Waiting,
                    "Check the worker after the deploy. STEPS: 1. open /health and wait 2. b",
                ),
            ],
        );
        row.watches = vec![WatchView {
            id: "tell44".into(),
            by: "human".into(),
            conditions: vec![waits("production", "in production", WatchTruth::NotYet)],
            actions: vec![WatchActionKind::Notify],
            headline: Some("the release went out".into()),
            ..Default::default()
        }];
        for expanded in [false, true] {
            for width in [18, 30, 44, 80, 200] {
                let lines = work_status_block(&row, expanded, &Times::new(), width);
                assert!(
                    lines.iter().all(|line| line.width() <= width),
                    "{width}: {:?}",
                    plain(&lines)
                );
                let said = plain(&lines)
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                // Wrapped, never cut: the id, the condition and the hint
                // survive every width.
                for needle in ["93co6q", "○ next production release", "tells you when"] {
                    assert!(said.contains(needle), "{width}: {needle}: {said}");
                }
            }
        }
        // Wide: one line an item, the text column aligned.
        let wide = block(&row, 200);
        assert_eq!(wide.iter().filter(|line| line.starts_with("  ")).count(), 3);
        let column = |line: &str, needle: &str| line.find(needle).unwrap();
        assert_eq!(
            column(&wide[2], "confirm prod"),
            column(&wide[4], "check ·")
        );
        // Narrow: an item wraps under its own words, not under its mark.
        let narrow = block(&row, 30);
        assert!(narrow.len() > wide.len());
        let first = narrow
            .iter()
            .position(|line| line.contains("due 11:36"))
            .unwrap();
        assert!(narrow[first].starts_with("  ●  due 11:36"), "{narrow:?}");
        assert!(
            narrow[first + 1].starts_with("     ") && !narrow[first + 1][5..].starts_with(' '),
            "{narrow:?}"
        );
        // A long prompt is cut so its line keeps the id in sight.
        row.owes[0].what = "x".repeat(300);
        let lines = block(&row, 200);
        assert!(lines[2].ends_with("   93co6q") && lines[2].width() < 130);
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
                format!("{bell}  tells you when ○ merged"),
                format!(
                    "{bell}  tells you at 17:00 (in 2h10m) · time to cut the release · by manager"
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
                "{bell}  tells you when ✓ merged · held: automations paused"
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
            [format!("{bell}  tells you when ? merged (no current data)")]
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
        assert_eq!(lines[6], "+2 more (wt watch)");
        // Narrow panes wrap under the words, not under the bell.
        let narrow = notify_lines(&row, &Times::new(), 30);
        assert!(narrow.iter().all(|line| line.width() <= 30), "{narrow:?}");
        let narrow = plain(&narrow);
        let hang = " ".repeat(bell.width() + 2);
        assert!(narrow.len() > 2);
        assert!(
            narrow[2].starts_with(&hang) && !narrow[2][hang.len()..].starts_with(' '),
            "{narrow:?}"
        );
    }

    #[test]
    fn the_pane_orders_header_owes_note_then_facts() {
        let mut waiting = row();
        waiting.owes = vec![check(
            VerifyStage::Staging,
            OwedState::Waiting,
            "open /health",
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
            at("● ready"),
            at("owes"),
            at("in staging"),
            at("tells you when"),
            at("REVERT"),
            at("feature/eng-12"),
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
        let derived = section_screen(vec![row()], crate::OwedBlock::Due.key());
        assert!(!derived.join("\n").contains("J/K move"));
        find(&derived, "TAB expand · y yank");
    }
}
