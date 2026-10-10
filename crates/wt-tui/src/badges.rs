//! The single source of truth for every concept rendered as a glyph, shared
//! by the list and the details pane.
//!
//! 1. Same concept, same glyph everywhere. The list teaches itself because
//!    the details pane uses the identical icon for the identical fact.
//! 2. A glyph and its adjacent label share one color; standalone metadata
//!    (separators, parentheticals) is dim.
//! 3. Details give every state of a state machine a glyph, with color
//!    carrying active versus quiet. The list uses absence as signal instead:
//!    quiet states render nothing, so busy rows stand out.
//! 4. Glyphs are followed by two cells of room (see `glyphs`).

use ratatui::{
    style::{Color, Style},
    text::Span,
};
use unicode_width::UnicodeWidthStr;
use wt_core::{WorkRisk, WorkState};

use crate::{
    BoardRow, CheckState, DisplayPolicy, LandingKind, PrPresentation, ReviewState, SessionView,
    WorkPresentation, glyphs, theme,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Badge {
    pub glyph: &'static str,
    pub color: Color,
}

const fn badge(glyph: &'static str, color: Color) -> Badge {
    Badge { glyph, color }
}

/// Color for an agent-asserted work state. A scan down the dot column answers
/// "what needs me": red is blocked on a human, yellow is verification
/// pending, green is merge it, magenta is in review, cyan is in flight, and
/// dim is queued or finished.
pub(crate) fn work_state_color(state: WorkState) -> Color {
    match state {
        WorkState::NeedsHuman => theme::ERR,
        WorkState::NeedsTesting => theme::WARN,
        WorkState::Ready => theme::OK,
        WorkState::Review => theme::INFO,
        WorkState::Working => theme::ACCENT,
        WorkState::Todo | WorkState::Verified | WorkState::Dropped => theme::FG_DIM,
    }
}

pub(crate) fn work_state_glyph(state: WorkState) -> &'static str {
    match state {
        WorkState::Dropped => glyphs::SLASH,
        // The one terminal state that succeeded wears the merged shape, dim.
        WorkState::Verified => glyphs::MERGE,
        WorkState::Todo => glyphs::DOT_OUTLINE,
        _ => glyphs::DOT,
    }
}

pub(crate) fn risk_color(risk: WorkRisk) -> Color {
    match risk {
        WorkRisk::Low => theme::OK,
        WorkRisk::Medium => theme::WARN,
        WorkRisk::High => theme::ERR,
    }
}

/// The work-status dot. Unasserted rows get the same hollow dim dot as
/// `todo`, because a blank slot reads as a rendering gap. An overdue
/// post-merge verification goes red; a gated `ready` wears a warn
/// circle-slash so a blocked branch never sits green in the merge band; a
/// stale record hollows the dot but keeps its hue.
pub(crate) fn work_status_badge(work: Option<&WorkPresentation>) -> Badge {
    let Some(work) = work else {
        return badge(glyphs::DOT_OUTLINE, theme::FG_DIM);
    };
    let Some(state) = work.effective_state else {
        return badge(glyphs::DOT_OUTLINE, theme::FG_DIM);
    };
    if work.verification_overdue {
        return badge(glyphs::DOT, theme::ERR);
    }
    if work.blocked {
        return badge(glyphs::SLASH, theme::WARN);
    }
    let glyph = if work.stale == Some(true) && !work.derived {
        glyphs::DOT_OUTLINE
    } else {
        work_state_glyph(state)
    };
    badge(glyph, work_state_color(state))
}

/// The loud mechanical states that keep the marker slot: an operation in
/// flight, a vanished path, a deleted remote branch, or landed work. Dirty
/// lives in the badge cluster and clean renders nothing.
pub(crate) fn loud_status_badge(row: &BoardRow) -> Option<Badge> {
    if let Some(busy) = &row.busy {
        return Some(busy_badge(&busy.op));
    }
    if row.path_missing {
        return Some(badge(glyphs::UNLINK, theme::ERR));
    }
    if row.branch_gone {
        return Some(badge(glyphs::SLASH, theme::WARN));
    }
    row.git.landed_on.map(|_| badge(glyphs::MERGE, theme::OK))
}

