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
    BoardRow, BoardSection, CheckState, DisplayPolicy, Model, PrPresentation, PreparedDetailGroup,
    ReviewRequestRow, ReviewState, SessionView, badges, glyphs, theme,
};

/// Conflicting files listed before collapsing into a `+N more` tail.
const MAX_CONFLICT_FILES: usize = 8;
/// Preamble lines of collapsed post-merge verification steps.
const COLLAPSED_PREAMBLE_LINES: usize = 2;
/// Lines each blocker note may spend in a folded-section summary.
const SECTION_NOTE_LINES: usize = 4;
/// Key of the board's pinned archived section (`wt-app` board layout).
const ARCHIVED_SECTION: &str = "\0archived";
/// Labels recognized at the start of a structured ready note.
const NOTE_LABELS: [&str; 4] = ["IF WRONG", "UNTESTED", "REVERT", "OPS"];

type Spans = Vec<Span<'static>>;

pub(crate) fn render(frame: &mut Frame<'_>, model: &mut Model, area: Rect) {
    let title = pane_title(model);
    let block = super::panel_owned(truncate_end(
        &title,
        usize::from(area.width).saturating_sub(6),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 3 || inner.height == 0 {
        return;
    }
    // One cell of padding each side; the right one doubles as the
    // scrollbar's gutter so a thumb never sits on text. A blank row above
    // the content (outside the scroll region) matches the TS pane's top
    // padding, so the title never sits against the border.
    let mut content = inner.inner(Margin {
        vertical: 0,
        horizontal: 1,
    });
    if content.height > 3 {
        content.y += 1;
        content.height -= 1;
    }
    let width = usize::from(content.width).max(1);
    let lines = build(model, width);
    let height = usize::from(content.height);
    let max_scroll = lines.len().saturating_sub(height);
    let scroll = usize::from(model.details_scroll).min(max_scroll);
    model.details_scroll = u16::try_from(scroll).unwrap_or(u16::MAX);
    let total = lines.len();
    frame.render_widget(
        Paragraph::new(lines).scroll((model.details_scroll, 0)),
        content,
    );
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
            "Requested reviews · Tab to fold or expand"
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
    let work = work_status_block(row, model.show_verification, width);
    if !work.is_empty() {
        lines.extend(work);
    }
    lines.push(Line::default());

    let label_width = label_column(&row.detail_groups);
    for group in &row.detail_groups {
        if group.id == "summary" {
            continue;
        }
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
                    .map(|line| Line::styled(line, theme::dim())),
            );
        }
    }

    let rebase = rebase_block(row, policy, width);
    if !rebase.is_empty() {
        lines.push(Line::default());
        lines.extend(rebase);
    }

    if row.automations_paused && policy.automations {
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
                .map(|line| Line::styled(line, theme::dim())),
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
        "branch" | "path" | "issue" | "stage" | "dev" | "pr" | "git" => group.id.clone(),
        _ => group.label.to_lowercase(),
    }
}