fn busy_badge(op: &str) -> Badge {
    match op {
        "remove" => badge(glyphs::TRASH, theme::ERR),
        "restack" => badge(glyphs::RESTACK, theme::ACCENT),
        _ => badge(glyphs::ROCKET, theme::ACCENT),
    }
}

pub(crate) fn is_dirty(row: &BoardRow) -> bool {
    row.git.tracked_changes.unwrap_or(0) + row.git.untracked_files.unwrap_or(0) > 0
}

/// Mechanical status as a verb for the details git line: every state has a
/// glyph, colored when active and dim when quiet.
pub(crate) fn status_verb(row: &BoardRow) -> (Badge, String) {
    if let Some(busy) = &row.busy {
        let text = match &busy.age {
            Some(age) => format!("{} · {age}", busy.label),
            None => busy.label.clone(),
        };
        return (busy_badge(&busy.op), text);
    }
    if row.path_missing {
        return (badge(glyphs::UNLINK, theme::ERR), "missing".into());
    }
    if row.branch_gone {
        return (badge(glyphs::SLASH, theme::WARN), "gone".into());
    }
    if let Some(landed) = row.git.landed_on {
        let text = match landed {
            LandingKind::Base => "merged",
            LandingKind::Production => "in production",
        };
        return (badge(glyphs::MERGE, theme::OK), text.into());
    }
    if row.git.tracked_changes.is_none() && row.git.untracked_files.is_none() {
        return (badge(glyphs::CLEAN, theme::FG_DIM), "unknown".into());
    }
    if is_dirty(row) {
        return (badge(glyphs::PENCIL, theme::WARN), "dirty".into());
    }
    (badge(glyphs::CLEAN, theme::FG_DIM), "clean".into())
}

/// Leftmost glyph of a worktree row. With a production branch configured,
/// release position owns the shape and work status owns the color. Without
/// one, the loud mechanical states keep the slot unless a post-merge
/// verification is owed, which only the work dot can say.
pub(crate) fn marker(row: &BoardRow, policy: &DisplayPolicy) -> Badge {
    let work = work_status_badge(row.work.as_ref());
    let owes = row.work.as_ref().is_some_and(|work| work.verification_owed);
    let base = match (row.git.landed_on, policy.production) {
        (Some(LandingKind::Production), true) => Badge {
            glyph: glyphs::PRODUCTION,
            ..work
        },
        (Some(LandingKind::Base), true) => Badge {
            glyph: glyphs::MERGE,
            ..work
        },
        _ if !owes => loud_status_badge(row).unwrap_or(work),
        _ => work,
    };
    if row.archived {
        Badge {
            color: theme::FG_DIM,
            ..base
        }
    } else {
        base
    }
}

/// The session F12 would attach to: the first live one, preferring the
/// primary harness.
pub(crate) fn active_session<'a>(
    row: &'a BoardRow,
    policy: &DisplayPolicy,
) -> Option<&'a SessionView> {
    row.sessions
        .iter()
        .filter(|session| session.live)
        .min_by_key(|session| {
            !session
                .harness
                .eq_ignore_ascii_case(&policy.primary_harness)
        })
}

pub(crate) fn harness_glyph(harness: &str) -> &'static str {
    match harness.to_ascii_lowercase().as_str() {
        "codex" => glyphs::CODEX,
        "opencode" => glyphs::OPENCODE,
        _ => glyphs::CLAUDE,
    }
}

pub(crate) fn harness_color(harness: &str) -> Color {
    match harness.to_ascii_lowercase().as_str() {
        "codex" => theme::CODEX,
        "opencode" => theme::OPENCODE,
        _ => theme::CLAUDE,
    }
}