/// Label column: the widest label present plus a one-cell gap.
fn label_column(groups: &[PreparedDetailGroup]) -> usize {
    groups
        .iter()
        .filter(|group| group.id != "summary")
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
        "pr" => pr_value(row.pr.as_ref(), policy, value_width),
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
    let (glyph, color) = issue_status_badge(row.issue_status.as_deref());
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

/// Tracker status glyph. Configured status styles are not part of the
/// prepared board, so this keys on the common workflow words and falls back
/// to a dim outline.
fn issue_status_badge(status: Option<&str>) -> (&'static str, ratatui::style::Color) {
    let Some(status) = status.map(str::to_ascii_lowercase) else {
        return (glyphs::DOT_OUTLINE, theme::FG_DIM);
    };
    if status.contains("cancel") || status.contains("duplicate") {
        (glyphs::TASK_CANCELLED, theme::FG_DIM)
    } else if status.contains("done") || status.contains("complete") || status.contains("closed") {
        (glyphs::TASK_COMPLETE, theme::OK)
    } else if status.contains("review") {
        (glyphs::HALF_CIRCLE, theme::INFO)
    } else if status.contains("progress") || status.contains("started") {
        (glyphs::DOT_CIRCLE, theme::WARN)
    } else if status.contains("block") {
        (glyphs::SLASH, theme::ERR)
    } else {
        (glyphs::DOT_OUTLINE, theme::FG_DIM)
    }
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

fn pr_value(pr: Option<&PrPresentation>, policy: &DisplayPolicy, width: usize) -> Vec<Spans> {
    let Some(pr) = pr else {
        return vec![vec![Span::styled("—", theme::dim())]];
    };
    let Some(number) = pr.number else {
        // Only an error is known; the caller renders it from the group.
        return if pr.error.is_some() {
            Vec::new()
        } else {
            vec![vec![Span::styled("—", theme::dim())]]
        };
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
    vec![fit_segments(segments, width)]
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
    // Name where it landed, as TS does ("merged into origin/main").
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
            vec![
                vec![Span::styled(format!("created {age}"), theme::dim())],
                vec![Span::styled(format!("+{age}"), theme::dim())],
            ],
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

/// The rebase lifecycle as a block: restacking, mid-rebase, or a branch
/// that will not rebase cleanly onto its base (with the clashing files).
fn rebase_block(row: &BoardRow, policy: &DisplayPolicy, width: usize) -> Vec<Line<'static>> {
    let session_state = badges::active_session(row, policy).map(|session| session.state.as_str());
    let Some(badge) = badges::rebase_badge(row, session_state) else {
        return Vec::new();
    };
    let header = |text: String| fit_line(glyph_text(badge.glyph, &text, badge.color), width);
    let files = |files: &[String], lines: &mut Vec<Line<'static>>| {
        for file in files.iter().take(MAX_CONFLICT_FILES) {
            lines.push(Line::styled(
                truncate_end(&format!("   {file}"), width),
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
        return vec![header(format!("Restacking ({phase})"))];
    }
    if row.git.rebasing || !row.git.conflict_files.is_empty() {
        let mut lines = wrap(
            &format!(
                "{}  Mid-rebase: resolve and continue in this worktree (/restack)",
                badge.glyph
            ),
            width,
        )
        .into_iter()
        .map(|line| Line::styled(line, theme::fg(badge.color)))
        .collect();
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
    let mut lines = vec![header(format!(
        "{head} · {count} conflicting file{}",
        if count == 1 { "" } else { "s" }
    ))];
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
    work_status_block(row, show_verification, 200)
}

/// Full-width work-status banner: header (state, risk, age, staleness),
/// then gate, note, and post-merge steps, in that order. The gate decides
/// whether the row may merge at all, the note is what a merger needs now,
/// and the steps belong to whoever holds the row after it lands.
fn work_status_block(row: &BoardRow, show_verification: bool, width: usize) -> Vec<Line<'static>> {
    let Some(work) = row.work.as_ref() else {
        return Vec::new();
    };
    let Some(effective) = work.effective_state else {
        return Vec::new();
    };
    let record = work.record.as_ref();
    let state = if work.verification_owed || work.blocked {
        record.map_or(effective, |record| record.state)
    } else {
        effective
    };
    let color = if work.verification_overdue {
        theme::ERR
    } else if work.blocked || work.verification_owed {
        theme::WARN
    } else {
        badges::work_state_color(state)
    };
    let (glyph, text) = if work.blocked {
        ("⊘", format!("blocked · {}", state.as_str()))
    } else if work.verification_owed {
        ("●", format!("unverified · {}", state.as_str()))
    } else if work.derived {
        (banner_glyph(state), format!("{} · live", state.as_str()))
    } else {
        (banner_glyph(state), state.as_str().to_owned())
    };
    let mut header = vec![
        Span::styled(format!("{glyph} "), theme::fg(color)),
        Span::styled(text, theme::bold(color)),
    ];
    if let Some(risk) = record.and_then(|record| record.risk) {
        header.push(Span::styled(" · risk ", theme::dim()));
        header.push(Span::styled(
            risk.as_str(),
            theme::fg(badges::risk_color(risk)),
        ));
    }
    if let Some(age) = work
        .age
        .as_ref()
        .filter(|_| !work.derived || record.is_some())
    {
        header.push(Span::styled(format!(" · {age} ago"), theme::dim()));
    }
    if work.stale == Some(true) && !work.derived {
        header.push(Span::styled(" · commits since", theme::fg(theme::WARN)));
    }
    let mut lines = wrap_spans(header, width);
    if let Some(gate) = record.and_then(|record| record.blocked_on.as_deref()) {
        lines.extend(
            wrap(&format!("blocked on: {gate}"), width)
                .into_iter()
                .map(|line| Line::styled(line, theme::fg(theme::WARN))),
        );
    }
    if let Some(note) = record.and_then(|record| record.note.as_deref()) {
        lines.extend(note_lines(note, color, width));
    }
    if let Some(steps) = record.and_then(|record| record.verify_after_merge.as_deref()) {
        // The model holds the open state; it defaults to open exactly when
        // the check is due and `V` toggles it.
        let expanded = show_verification;
        lines.extend(verify_lines(
            steps,
            color,
            work.verification_owed,
            work.verification_overdue,
            expanded,
            width,
        ));
    }
    lines
}

/// Base-font circles center like the note rail's `│`, unlike the Nerd Font
/// dot, so the banner glyph lines up with the rail beneath it.
fn banner_glyph(state: WorkState) -> &'static str {
    if state == WorkState::Todo {
        "○"
    } else {
        "●"
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
    let rail = || Span::styled("│ ", theme::fg(color));
    let mut lines = Vec::new();
    for (label, body) in sections {
        let indent = if label.is_some() { gutter } else { 0 };
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
            Some("OPS") if body.trim().to_ascii_lowercase().starts_with("none") => theme::FG_DIM,
            _ => theme::FG_MID,
        };
        let label_color = if label == Some("UNTESTED") {
            theme::WARN
        } else {
            theme::FG_DIM
        };
        for (index, line) in wrap(&body, inner.saturating_sub(indent).max(1))
            .into_iter()
            .enumerate()
        {
            let lead = match label {
                Some(label) if index == 0 => {
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

/// Split a ready note at `IF WRONG:`, `UNTESTED:`, `REVERT:`, and `OPS:`
/// labels that start a word and are followed by whitespace. Text before the
/// first label is the unlabeled lead.
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

/// Post-merge steps: header with step count and the `V` hint, a preamble
/// (clipped while collapsed), and numbered steps when expanded. State color
/// marks the structure only while the check is owed; before that it is
/// dormant and dim.
fn verify_lines(
    text: &str,
    color: ratatui::style::Color,
    owed: bool,
    overdue: bool,
    expanded: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let (preamble, steps) = parse_verify_steps(text);
    let head_color = if owed { color } else { theme::FG_DIM };
    let body_color = if owed { theme::FG_MID } else { theme::FG_DIM };
    let mut header = vec![Span::styled(
        format!(
            "{}verify after merge",
            if overdue { "OVERDUE · " } else { "" }
        ),
        if owed {
            theme::bold(head_color)
        } else {
            theme::fg(head_color)
        },
    )];
    if !steps.is_empty() {
        header.push(Span::styled(
            format!(
                " · {} step{}",
                steps.len(),
                if steps.len() == 1 { "" } else { "s" }
            ),
            theme::dim(),
        ));
    }
    header.push(Span::styled(
        if expanded {
            " · V collapses"
        } else {
            " · V expands"
        },
        theme::dim(),
    ));
    let mut lines = vec![fit_line(header, width)];
    let preamble = if expanded {
        wrap(&preamble, width)
    } else {
        clip_lines(&preamble, width, COLLAPSED_PREAMBLE_LINES)
    };
    lines.extend(
        preamble
            .into_iter()
            .filter(|line| !line.is_empty())
            .map(|line| Line::styled(line, theme::fg(body_color))),
    );
    if expanded {
        let gutter = steps.len().max(1).to_string().len() + 2;
        for (index, step) in steps.iter().enumerate() {
            for (part, line) in wrap(step, width.saturating_sub(gutter).max(1))
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
                    lead,
                    Span::styled(line, theme::fg(body_color)),
                ]));
            }
        }
    }
    lines
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
    let hint = match (section.folded, section.key == ARCHIVED_SECTION) {
        (true, true) => "TAB expand · y yank",
        (true, false) => "TAB expand · y yank · L rename · J/K move",
        (false, true) => "TAB fold · y yank",
        (false, false) => "TAB fold · y yank · L rename · J/K move",
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
pub(crate) fn section_rollup_lines(section: &BoardSection) -> Vec<Line<'static>> {
    let (mut lines, blocked) = section_summary(section, &[], 200);
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
    let rollup = &section.rollup;
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
    fact(rollup.stale_statuses, "stale status", theme::WARN);
    fact(rollup.verification_owed, "verify owed", theme::WARN);
    fact(rollup.verification_overdue, "verify overdue", theme::ERR);
    fact(rollup.paused_automations, "paused", theme::WARN);
    fact(rollup.needs_attention, "need attention", theme::WARN);
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

/// The prepared rollup's states, most urgent first by the shared section
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
        let room = width - used;
        out.push(Span::styled(truncate_end(&span.content, room), span.style));
        break;
    }
    Line::from(out)
}

/// Wrap a styled line at span boundaries and spaces.
fn wrap_spans(spans: Spans, width: usize) -> Vec<Line<'static>> {
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
            title_source: TitleSource::Llm,
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
        assert!(find(&lines, "Fix the flaky login test").contains("(llm)"));
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

    fn section_screen(rows: Vec<BoardRow>, key: &str) -> Vec<String> {
        let count = rows.len();
        let mut board = Board {
            rows,
            sections: vec![BoardSection {
                key: key.into(),
                title: "To Merge".into(),
                folded: true,
                rows: (0..count).collect(),
                ..Default::default()
            }],
            ..Default::default()
        };
        board.sections[0].rollup.states = vec![
            crate::WorkStateCount {
                state: Some(WorkState::Working),
                count: 1,
            },
            crate::WorkStateCount {
                state: Some(WorkState::NeedsHuman),
                count: 1,
            },
            crate::WorkStateCount {
                state: Some(WorkState::Ready),
                count: 1,
            },
        ];
        board.sections[0].rollup.blocked_notes = vec!["gated: release approval".into()];
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
        let lines = section_screen(vec![row(), asking], "manual");
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
        let rollup = find(&lines, "2 worktrees");
        let ready = rollup.find("ready").unwrap();
        let human = rollup.find("needs-human").unwrap();
        let working = rollup.find("working").unwrap();
        assert!(ready < human && human < working, "{rollup}");
        find(&lines, "TAB expand · y yank · L rename · J/K move");

        let archived = section_screen(vec![row()], ARCHIVED_SECTION);
        assert!(!archived.join("\n").contains("L rename"));
        find(&archived, "TAB expand · y yank");
    }
}