/// Session state color, per harness for the two brand states. `waiting`
/// ("your turn") wears the brand color; `working` wears its complement.
pub(crate) fn session_state_color(harness: &str, state: &str) -> Color {
    let harness = harness.to_ascii_lowercase();
    match state {
        "working" => match harness.as_str() {
            "codex" => theme::CODEX_ALT,
            "opencode" => theme::OPENCODE_ALT,
            _ => theme::ACCENT,
        },
        "waiting" => harness_color(&harness),
        "asking" => theme::INFO,
        "polling" => theme::TEAL,
        "abandoned" => theme::ERR,
        "idle" => theme::FG_DIM,
        _ => theme::ACCENT_ALT,
    }
}

pub(crate) fn session_state_dot(state: &str) -> &'static str {
    match state {
        "working" => "●",
        "asking" => "?",
        "polling" => "↻",
        "waiting" => "○",
        "abandoned" => "✕",
        "idle" => "·",
        _ => "◌",
    }
}

pub(crate) fn pr_state_badge(pr: &PrPresentation) -> Badge {
    match pr.state.as_deref() {
        Some("MERGED") => badge(glyphs::PR_MERGED, theme::INFO),
        Some("CLOSED") => badge(glyphs::PR_CLOSED, theme::ERR),
        _ if pr.draft => badge(glyphs::PR_DRAFT, theme::FG_DIM),
        _ => badge(glyphs::PR_OPEN, theme::ACCENT_ALT),
    }
}

/// The list's PR slot: an armed-but-not-queued PR shows the merge-queue
/// icon without a position, since the list has no room for a separate
/// auto-merge segment.
pub(crate) fn pr_slot_badge(pr: &PrPresentation) -> Badge {
    if pr.merge_queue.is_none() && pr.is_open() && !pr.draft && pr.auto_merge_armed {
        badge(glyphs::MERGE_QUEUE, theme::INFO)
    } else {
        pr_state_badge(pr)
    }
}

pub(crate) fn check_badge(checks: CheckState) -> Option<Badge> {
    match checks {
        CheckState::Pass => Some(badge(glyphs::CHECK_PASS, theme::OK)),
        CheckState::Fail => Some(badge(glyphs::CHECK_FAIL, theme::ERR)),
        CheckState::Pending => Some(badge(glyphs::CHECK_PENDING, theme::WARN)),
        CheckState::None => None,
    }
}

/// Changes-requested is amber, not alarm red: "needs another pass".
pub(crate) fn review_badge(review: ReviewState, policy: &DisplayPolicy) -> Option<Badge> {
    if !policy.reviewers {
        return None;
    }
    match review {
        ReviewState::Approved => Some(badge(glyphs::THUMBS_UP, theme::OK)),
        ReviewState::ChangesRequested => Some(badge(glyphs::LIGHTBULB, theme::WARN)),
        ReviewState::Pending => Some(badge(glyphs::EYE, theme::WARN)),
        ReviewState::Unrequested => Some(badge(glyphs::EYE, theme::FG_DIM)),
        ReviewState::None => None,
    }
}

pub(crate) fn review_bot_glyph(policy: &DisplayPolicy) -> &'static str {
    if policy.review_bot_carrot {
        glyphs::CARROT
    } else {
        glyphs::REVIEW_CHECKLIST
    }
}

/// A clean review of an older head is not a clean bill of health for this
/// one, so a stale clean review warns instead of reading green.
pub(crate) fn review_bot_badge(pr: &PrPresentation, policy: &DisplayPolicy) -> Option<Badge> {
    if !show_review_bot(pr, policy) {
        return None;
    }
    let bot = pr.review_bot.as_ref()?;
    let glyph = review_bot_glyph(policy);
    match bot.state.as_str() {
        "unresolved" => Some(badge(glyph, theme::INFO)),
        "pending" => Some(badge(glyph, theme::WARN)),
        "clean" => Some(badge(
            glyph,
            if bot.stale { theme::WARN } else { theme::OK },
        )),
        _ => None,
    }
}

/// A thread-mode bot's "skipped draft" run completes like a real review, so
/// drafts hide the badge unless the bot reports through a checklist.
pub(crate) fn show_review_bot(pr: &PrPresentation, policy: &DisplayPolicy) -> bool {
    pr.is_open() && (!pr.draft || policy.review_bot_checklist)
}

/// Merge-queue state: green is about to land, yellow is waiting, red is
/// blocked. Unknown values pass through dim so a new enum surfaces.
pub(crate) fn merge_queue_state(state: &str) -> (String, Color) {
    match state {
        "MERGEABLE" => ("mergeable".into(), theme::OK),
        "AWAITING_CHECKS" => ("awaiting checks".into(), theme::WARN),
        "QUEUED" => ("queued".into(), theme::WARN),
        "UNMERGEABLE" => ("unmergeable".into(), theme::ERR),
        "LOCKED" => ("locked".into(), theme::ERR),
        other => (other.to_ascii_lowercase(), theme::FG_DIM),
    }
}

/// The rebase-lifecycle slot, in priority order: the restack engine holds
/// the lock (accent sync); the checkout sits mid-rebase (warn sync); the
/// branch conflicts with its base but its session is engaged, so it is being
/// resolved (warn sync); or it conflicts and nothing is working on it (red
/// triangle).
pub(crate) fn rebase_badge(row: &BoardRow, session_state: Option<&str>) -> Option<Badge> {
    if row.busy.as_ref().is_some_and(|busy| busy.op == "restack") {
        return Some(badge(glyphs::RESTACK, theme::ACCENT));
    }
    if row.git.rebasing || !row.git.conflict_files.is_empty() {
        return Some(badge(glyphs::RESTACK, theme::WARN));
    }
    if !row.git.base_conflicts.is_empty() {
        if session_state.is_some_and(|state| matches!(state, "working" | "polling" | "asking")) {
            return Some(badge(glyphs::RESTACK, theme::WARN));
        }
        return Some(badge(glyphs::CONFLICT, theme::ERR));
    }
    None
}

fn shows(policy: &DisplayPolicy, slot: &str) -> bool {
    !policy.hidden_badges.iter().any(|hidden| hidden == slot)
}

/// One fixed-width cell group in the right-hand cluster.
pub(crate) struct ClusterSlot {
    pub text: String,
    pub color: Color,
    pub width: usize,
}

/// The right-aligned badge cluster for a worktree row, in a fixed slot
/// order so a glyph's position means the same thing on every row: action,
/// dirty, rebase, environment, session, review bot, human review, PR or
/// merge-queue position, checks.
pub(crate) fn cluster(row: &BoardRow, policy: &DisplayPolicy) -> Vec<ClusterSlot> {
    let mut slots = Vec::new();
    let dim = row.archived.then_some(theme::FG_DIM);
    let mut push = |glyph: &str, color: Color, width: usize| {
        slots.push(ClusterSlot {
            text: glyph.to_owned(),
            color,
            width,
        });
    };
    let session = active_session(row, policy);
    if row.action_running && shows(policy, "action") {
        push(glyphs::COMMENT, theme::OK, 2);
    }
    if is_dirty(row) && shows(policy, "dirty") {
        push(glyphs::PENCIL, dim.unwrap_or(theme::WARN), 2);
    }
    if shows(policy, "rebase")
        && let Some(rebase) = rebase_badge(row, session.map(|session| session.state.as_str()))
    {
        push(rebase.glyph, dim.unwrap_or(rebase.color), 2);
    }
    if row.environment_live && shows(policy, "deploy") {
        push(glyphs::BOLT, dim.unwrap_or(theme::WARN), 2);
    }
    if let Some(session) = session.filter(|_| shows(policy, "session")) {
        push(
            harness_glyph(&session.harness),
            session_state_color(&session.harness, &session.state),
            2,
        );
    }
    if let Some(pr) = row.pr.as_ref().filter(|pr| pr.number.is_some()) {
        if shows(policy, "review_bot")
            && let Some(bot) = review_bot_badge(pr, policy)
        {
            push(bot.glyph, dim.unwrap_or(bot.color), 2);
        }
        if shows(policy, "review")
            && pr.is_open()
            && !pr.draft
            && let Some(review) = review_badge(pr.review, policy)
        {
            push(review.glyph, dim.unwrap_or(review.color), 2);
        }
        if shows(policy, "pr") {
            if let Some(queue) = &pr.merge_queue {
                let position = if queue.position >= 10 {
                    "+".to_owned()
                } else {
                    queue.position.to_string()
                };
                push(
                    &format!("{} {position}", glyphs::MERGE_QUEUE),
                    dim.unwrap_or(merge_queue_state(&queue.state).1),
                    4,
                );
            } else {
                let slot = pr_slot_badge(pr);
                push(slot.glyph, dim.unwrap_or(slot.color), 2);
            }
        }
        if shows(policy, "checks")
            && pr.is_open()
            && let Some(checks) = check_badge(pr.checks)
        {
            push(checks.glyph, dim.unwrap_or(checks.color), 2);
        }
    }
    slots
}

/// Cells the cluster occupies including its two-cell lead gap; zero when
/// empty so a quiet row gives all its room to the title.
pub(crate) fn cluster_width(slots: &[ClusterSlot]) -> usize {
    match slots.iter().map(|slot| slot.width).sum::<usize>() {
        0 => 0,
        content => content + 2,
    }
}

/// Render cluster slots as spans, each padded to its slot width.
pub(crate) fn cluster_spans(
    slots: &[ClusterSlot],
    background: Option<Color>,
) -> Vec<Span<'static>> {
    if slots.is_empty() {
        return Vec::new();
    }
    let style = |color: Color| {
        let style = Style::new().fg(color);
        background.map_or(style, |bg| style.bg(bg))
    };
    let mut spans = vec![Span::styled("  ", style(theme::FG))];
    for slot in slots {
        let pad = slot.width.saturating_sub(slot.text.width());
        spans.push(Span::styled(
            format!("{}{}", slot.text, " ".repeat(pad)),
            style(slot.color),
        ));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GitPresentation, MergeQueueView, ReviewBotView};

    fn row() -> BoardRow {
        BoardRow {
            key: "one".into(),
            git: GitPresentation {
                tracked_changes: Some(0),
                untracked_files: Some(0),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn work(state: WorkState) -> WorkPresentation {
        WorkPresentation {
            effective_state: Some(state),
            ..Default::default()
        }
    }

    #[test]
    fn work_dot_follows_state_color_staleness_gate_and_overdue() {
        assert_eq!(
            work_status_badge(None),
            badge(glyphs::DOT_OUTLINE, theme::FG_DIM)
        );
        assert_eq!(
            work_status_badge(Some(&work(WorkState::Ready))),
            badge(glyphs::DOT, theme::OK)
        );
        let stale = WorkPresentation {
            stale: Some(true),
            ..work(WorkState::Ready)
        };
        assert_eq!(
            work_status_badge(Some(&stale)),
            badge(glyphs::DOT_OUTLINE, theme::OK)
        );
        let gated = WorkPresentation {
            blocked: true,
            ..work(WorkState::Ready)
        };
        assert_eq!(
            work_status_badge(Some(&gated)),
            badge(glyphs::SLASH, theme::WARN)
        );
        let overdue = WorkPresentation {
            verification_overdue: true,
            blocked: true,
            ..work(WorkState::Ready)
        };
        assert_eq!(
            work_status_badge(Some(&overdue)),
            badge(glyphs::DOT, theme::ERR)
        );
        assert_eq!(
            work_status_badge(Some(&work(WorkState::Verified))).glyph,
            glyphs::MERGE
        );
    }

    #[test]
    fn marker_prefers_loud_git_state_unless_verification_is_owed() {
        let policy = DisplayPolicy::default();
        let mut landed = row();
        landed.git.landed_on = Some(LandingKind::Base);
        landed.work = Some(work(WorkState::Ready));
        assert_eq!(marker(&landed, &policy), badge(glyphs::MERGE, theme::OK));
        landed.work.as_mut().unwrap().verification_owed = true;
        landed.work.as_mut().unwrap().effective_state = Some(WorkState::NeedsTesting);
        assert_eq!(marker(&landed, &policy), badge(glyphs::DOT, theme::WARN));
        let production = DisplayPolicy {
            production: true,
            ..Default::default()
        };
        landed.git.landed_on = Some(LandingKind::Production);
        assert_eq!(
            marker(&landed, &production),
            badge(glyphs::PRODUCTION, theme::WARN)
        );
        let mut busy = row();
        busy.busy = Some(crate::BusyView {
            op: "remove".into(),
            ..Default::default()
        });
        assert_eq!(marker(&busy, &policy), badge(glyphs::TRASH, theme::ERR));
        busy.archived = true;
        assert_eq!(marker(&busy, &policy).color, theme::FG_DIM);
    }

    #[test]
    fn cluster_keeps_slot_order_and_absence_as_signal() {
        let policy = DisplayPolicy {
            reviewers: true,
            primary_harness: "Claude".into(),
            ..Default::default()
        };
        let quiet = row();
        assert!(cluster(&quiet, &policy).is_empty());
        assert_eq!(cluster_width(&cluster(&quiet, &policy)), 0);

        let mut busy = row();
        busy.git.tracked_changes = Some(2);
        busy.git.base_conflicts = vec!["a.rs".into()];
        busy.environment_live = true;
        busy.sessions = vec![SessionView {
            harness: "Claude".into(),
            state: "idle".into(),
            live: true,
            ..Default::default()
        }];
        busy.pr = Some(PrPresentation {
            number: Some(7),
            state: Some("OPEN".into()),
            checks: CheckState::Fail,
            review: ReviewState::Approved,
            review_bot: Some(ReviewBotView {
                state: "clean".into(),
                stale: true,
                ..Default::default()
            }),
            merge_queue: Some(MergeQueueView {
                position: 12,
                state: "QUEUED".into(),
            }),
            ..Default::default()
        });
        let slots = cluster(&busy, &policy);
        let glyphs: Vec<_> = slots.iter().map(|slot| slot.text.as_str()).collect();
        assert_eq!(
            glyphs,
            [
                glyphs::PENCIL,
                glyphs::CONFLICT,
                glyphs::BOLT,
                glyphs::CLAUDE,
                glyphs::REVIEW_CHECKLIST,
                glyphs::THUMBS_UP,
                &format!("{} +", glyphs::MERGE_QUEUE),
                glyphs::CHECK_FAIL,
            ]
        );
        assert_eq!(slots[4].color, theme::WARN, "stale clean bot review warns");
        assert_eq!(cluster_width(&slots), 2 * 7 + 4 + 2);

        let hidden = DisplayPolicy {
            hidden_badges: vec!["dirty".into(), "pr".into()],
            ..policy.clone()
        };
        let hidden_glyphs: Vec<_> = cluster(&busy, &hidden)
            .into_iter()
            .map(|slot| slot.text)
            .collect();
        assert!(!hidden_glyphs.iter().any(|glyph| glyph == glyphs::PENCIL));
        assert!(
            !hidden_glyphs
                .iter()
                .any(|glyph| glyph.starts_with(glyphs::MERGE_QUEUE))
        );
    }

    #[test]
    fn engaged_session_turns_a_base_conflict_into_resolving() {
        let mut conflicted = row();
        conflicted.git.base_conflicts = vec!["a.rs".into()];
        assert_eq!(
            rebase_badge(&conflicted, None),
            Some(badge(glyphs::CONFLICT, theme::ERR))
        );
        assert_eq!(
            rebase_badge(&conflicted, Some("working")),
            Some(badge(glyphs::RESTACK, theme::WARN))
        );
        assert_eq!(
            rebase_badge(&conflicted, Some("idle")),
            Some(badge(glyphs::CONFLICT, theme::ERR))
        );
    }

    #[test]
    fn armed_open_pr_takes_the_merge_queue_icon_in_the_list_only() {
        let pr = PrPresentation {
            number: Some(1),
            state: Some("OPEN".into()),
            auto_merge_armed: true,
            ..Default::default()
        };
        assert_eq!(pr_slot_badge(&pr).glyph, glyphs::MERGE_QUEUE);
        assert_eq!(pr_state_badge(&pr).glyph, glyphs::PR_OPEN);
    }
}
