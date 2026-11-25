//! One worktree's history as a timeline: the `H` modal and the history block
//! of a removal record. Everything here is presentation of
//! `Board::row_history`; nothing reads the event log.
//!
//! A line is a time, a glyph, a headline in the glyph's color, dim metadata
//! and, for a note or a message, its text. Glyphs come from `badges`, so an
//! event wears what the board drew for the same fact.

use std::collections::BTreeMap;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use wt_core::{
    AutomationOutcomeKind, ChecksChange, LandedOn, PrChange, QueueChange, ReviewChange, WorkState,
    WorktreeEvent, WorktreeEventKind as Kind,
};

use super::{
    overlays::{Modal, fit_spans, hint, scrollbar},
    text::{age, now_ms, truncate_end},
};
use crate::{
    Board, DisplayPolicy, LocalStamp,
    badges::{self, Badge},
    model::TimelinePrompt,
    theme,
};

/// The time column: a clock (`14:32`) or a right-aligned age (`3d`).
const TIME_CELLS: usize = 5;
/// Fires of one automation this close together are one burst.
const BURST_MS: u64 = 10 * 60 * 1000;
/// A status has to change this many times in a row to read as flapping.
const FLAP_MIN: usize = 3;
/// Narrowest content the modal asks for, so a short history is not a sliver.
const MIN_CONTENT_CELLS: usize = 64;
/// Timeline rows a pinned header must leave; a shorter frame scrolls the
/// header away with the rest.
const PINNED_MIN_ROWS: usize = 4;

const EMPTY: &str = "No history yet. Recording starts from now; older worktrees predate the log.";

/// One timeline line: an event, or a run of events folded into one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry<'a> {
    /// When its latest event happened.
    pub at_ms: u64,
    /// When its oldest event happened: stable while the entry absorbs newer
    /// ones, so it identifies the line across snapshots.
    pub first_ms: u64,
    /// The status standing just before its oldest event, which decides
    /// what a status line still has to say about a gate or an owed check.
    pub prev: Option<&'a Kind>,
    pub what: What<'a>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum What<'a> {
    Event(&'a Kind),
    /// The status went back and forth between two states; `now` is where
    /// it ended.
    Flap {
        now: WorkState,
        other: WorkState,
        changes: usize,
    },
    /// Review rounds in a row from one reviewer; `count` is the latest.
    Rounds {
        reviewer: Option<&'a str>,
        count: u32,
        rounds: usize,
    },
    /// One automation outcome repeated within [`BURST_MS`] of the last.
    Burst {
        event: &'a Kind,
        times: usize,
    },
    /// What wt did about a met watch, as `wt_core::fold_watch_history`
    /// gathered it.
    Fired(wt_core::WatchFired<'a>),
}

/// One line of the folded history, with the status standing before it.
struct Known<'a> {
    at_ms: u64,
    first_ms: u64,
    prev: Option<&'a Kind>,
    step: Step<'a>,
}

enum Step<'a> {
    Event(&'a Kind),
    Fired(wt_core::WatchFired<'a>),
}

impl<'a> Known<'a> {
    fn event(&self) -> Option<&'a Kind> {
        match &self.step {
            Step::Event(event) => Some(event),
            Step::Fired(_) => None,
        }
    }
}

/// Fold noise, oldest first in and out. Besides a watch's own events, only
/// events next to each other fold, so a line never hides something that
/// happened in between. Kinds this build does not know are dropped.
pub(crate) fn collapse(events: &[WorktreeEvent]) -> Vec<Entry<'_>> {
    let mut standing = None;
    let mut known: Vec<Known<'_>> = Vec::new();
    // A wake delivered after its worktree landed records that the step came
    // due where another wake records "woken"; either line says it held.
    let came_due = |held: &WorktreeEvent| {
        let Kind::Watch {
            id: Some(held_id),
            change: wt_core::WatchChange::Held,
            ..
        } = &held.event
        else {
            return false;
        };
        events.iter().any(|event| {
            event.at_ms >= held.at_ms
                && matches!(
                    &event.event,
                    Kind::Owed { id: Some(id), change: wt_core::OwedChange::Due, .. }
                        if id == held_id
                )
        })
    };
    for line in wt_core::fold_watch_history(events) {
        let prev = standing;
        // A status folded into a watch's line still stands.
        if let wt_core::HistoryLine::Event(event) | wt_core::HistoryLine::Folded(event) = &line {
            match &event.event {
                Kind::Status { .. } => standing = Some(&event.event),
                Kind::StatusCleared { .. } => standing = None,
                _ => {}
            }
        }
        match line {
            wt_core::HistoryLine::Event(event) if came_due(event) => {}
            wt_core::HistoryLine::Event(event) if event.event != Kind::Unknown => {
                known.push(Known {
                    at_ms: event.at_ms,
                    first_ms: event.at_ms,
                    prev,
                    step: Step::Event(&event.event),
                });
            }
            wt_core::HistoryLine::Fired(fired) => known.push(Known {
                at_ms: fired.at_ms,
                first_ms: fired.first_ms,
                prev,
                step: Step::Fired(fired),
            }),
            wt_core::HistoryLine::Event(_) | wt_core::HistoryLine::Folded(_) => {}
        }
    }
    let mut entries = Vec::new();
    let mut index = 0;
    while index < known.len() {
        let (what, len) = run_at(&known[index..]);
        entries.push(Entry {
            at_ms: known[index + len - 1].at_ms,
            first_ms: known[index].first_ms,
            prev: known[index].prev,
            what,
        });
        index += len;
    }
    entries
}

/// The gate and the owed check a status carries. Every later assertion
/// repeats them until they change, so only a difference is news.
fn carried(status: Option<&Kind>) -> (Option<&str>, Option<&str>) {
    match status {
        Some(Kind::Status {
            blocked_on,
            verify_in,
            ..
        }) => (blocked_on.as_deref(), verify_in.as_deref()),
        _ => (None, None),
    }
}

/// A status change that sets or clears no gate or owed check; one that
/// does is never noise.
fn plain_status(event: &Kind, prev: Option<&Kind>) -> Option<WorkState> {
    match event {
        Kind::Status { to, .. } if carried(Some(event)) == carried(prev) => Some(*to),
        _ => None,
    }
}

/// The entry starting at `events[0]` and how many events it takes.
fn run_at<'a>(events: &[Known<'a>]) -> (What<'a>, usize) {
    let first = match &events[0].step {
        Step::Event(event) => *event,
        Step::Fired(fired) => return (What::Fired(fired.clone()), 1),
    };
    if plain_status(first, events[0].prev).is_some() {
        let states: Vec<WorkState> = events
            .iter()
            .map_while(|known| plain_status(known.event()?, known.prev))
            .collect();
        let mut len = 1;
        while len < states.len()
            && states[len] != states[len - 1]
            && (len < 2 || states[len] == states[len - 2])
        {
            len += 1;
        }
        if len >= FLAP_MIN {
            let flap = What::Flap {
                now: states[len - 1],
                other: states[len - 2],
                changes: len,
            };
            return (flap, len);
        }
    }
    if let Kind::Review {
        reviewer,
        change: ReviewChange::Findings { .. },
    } = first
    {
        let counts: Vec<u32> = events
            .iter()
            .map_while(|known| match known.event()? {
                Kind::Review {
                    reviewer: other,
                    change: ReviewChange::Findings { count },
                } if other == reviewer => Some(*count),
                _ => None,
            })
            .collect();
        if counts.len() >= 2 {
            let rounds = What::Rounds {
                reviewer: reviewer.as_deref(),
                count: counts[counts.len() - 1],
                rounds: counts.len(),
            };
            return (rounds, counts.len());
        }
    }
    if matches!(first, Kind::Automation { .. }) {
        let mut len = 1;
        while len < events.len()
            && events[len].event() == Some(first)
            && events[len].at_ms.saturating_sub(events[len - 1].at_ms) <= BURST_MS
        {
            len += 1;
        }
        if len >= 2 {
            return (
                What::Burst {
                    event: first,
                    times: len,
                },
                len,
            );
        }
    }
    (What::Event(first), 1)
}

/// What one line says, before layout.
struct Shown {
    badge: Badge,
    headline: String,
    /// Who, which PR, which rule: joined with dots, dim.
    meta: Vec<String>,
    /// A note or a quoted message.
    body: Option<String>,
}

fn findings(count: u32) -> String {
    format!("review: {}", plural(count as usize, "finding"))
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// `own` is the row's slug: its agent asserting its own status is the
/// normal case and is not named.
fn show(entry: &Entry<'_>, policy: &DisplayPolicy, own: &str) -> Option<Shown> {
    match &entry.what {
        What::Event(event) => show_event(event, entry.prev, policy, own),
        What::Flap {
            now,
            other,
            changes,
        } => Some(Shown {
            // A flap changes no gate, so the one standing before it holds.
            badge: badges::asserted_status_badge(*now, carried(entry.prev).0.is_some()),
            headline: format!("{} ↔ {}", now.as_str(), other.as_str()),
            meta: vec![format!("×{changes}")],
            body: None,
        }),
        What::Rounds {
            reviewer,
            count,
            rounds,
        } => {
            let mut shown = show_event(
                &Kind::Review {
                    reviewer: reviewer.map(str::to_owned),
                    change: ReviewChange::Findings { count: *count },
                },
                None,
                policy,
                own,
            )?;
            shown.meta.insert(0, format!("{rounds} rounds"));
            Some(shown)
        }
        What::Burst { event, times } => {
            let mut shown = show_event(event, None, policy, own)?;
            shown.meta.insert(0, format!("×{times}"));
            Some(shown)
        }
        What::Fired(fired) => Some(show_fired(fired)),
    }
}

fn show_fired(fired: &wt_core::WatchFired<'_>) -> Shown {
    let words = wt_core::watch_fired_words(&fired.done, fired.unconfirmed, fired.what);
    let mut meta = vec![words.about];
    if fired.just_set {
        meta.push("as soon as it was set".to_owned());
    }
    Shown {
        badge: badges::watch_fired_badge(fired.unconfirmed),
        headline: words.headline,
        meta,
        body: None,
    }
}

fn show_event(
    event: &Kind,
    prev: Option<&Kind>,
    policy: &DisplayPolicy,
    own: &str,
) -> Option<Shown> {
    // No name at all is not shown as a person: an assertion made without
    // `WT_AGENT` set records none either.
    let named = |by: &Option<String>| {
        by.as_deref()
            .filter(|by| *by != own)
            .map(|by| format!("by {by}"))
    };
    let badge = badges::event_badge(event, policy)?;
    let mut meta: Vec<String> = Vec::new();
    let mut body = None;
    let headline = match event {
        Kind::Created { base } => {
            meta.extend(base.as_ref().map(|base| format!("from {base}")));
            "created".to_owned()
        }
        Kind::Status {
            to,
            by,
            risk,
            note,
            blocked_on,
            verify_in,
            ..
        } => {
            let (gate_was, verify_was) = carried(prev);
            match (blocked_on.as_deref(), gate_was) {
                (Some(gate), was) if was != Some(gate) => meta.push(format!("blocked on {gate}")),
                (None, Some(_)) => meta.push("gate cleared".to_owned()),
                _ => {}
            }
            meta.extend(risk.map(|risk| format!("{} risk", risk.as_str())));
            if let Some(stage) = verify_in.as_deref()
                && verify_was != Some(stage)
            {
                meta.push(format!("verify in {stage}"));
            }
            meta.extend(named(by));
            body = note.clone();
            to.as_str().to_owned()
        }
        Kind::StatusCleared { from, by } => {
            meta.extend(from.map(|from| format!("was {}", from.as_str())));
            meta.extend(named(by));
            "status cleared".to_owned()
        }
        Kind::Pr { number, change } => {
            meta.push(format!("#{number}"));
            match change {
                PrChange::Opened => "PR opened",
                PrChange::OpenedDraft => "PR opened as draft",
                PrChange::ReadyForReview => "PR ready for review",
                PrChange::BackToDraft => "PR back to draft",
                PrChange::Closed => "PR closed",
                PrChange::Reopened => "PR reopened",
                PrChange::Merged => "PR merged",
            }
            .to_owned()
        }
        Kind::Checks { state, failed } => match state {
            ChecksChange::Failing => {
                if !failed.is_empty() {
                    meta.push(failed.join(", "));
                }
                "checks failing".to_owned()
            }
            ChecksChange::Passing => "checks passing".to_owned(),
        },
        Kind::Review { reviewer, change } => {
            meta.extend(reviewer.clone());
            match change {
                ReviewChange::Findings { count } => findings(*count),
                ReviewChange::Clean => "review clean".to_owned(),
                ReviewChange::ChangesRequested => "changes requested".to_owned(),
                ReviewChange::Approved => "approved".to_owned(),
            }
        }
        Kind::MergeQueue { change } => match change {
            QueueChange::Queued => "queued to merge",
            QueueChange::Left => "left the merge queue",
        }
        .to_owned(),
        Kind::Conflict { resolved } => if *resolved {
            "conflict resolved"
        } else {
            "conflicts with its base"
        }
        .to_owned(),
        // The words the details pane uses for the same landing.
        Kind::Landed { on } => match on {
            LandedOn::Base => "merged",
            LandedOn::Production => "in production",
        }
        .to_owned(),
        Kind::Automation {
            rule,
            outcome,
            reason,
        } => {
            meta.push(rule.clone());
            meta.extend(reason.clone());
            match outcome {
                AutomationOutcomeKind::Delivered => "automation delivered",
                AutomationOutcomeKind::Skipped => "automation skipped",
                AutomationOutcomeKind::Failed => "automation failed",
                AutomationOutcomeKind::Unconfirmed => "automation unconfirmed",
            }
            .to_owned()
        }
        Kind::Message {
            from,
            text,
            delivered,
        } => {
            meta.extend(from.as_ref().map(|from| format!("from {from}")));
            meta.push(
                match delivered {
                    Some(true) => "delivered",
                    None => "unconfirmed",
                    Some(false) => "not delivered",
                }
                .to_owned(),
            );
            body = Some(format!("\"{text}\""));
            "message".to_owned()
        }
        Kind::Owed {
            id,
            change,
            what,
            evidence,
            by,
            stage,
            when,
            unconfirmed,
        } => {
            use wt_core::OwedChange as Change;
            // One line: the first line of what was owed, a check named by
            // its stage. The evidence follows dimly.
            let what = wt_core::watch::headline(what);
            let what = match stage {
                Some(stage) => format!("{stage} check: {what}"),
                None => what,
            };
            let headline = match (change, when) {
                (Change::Due, _) => {
                    meta.extend(
                        id.as_deref()
                            .or(stage.as_deref())
                            .map(|target| format!("wt done {target}")),
                    );
                    if *unconfirmed {
                        meta.push("wake sent, delivery not confirmed".to_owned());
                    }
                    format!("due: {what}")
                }
                (Change::Done, _) => format!("done: {what}"),
                (Change::Again, Some(when)) => format!("again {when}: {what}"),
                (Change::Again, None) => format!("again: {what}"),
                (Change::Dropped, _) => format!("dropped: {what}"),
            };
            if matches!(change, Change::Done | Change::Dropped) {
                meta.extend(evidence.as_deref().map(wt_core::watch::headline));
            }
            meta.extend(named(by));
            headline
        }
        Kind::Skill { name, args, by } => {
            meta.extend(args.clone());
            // Unlike a status, who ran it is the point: the person typing
            // it and the row's own agent choosing it read differently.
            meta.extend(by.as_deref().map(|by| {
                if by == own {
                    "by agent".to_owned()
                } else {
                    format!("by {by}")
                }
            }));
            format!("/{name}")
        }
        Kind::Mark { text, by } => {
            meta.extend(named(by));
            body = Some(text.clone());
            "note".to_owned()
        }
        Kind::Restack { ok, onto } => {
            meta.extend(onto.as_ref().map(|onto| format!("onto {onto}")));
            if *ok { "restacked" } else { "restack failed" }.to_owned()
        }
        Kind::Removed { landed } => {
            meta.push(
                if *landed {
                    "after landing"
                } else {
                    "not landed"
                }
                .to_owned(),
            );
            "removed".to_owned()
        }
        Kind::Watch {
            change,
            what,
            by,
            reason,
            then,
            unconfirmed,
            id: _,
        } => {
            use wt_core::WatchChange as Change;
            // The headline and the first metadata read as one phrase:
            // "will wake the session  at 17:00", "told you  merged".
            let words = match change {
                Change::Set => Some(wt_core::watch_set_words(then, what)),
                // Folded into its watch's line everywhere a timeline is drawn.
                Change::ClearedGate | Change::Woken | Change::Notified => {
                    Some(wt_core::watch_fired_words(&[*change], *unconfirmed, what))
                }
                Change::Held | Change::Cancelled | Change::Dropped => None,
            };
            let headline = match (&words, change) {
                (Some(words), _) => words.headline.clone(),
                (None, Change::Held) => "held".to_owned(),
                (None, Change::Cancelled) => "stopped waiting".to_owned(),
                (None, _) => "gave up waiting".to_owned(),
            };
            match words {
                Some(words) => {
                    meta.push(words.about);
                    meta.extend(words.then.map(str::to_owned));
                }
                None if *change == Change::Held => meta.push(what.clone()),
                None => meta.push(format!("for {what}")),
            }
            meta.extend(named(by));
            meta.extend(reason.clone());
            headline
        }
        Kind::Unknown => return None,
    };
    meta.retain(|part| !part.trim().is_empty());
    Some(Shown {
        badge,
        headline,
        meta,
        body: body.filter(|body| !body.trim().is_empty()),
    })
}

fn entry_line(time: &str, shown: Shown) -> Line<'static> {
    let color = shown.badge.color;
    let mut spans = vec![
        Span::styled(
            format!("{:>TIME_CELLS$}  ", truncate_end(time, TIME_CELLS)),
            theme::dim(),
        ),
        Span::styled(format!("{}  ", shown.badge.glyph), theme::fg(color)),
        Span::styled(shown.headline, theme::bold(color)),
    ];
    if !shown.meta.is_empty() {
        spans.push(Span::styled(
            format!("  {}", shown.meta.join(" · ")),
            theme::dim(),
        ));
    }
    if let Some(body) = shown.body {
        spans.push(Span::styled(format!("  {body}"), theme::fg(theme::FG_MID)));
    }
    Line::from(spans)
}

/// The heading for an event with no prepared local day. Whole days elapsed,
/// not calendar days: a local date needs the timezone work that stays off
/// the render path.
fn relative_day(at_ms: u64, now_ms: u64) -> String {
    match now_ms.saturating_sub(at_ms) / 86_400_000 {
        0 => "past 24 hours".to_owned(),
        1 => "1 day ago".to_owned(),
        days @ 2..14 => format!("{days} days ago"),
        days => format!("{} weeks ago", days / 7),
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

pub(crate) struct TimelineLine {
    pub line: Line<'static>,
    /// The entry this line draws ([`Entry::first_ms`]); headings and gaps
    /// have none.
    pub entry: Option<u64>,
}

/// The timeline, newest first under day headings. Lines are as long as
/// their text; the caller cuts them to its width.
pub(crate) fn timeline(
    events: &[WorktreeEvent],
    times: &BTreeMap<u64, LocalStamp>,
    policy: &DisplayPolicy,
    own: &str,
    now_ms: u64,
) -> Vec<TimelineLine> {
    let entries = collapse(events);
    let shown: Vec<(&Entry<'_>, Shown)> = entries
        .iter()
        .rev()
        .filter_map(|entry| Some((entry, show(entry, policy, own)?)))
        .collect();
    // One unlabelled timestamp would put an age under a clock and a
    // calendar day beside an elapsed one: clocks for all, or for none.
    let clock = shown
        .iter()
        .all(|(entry, _)| times.contains_key(&entry.at_ms));
    let mut lines = Vec::new();
    let mut day: Option<String> = None;
    for (entry, shown) in shown {
        let (heading, time) = match times.get(&entry.at_ms).filter(|_| clock) {
            Some(stamp) => (stamp.day.clone(), stamp.clock.clone()),
            None => (relative_day(entry.at_ms, now_ms), age(entry.at_ms, now_ms)),
        };
        if day.as_deref() != Some(heading.as_str()) {
            if !lines.is_empty() {
                lines.push(TimelineLine {
                    line: Line::default(),
                    entry: None,
                });
            }
            lines.push(TimelineLine {
                line: Span::styled(capitalized(&heading), theme::bold(theme::FG)).into(),
                entry: None,
            });
            day = Some(heading);
        }
        lines.push(TimelineLine {
            line: entry_line(&time, shown),
            entry: Some(entry.first_ms),
        });
    }
    lines
}

/// The one line that answers "how did this row get here": when it was
/// created, where it stands and for how long, and the counts worth knowing.
/// Only the parts the events support.
pub(crate) fn summary(events: &[WorktreeEvent], now_ms: u64) -> Line<'static> {
    let ago = |at_ms: u64| age(at_ms, now_ms);
    let mut parts: Vec<(String, Color)> = Vec::new();
    // A slug's history starts at its latest creation.
    let start = events
        .iter()
        .rposition(|event| matches!(event.event, Kind::Created { .. }));
    if let Some(created) = start.map(|index| &events[index]) {
        parts.push((format!("created {} ago", ago(created.at_ms)), theme::FG_MID));
    }
    let events = &events[start.unwrap_or(0)..];
    let removed = events
        .iter()
        .rfind(|event| matches!(event.event, Kind::Removed { .. }));
    let statuses: Vec<&WorktreeEvent> = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                Kind::Status { .. } | Kind::StatusCleared { .. }
            )
        })
        .collect();
    if let Some(removed) = removed {
        parts.push((format!("removed {} ago", ago(removed.at_ms)), theme::FG_MID));
    } else if let Some(Kind::Status { to, blocked_on, .. }) =
        statuses.last().map(|event| &event.event)
    {
        // A re-assertion with another risk or gate keeps the state's clock.
        let since = statuses
            .iter()
            .rev()
            .take_while(
                |event| matches!(&event.event, Kind::Status { to: state, .. } if state == to),
            )
            .last()
            .map_or(0, |event| event.at_ms);
        parts.push((
            format!("{} for {}", to.as_str(), ago(since)),
            badges::asserted_status_badge(*to, blocked_on.is_some()).color,
        ));
    }
    if let Some(landed) = events
        .iter()
        .rfind(|event| matches!(event.event, Kind::Landed { .. }))
    {
        let verb = match landed.event {
            Kind::Landed {
                on: LandedOn::Production,
            } => "in production",
            _ => "merged",
        };
        parts.push((format!("{verb} {} ago", ago(landed.at_ms)), theme::OK));
    }
    let count =
        |matches: fn(&Kind) -> bool| events.iter().filter(|event| matches(&event.event)).count();
    let rounds = count(|event| {
        matches!(
            event,
            Kind::Review {
                change: ReviewChange::Findings { .. },
                ..
            }
        )
    });
    if rounds > 0 {
        parts.push((plural(rounds, "review round"), theme::FG_MID));
    }
    let automations = count(|event| {
        matches!(
            event,
            Kind::Automation {
                outcome: AutomationOutcomeKind::Delivered | AutomationOutcomeKind::Unconfirmed,
                ..
            }
        )
    });
    if automations > 0 {
        parts.push((plural(automations, "automation"), theme::FG_MID));
    }
    let skills = count(|event| matches!(event, Kind::Skill { .. }));
    if skills > 0 {
        parts.push((plural(skills, "skill run"), theme::FG_MID));
    }
    let mut spans = Vec::new();
    for (index, (text, color)) in parts.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", theme::dim()));
        }
        spans.push(Span::styled(text, theme::fg(color)));
    }
    Line::from(spans)
}

/// The row's marker and title, its slug when that says more, and the summary.
fn header(board: &Board, key: &str, events: &[WorktreeEvent], now_ms: u64) -> Vec<Line<'static>> {
    let title = |title: &str, slug: &str| {
        let mut spans = vec![Span::styled(
            if title.is_empty() { slug } else { title }.to_owned(),
            theme::bold(theme::FG_BRIGHT),
        )];
        if !title.is_empty() && slug != title {
            spans.push(Span::styled(format!("  {slug}"), theme::dim()));
        }
        spans
    };
    let mut spans = Vec::new();
    if let Some(row) = board.rows.iter().find(|row| row.key == key) {
        let marker = badges::marker(row, &board.display);
        spans.push(Span::styled(
            format!("{}  ", marker.glyph),
            theme::fg(marker.color),
        ));
        spans.extend(title(&row.title, &row.slug));
    } else if let Some(row) = board.removed_history.rows.iter().find(|row| row.key == key) {
        spans.extend(title(&row.title, &row.slug));
    } else {
        // The row left the board while its history was open.
        spans.extend(title("", key));
    }
    let mut lines = vec![Line::from(spans)];
    let summary = summary(events, now_ms);
    if !summary.spans.is_empty() {
        lines.push(summary);
    }
    lines
}

/// The slug a row's own agent asserts under.
fn own_slug<'a>(board: &'a Board, key: &'a str) -> &'a str {
    let live = board.rows.iter().find(|row| row.key == key);
    let removed = || board.removed_history.rows.iter().find(|row| row.key == key);
    live.map(|row| row.slug.as_str())
        .or_else(|| removed().map(|row| row.slug.as_str()))
        .unwrap_or(key)
}

impl TimelinePrompt {
    /// Fit the scroll to what is about to be drawn. Events arrive at the
    /// top, so a reader who scrolled down keeps the line they were on; one
    /// still at the top sees the new events.
    fn settle(&mut self, lines: &[TimelineLine], visible: usize) {
        if self.scroll > 0
            && let Some((entry, below, drawn_at)) = self.anchor
            // A scroll key since the last draw outranks the anchor.
            && drawn_at == self.scroll
            && let Some(index) = lines.iter().position(|line| line.entry == Some(entry))
        {
            self.scroll = index.saturating_sub(below);
        }
        self.max_scroll = lines.len().saturating_sub(visible);
        self.scroll = self.scroll.min(self.max_scroll);
        self.anchor = lines
            .iter()
            .enumerate()
            .skip(self.scroll)
            .find_map(|(index, line)| Some((line.entry?, index - self.scroll, self.scroll)));
    }
}

pub(super) fn modal(frame: &mut Frame<'_>, area: Rect, board: &Board, view: &mut TimelinePrompt) {
    let now = now_ms();
    let events = board
        .row_history
        .get(&view.key)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let head = header(board, &view.key, events, now);
    let own = own_slug(board, &view.key);
    let mut lines = timeline(events, &board.history_times, &board.display, own, now);
    if lines.is_empty() {
        lines.push(TimelineLine {
            line: Span::styled(EMPTY, theme::dim()).into(),
            entry: None,
        });
    }
    let modal = Modal::new(
        "History",
        vec![
            hint("j/k", "scroll"),
            hint("g/G", "top / bottom"),
            hint("esc / q / H", "close"),
        ],
    );
    let content = head
        .iter()
        .chain(lines.iter().map(|line| &line.line))
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .max(MIN_CONTENT_CELLS);
    // The header's rule is the extra row.
    let outer = modal.place(area, content, head.len() + 1 + lines.len(), false);
    let body = modal.draw(frame, area, outer);
    let width = body.width as usize;
    if body.height == 0 || width == 0 {
        return;
    }
    let mut head = head;
    head.push(Span::styled("─".repeat(width), theme::fg(theme::BORDER_DIM)).into());
    let pinned = body.height as usize >= head.len() + PINNED_MIN_ROWS.min(lines.len());
    let list = if pinned {
        let rows = head.len() as u16;
        let fitted: Vec<Line<'static>> = head
            .drain(..)
            .map(|line| Line::from(fit_spans(line.spans, width, Style::default())))
            .collect();
        frame.render_widget(
            Paragraph::new(fitted),
            Rect {
                height: rows,
                ..body
            },
        );
        Rect {
            y: body.y + rows,
            height: body.height - rows,
            ..body
        }
    } else {
        lines.splice(
            0..0,
            head.drain(..)
                .map(|line| TimelineLine { line, entry: None }),
        );
        body
    };
    let visible = list.height as usize;
    view.settle(&lines, visible);
    let total = lines.len();
    let rendered: Vec<Line<'static>> = lines
        .into_iter()
        .skip(view.scroll)
        .take(visible)
        .map(|line| Line::from(fit_spans(line.line.spans, width, Style::default())))
        .collect();
    frame.render_widget(Paragraph::new(rendered), list);
    scrollbar(frame, outer, modal.border, total, view.scroll, visible);
}

/// A removed row's history for its removal record, cut to `width`: the
/// summary, then the same timeline the modal draws. Empty when the row has
/// no recorded events.
pub(crate) fn record_block(board: &Board, key: &str, width: usize) -> Vec<Line<'static>> {
    let now = now_ms();
    let Some(events) = board.row_history.get(key) else {
        return Vec::new();
    };
    let own = own_slug(board, key);
    let lines = timeline(events, &board.history_times, &board.display, own, now);
    if lines.is_empty() {
        return Vec::new();
    }
    std::iter::once(summary(events, now))
        .chain(std::iter::once(Line::default()))
        .chain(lines.into_iter().map(|line| line.line))
        .map(|line| Line::from(fit_spans(line.spans, width, Style::default())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoardRow, Interaction, Model, glyphs, model::InputResult};
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
    use unicode_width::UnicodeWidthStr;
    use wt_core::WorkRisk;

    const MINUTE: u64 = 60_000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    const NOW: u64 = 1_800_000_000_000;

    fn at(at_ms: u64, event: Kind) -> WorktreeEvent {
        WorktreeEvent { at_ms, event }
    }

    fn status(to: WorkState) -> Kind {
        Kind::Status {
            from: None,
            to,
            by: None,
            risk: None,
            note: None,
            blocked_on: None,
            verify_in: None,
        }
    }

    fn findings(reviewer: &str, count: u32) -> Kind {
        Kind::Review {
            reviewer: Some(reviewer.into()),
            change: ReviewChange::Findings { count },
        }
    }

    fn fired(rule: &str, outcome: AutomationOutcomeKind) -> Kind {
        Kind::Automation {
            rule: rule.into(),
            outcome,
            reason: None,
        }
    }

    /// A branch's whole life, oldest first, ending `now`.
    fn story(now: u64) -> Vec<WorktreeEvent> {
        vec![
            at(
                now - 3 * DAY - 2 * HOUR,
                Kind::Created {
                    base: Some("main".into()),
                },
            ),
            at(now - 3 * DAY - HOUR, status(WorkState::Working)),
            at(
                now - 2 * DAY - 5 * HOUR,
                Kind::Pr {
                    number: 12,
                    change: PrChange::OpenedDraft,
                },
            ),
            at(
                now - 2 * DAY - 4 * HOUR,
                Kind::Checks {
                    state: ChecksChange::Failing,
                    failed: vec!["lint".into(), "unit".into()],
                },
            ),
            at(
                now - 2 * DAY - 3 * HOUR,
                Kind::Checks {
                    state: ChecksChange::Passing,
                    failed: Vec::new(),
                },
            ),
            at(now - 2 * DAY - 2 * HOUR, findings("coderabbit", 5)),
            at(now - 2 * DAY - 2 * HOUR + 1, Kind::Unknown),
            at(now - 2 * DAY - HOUR, findings("coderabbit", 2)),
            at(
                now - DAY - 3 * HOUR,
                Kind::Review {
                    reviewer: Some("coderabbit".into()),
                    change: ReviewChange::Clean,
                },
            ),
            at(
                now - DAY - 2 * HOUR,
                fired("ping-ready", AutomationOutcomeKind::Delivered),
            ),
            at(
                now - DAY - HOUR,
                Kind::Message {
                    from: Some("manager".into()),
                    text: "Rebase onto main before merging".into(),
                    delivered: Some(true),
                },
            ),
            at(
                now - DAY - MINUTE,
                Kind::Message {
                    from: None,
                    text: "Any update?".into(),
                    delivered: None,
                },
            ),
            at(now - 5 * HOUR, status(WorkState::Review)),
            at(now - 5 * HOUR + 10 * MINUTE, status(WorkState::Ready)),
            at(now - 5 * HOUR + 20 * MINUTE, status(WorkState::Review)),
            at(now - 5 * HOUR + 30 * MINUTE, status(WorkState::Ready)),
            at(
                now - 3 * HOUR,
                Kind::Status {
                    from: Some(WorkState::Ready),
                    to: WorkState::Ready,
                    by: Some("fix-login".into()),
                    risk: Some(WorkRisk::Low),
                    note: Some("tests pass".into()),
                    blocked_on: Some("api deploy".into()),
                    verify_in: None,
                },
            ),
            at(now - 2 * HOUR, Kind::Landed { on: LandedOn::Base }),
            at(
                now - HOUR,
                Kind::Landed {
                    on: LandedOn::Production,
                },
            ),
            at(now - 10 * MINUTE, Kind::Removed { landed: true }),
        ]
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn texts(events: &[WorktreeEvent], times: &BTreeMap<u64, LocalStamp>) -> Vec<String> {
        timeline(events, times, &DisplayPolicy::default(), "fix-login", NOW)
            .iter()
            .map(|line| text(&line.line))
            .collect()
    }

    fn board(events: Vec<WorktreeEvent>) -> Board {
        Board {
            rows: vec![BoardRow {
                key: "one".into(),
                slug: "fix-login".into(),
                title: "Fix the login redirect".into(),
                ..Default::default()
            }],
            row_history: [("one".to_owned(), events)].into(),
            ..Default::default()
        }
    }

    fn open(board: Board) -> Model {
        Model {
            board: board.into(),
            interaction: Interaction::Timeline(TimelinePrompt {
                key: "one".into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn view(model: &mut Model) -> &mut TimelinePrompt {
        match &mut model.interaction {
            Interaction::Timeline(view) => view,
            _ => panic!("the history modal is not open"),
        }
    }

    fn draw(model: &mut Model, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| super::super::overlays::render(frame, model, frame.area()))
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
            row.find(needle)
                .map(|byte| (row[..byte].width() as u16, y as u16))
        })
    }

    /// The modal's rows between its borders, trimmed.
    fn body_rows(buffer: &Buffer) -> Vec<String> {
        rows(buffer)
            .into_iter()
            .filter_map(|row| {
                let start = row.find('│')? + '│'.len_utf8();
                let end = row.rfind(['│', '┃'])?;
                (end > start).then(|| row[start..end].trim().to_owned())
            })
            .collect()
    }

    /// The timeline row drawn directly under the pinned header's rule.
    fn top_entry(buffer: &Buffer) -> String {
        let body = body_rows(buffer);
        let rule = body.iter().position(|row| row.starts_with("───")).unwrap();
        body[rule + 1].clone()
    }

    #[test]
    fn a_status_flapping_between_two_states_is_one_entry() {
        let events = [
            at(1, status(WorkState::Working)),
            at(2, status(WorkState::Review)),
            at(3, status(WorkState::Ready)),
            at(4, status(WorkState::Review)),
            at(5, status(WorkState::Ready)),
        ];
        let entries = collapse(&events);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].what, What::Event(&events[0].event));
        assert_eq!(
            entries[1],
            Entry {
                at_ms: 5,
                first_ms: 2,
                prev: Some(&events[0].event),
                what: What::Flap {
                    now: WorkState::Ready,
                    other: WorkState::Review,
                    changes: 4,
                },
            }
        );
        // Two changes are a sequence, not a flap.
        assert_eq!(collapse(&events[..3]).len(), 3);
    }

    #[test]
    fn a_gate_or_another_event_ends_a_flap() {
        let gated = Kind::Status {
            from: None,
            to: WorkState::Ready,
            by: None,
            risk: None,
            note: None,
            blocked_on: Some("deploy".into()),
            verify_in: None,
        };
        let events = [
            at(1, status(WorkState::Review)),
            at(2, status(WorkState::Ready)),
            at(3, status(WorkState::Review)),
            at(4, gated.clone()),
            at(5, status(WorkState::Review)),
            at(6, Kind::Conflict { resolved: false }),
            at(7, status(WorkState::Ready)),
            at(8, status(WorkState::Review)),
        ];
        let entries = collapse(&events);
        let whats: Vec<&What<'_>> = entries.iter().map(|entry| &entry.what).collect();
        assert_eq!(
            whats,
            [
                &What::Flap {
                    now: WorkState::Review,
                    other: WorkState::Ready,
                    changes: 3,
                },
                &What::Event(&gated),
                &What::Event(&events[4].event),
                &What::Event(&events[5].event),
                &What::Event(&events[6].event),
                &What::Event(&events[7].event),
            ]
        );
    }

    #[test]
    fn review_rounds_fold_per_reviewer_and_keep_the_latest_count() {
        let events = [
            at(1, findings("bot", 5)),
            at(2, Kind::Unknown),
            at(3, findings("bot", 2)),
            at(4, findings("other", 1)),
        ];
        let entries = collapse(&events);
        assert_eq!(entries.len(), 2, "an unknown kind is dropped, not a break");
        assert_eq!(
            entries[0],
            Entry {
                at_ms: 3,
                first_ms: 1,
                prev: None,
                what: What::Rounds {
                    reviewer: Some("bot"),
                    count: 2,
                    rounds: 2,
                },
            }
        );
        assert_eq!(entries[1].what, What::Event(&events[3].event));
    }

    #[test]
    fn identical_automation_fires_fold_only_within_a_short_span() {
        let ping = || fired("ping", AutomationOutcomeKind::Delivered);
        let events = [
            at(0, ping()),
            at(5 * MINUTE, ping()),
            at(14 * MINUTE, ping()),
            at(14 * MINUTE + BURST_MS + 1, ping()),
            at(40 * MINUTE, fired("ping", AutomationOutcomeKind::Failed)),
        ];
        let entries = collapse(&events);
        assert_eq!(
            entries.iter().map(|entry| &entry.what).collect::<Vec<_>>(),
            [
                &What::Burst {
                    event: &events[0].event,
                    times: 3,
                },
                &What::Event(&events[3].event),
                &What::Event(&events[4].event),
            ]
        );
        assert_eq!((entries[0].first_ms, entries[0].at_ms), (0, 14 * MINUTE));
    }

    fn watch(change: wt_core::WatchChange, what: &str) -> Kind {
        Kind::Watch {
            id: Some("aaaaaa".into()),
            change,
            what: what.into(),
            by: None,
            reason: None,
            then: Vec::new(),
            unconfirmed: false,
        }
    }

    fn watch_set(what: &str, by: Option<&str>, then: &[&str]) -> Kind {
        Kind::Watch {
            id: Some("aaaaaa".into()),
            change: wt_core::WatchChange::Set,
            what: what.into(),
            by: by.map(str::to_owned),
            reason: None,
            then: then.iter().map(|action| (*action).to_owned()).collect(),
            unconfirmed: false,
        }
    }

    /// Oldest first, each line from its headline on.
    fn said(events: &[WorktreeEvent]) -> Vec<String> {
        texts(events, &BTreeMap::new())
            .iter()
            .skip(1)
            .rev()
            .map(|line| {
                let (_time, rest) = line.trim_start().split_once("  ").unwrap();
                rest.split_once("  ").unwrap().1.to_owned()
            })
            .collect()
    }

    #[test]
    fn one_watch_is_two_lines_when_it_was_set_and_what_wt_did() {
        use wt_core::WatchChange as Change;
        // A wake at a time, as earlier builds recorded it with the prompt's
        // own message event.
        let wake = [
            at(0, watch_set("23:08", Some("fix-login"), &["wake"])),
            at(HOUR, watch(Change::Held, "23:08")),
            at(
                HOUR + 1,
                Kind::Message {
                    from: Some("wt".into()),
                    text: "[wt] What you were waiting for has happened: 23:08.".into(),
                    delivered: Some(true),
                },
            ),
            at(HOUR + 2, watch(Change::Woken, "23:08")),
        ];
        assert_eq!(
            said(&wake),
            [
                "will wake the session  at 23:08",
                "woke the session  at 23:08"
            ]
        );
        let entries = collapse(&wake);
        assert_eq!(
            (entries[1].first_ms, entries[1].at_ms),
            (HOUR + 2, HOUR + 2)
        );

        // A gate that clears and wakes: the status that drops the gate is
        // wt clearing it, and what stands afterwards has no gate to clear.
        let gate = [
            at(0, asserted(WorkState::Ready, None, Some("the flag"), None)),
            at(
                1,
                watch_set("check flag=go", Some("manager"), &["unblock", "wake"]),
            ),
            at(HOUR, watch(Change::Held, "check flag=go")),
            at(HOUR + 1, asserted(WorkState::Ready, None, None, None)),
            at(HOUR + 2, watch(Change::ClearedGate, "check flag=go")),
            at(HOUR + 3, watch(Change::Woken, "check flag=go")),
            at(2 * HOUR, asserted(WorkState::Review, None, None, None)),
        ];
        assert_eq!(
            said(&gate),
            [
                "ready  blocked on the flag",
                "gate clears  when check flag=go · then wakes the session · by manager",
                "cleared the gate and woke the session  check flag=go held",
                "review",
            ]
        );

        // Told as soon as it was set: one line.
        let instant = [
            at(0, watch_set("merged", None, &["notify"])),
            at(1_000, watch(Change::Held, "merged")),
            at(2_000, watch(Change::Notified, "merged")),
        ];
        assert_eq!(said(&instant), ["told you  merged · as soon as it was set"]);
        // Held with nothing done yet says so.
        assert_eq!(
            said(&instant[..2]),
            ["will tell you  when merged", "held  merged"]
        );
    }

    #[test]
    fn what_a_row_owes_is_one_line_a_change_with_the_evidence_dim() {
        use wt_core::{OwedChange, WatchChange};
        let owed = |change, id: Option<&str>, stage: Option<&str>| Kind::Owed {
            id: id.map(str::to_owned),
            change,
            what: "confirm prod logs\nthen open the PR".into(),
            evidence: None,
            by: Some("fix-login".into()),
            stage: stage.map(str::to_owned),
            when: None,
            unconfirmed: false,
        };
        let line = |event: Kind| {
            let shown = show_event(&event, None, &DisplayPolicy::default(), "fix-login").unwrap();
            (
                format!("{}  {}", shown.headline, shown.meta.join(" · ")),
                (shown.badge.glyph, shown.badge.color),
                shown.body,
            )
        };
        assert_eq!(
            line(owed(OwedChange::Due, Some("93co6q"), None)),
            (
                "due: confirm prod logs  wt done 93co6q".to_owned(),
                (crate::glyphs::DOT, theme::WARN),
                None
            )
        );
        let mut unsure = owed(OwedChange::Due, None, Some("staging"));
        if let Kind::Owed { unconfirmed, .. } = &mut unsure {
            *unconfirmed = true;
        }
        assert_eq!(
            line(unsure).0,
            "due: staging check: confirm prod logs  wt done staging · wake sent, delivery not confirmed"
        );
        // Done: the evidence is metadata, dim beside the headline.
        let mut done = owed(OwedChange::Done, Some("93co6q"), None);
        if let Kind::Owed { evidence, by, .. } = &mut done {
            *evidence = Some("logs clean for 1h".into());
            *by = Some("manager".into());
        }
        assert_eq!(
            line(done.clone()),
            (
                "done: confirm prod logs  logs clean for 1h · by manager".to_owned(),
                (crate::glyphs::TASK_COMPLETE, theme::OK),
                None
            )
        );
        let drawn = entry_line(
            "now",
            show_event(&done, None, &DisplayPolicy::default(), "fix-login").unwrap(),
        );
        let evidence = drawn
            .spans
            .iter()
            .find(|span| span.content.contains("logs clean"))
            .unwrap();
        assert_eq!(evidence.style.fg, Some(theme::FG_DIM));
        let mut again = owed(OwedChange::Again, Some("93co6q"), None);
        if let Kind::Owed { when, .. } = &mut again {
            *when = Some("at 17:00".into());
        }
        assert_eq!(
            line(again),
            (
                "again at 17:00: confirm prod logs  ".to_owned(),
                (crate::glyphs::DOT_OUTLINE, badges::WAITING),
                None
            )
        );
        assert_eq!(
            line(owed(OwedChange::Dropped, None, Some("production"))),
            (
                "dropped: production check: confirm prod logs  ".to_owned(),
                (crate::glyphs::TASK_CANCELLED, theme::FG_DIM),
                None
            )
        );
        // A wake delivered after landing: "due" says that it held.
        let held = |id: &str| Kind::Watch {
            id: Some(id.into()),
            change: WatchChange::Held,
            what: "in production".into(),
            by: None,
            reason: None,
            then: Vec::new(),
            unconfirmed: false,
        };
        let events = [
            at(0, held("93co6q")),
            at(1, owed(OwedChange::Due, Some("93co6q"), None)),
            at(2, held("other1")),
        ];
        assert_eq!(
            said(&events),
            [
                "due: confirm prod logs  wt done 93co6q",
                "held  in production"
            ]
        );
    }

    #[test]
    fn a_watch_line_wears_the_color_of_what_became_of_it() {
        use wt_core::WatchChange as Change;
        let line = |event: Kind| {
            let shown = show_event(&event, None, &DisplayPolicy::default(), "fix-login").unwrap();
            let badge = shown.badge;
            (
                format!("{}  {}", shown.headline, shown.meta.join(" · ")),
                badge,
            )
        };
        let (text, badge) = line(watch_set(
            "#3806 in production",
            Some("manager"),
            &["unblock", "wake"],
        ));
        assert_eq!(
            text,
            "gate clears  when #3806 in production · then wakes the session · by manager"
        );
        assert_eq!(
            (badge.glyph, badge.color),
            (crate::glyphs::WATCH, badges::WAITING)
        );
        // The row's own agent is not named, as on a status line.
        assert_eq!(
            line(watch_set("17:00", Some("fix-login"), &["wake"])).0,
            "will wake the session  at 17:00"
        );
        for (change, expected, color) in [
            (Change::Held, "held  merged", theme::OK),
            (Change::ClearedGate, "cleared the gate  merged", theme::OK),
            (Change::Woken, "woke the session  merged", theme::OK),
            (Change::Notified, "told you  merged", theme::OK),
            (
                Change::Cancelled,
                "stopped waiting  for merged",
                theme::FG_DIM,
            ),
        ] {
            let (text, badge) = line(watch(change, "merged"));
            assert_eq!(text, expected);
            assert_eq!(badge.color, color);
        }
        let dropped = Kind::Watch {
            id: None,
            change: Change::Dropped,
            what: "merged".into(),
            by: None,
            reason: Some("its worktree was removed".into()),
            then: Vec::new(),
            unconfirmed: false,
        };
        let (text, badge) = line(dropped);
        assert_eq!(
            text,
            "gave up waiting  for merged · its worktree was removed"
        );
        assert_eq!(badge.color, theme::WARN);

        // A wake nothing proved delivered warns, folded or not.
        let mut unsure = watch(Change::Woken, "23:08");
        if let Kind::Watch { unconfirmed, .. } = &mut unsure {
            *unconfirmed = true;
        }
        let (text, badge) = line(unsure.clone());
        assert_eq!(text, "wake sent, delivery not confirmed  at 23:08");
        assert_eq!(badge.color, theme::WARN);
        let events = [at(0, unsure)];
        let entries = collapse(&events);
        let shown = show(&entries[0], &DisplayPolicy::default(), "fix-login").unwrap();
        assert_eq!(shown.headline, "wake sent, delivery not confirmed");
        assert_eq!(shown.badge.color, theme::WARN);
    }

    #[test]
    fn checks_that_fail_and_recover_stay_as_they_happened() {
        let checks = |state| Kind::Checks {
            state,
            failed: Vec::new(),
        };
        let events = [
            at(1, checks(ChecksChange::Passing)),
            at(2, checks(ChecksChange::Failing)),
            at(3, checks(ChecksChange::Passing)),
        ];
        assert_eq!(collapse(&events).len(), 3);
        assert!(collapse(&[at(1, Kind::Unknown)]).is_empty());
    }

    #[test]
    fn a_history_reads_newest_first_one_aligned_line_per_event() {
        let lines = texts(&story(NOW), &BTreeMap::new());
        let expected = [
            "Past 24 hours".to_owned(),
            format!("  10m  {}  removed  after landing", glyphs::TRASH),
            format!("   1h  {}  in production", glyphs::PRODUCTION),
            format!("   2h  {}  merged", glyphs::MERGE),
            format!(
                "   3h  {}  ready  blocked on api deploy · low risk  tests pass",
                glyphs::SLASH
            ),
            format!("   4h  {}  ready ↔ review  ×4", glyphs::DOT),
            String::new(),
            "1 day ago".to_owned(),
            format!(
                "   1d  {}  message  unconfirmed  \"Any update?\"",
                glyphs::SEND
            ),
            format!(
                "   1d  {}  message  from manager · delivered  \"Rebase onto main before merging\"",
                glyphs::SEND
            ),
            format!(
                "   1d  {}  automation delivered  ping-ready",
                glyphs::AUTOMATION
            ),
            format!(
                "   1d  {}  review clean  coderabbit",
                glyphs::REVIEW_CHECKLIST
            ),
            String::new(),
            "2 days ago".to_owned(),
            format!(
                "   2d  {}  review: 2 findings  2 rounds · coderabbit",
                glyphs::REVIEW_CHECKLIST
            ),
            format!("   2d  {}  checks passing", glyphs::CHECK_PASS),
            format!("   2d  {}  checks failing  lint, unit", glyphs::CHECK_FAIL),
            format!("   2d  {}  PR opened as draft  #12", glyphs::PR_DRAFT),
            String::new(),
            "3 days ago".to_owned(),
            format!("   3d  {}  working", glyphs::DOT),
            format!("   3d  {}  created  from main", glyphs::BRANCH),
        ];
        assert_eq!(lines, expected);
    }

    #[test]
    fn headline_takes_the_glyph_color_metadata_is_dim_and_text_is_body_gray() {
        let lines = timeline(
            &story(NOW),
            &BTreeMap::new(),
            &DisplayPolicy::default(),
            "fix-login",
            NOW,
        );
        let line = |needle: &str| {
            &lines
                .iter()
                .find(|line| text(&line.line).contains(needle))
                .unwrap()
                .line
        };
        let gated = line("blocked on api deploy");
        assert_eq!(gated.spans[0].style.fg, Some(theme::FG_DIM), "time");
        assert_eq!(gated.spans[1].style.fg, Some(theme::WARN), "glyph");
        assert_eq!(gated.spans[2].style.fg, Some(theme::WARN), "headline");
        assert_eq!(gated.spans[3].style.fg, Some(theme::FG_DIM), "metadata");
        assert_eq!(gated.spans[4].style.fg, Some(theme::FG_MID), "note");
        let failing = line("checks failing");
        assert_eq!(failing.spans[1].style.fg, Some(theme::ERR));
        assert_eq!(failing.spans[2].style.fg, Some(theme::ERR));
        let unconfirmed = line("Any update?");
        assert_eq!(unconfirmed.spans[1].style.fg, Some(theme::WARN));
        assert_eq!(unconfirmed.spans[4].content, "  \"Any update?\"");
        assert_eq!(unconfirmed.spans[4].style.fg, Some(theme::FG_MID));
        assert_eq!(line("Rebase onto").spans[1].style.fg, Some(theme::ACCENT));
        assert_eq!(line("ping-ready").spans[1].style.fg, Some(theme::OK));
        // Every entry starts its text in the same column.
        for entry in lines.iter().filter(|line| line.entry.is_some()) {
            assert_eq!(entry.line.spans[0].content.width(), TIME_CELLS + 2);
            assert_eq!(entry.line.spans[1].content.width(), 3);
        }
    }

    #[test]
    fn a_skill_use_leads_with_its_command_and_says_when_an_agent_ran_it() {
        let skill = |name: &str, args: Option<&str>, by: Option<&str>| Kind::Skill {
            name: name.into(),
            args: args.map(str::to_owned),
            by: by.map(str::to_owned),
        };
        let events = [
            at(NOW - 5 * MINUTE, Kind::Created { base: None }),
            at(NOW - 4 * MINUTE, skill("fable-review", None, None)),
            at(
                NOW - 3 * MINUTE,
                skill("check", Some("src/foo"), Some("fix-login")),
            ),
            at(NOW - 2 * MINUTE, skill("check", None, Some("manager"))),
            at(
                NOW - MINUTE,
                Kind::Mark {
                    text: "handed to QA".into(),
                    by: Some("fix-login".into()),
                },
            ),
        ];
        assert_eq!(
            said(&events)[1..],
            [
                "/fable-review",
                "/check  src/foo · by agent",
                "/check  by manager",
                "note  handed to QA",
            ]
        );
        let lines = texts(&events, &BTreeMap::new());
        assert_eq!(
            lines[1..3],
            [
                format!("   1m  {}  note  handed to QA", glyphs::NOTE),
                format!("   2m  {}  /check  by manager", glyphs::SKILL),
            ]
        );
        let drawn = timeline(
            &events,
            &BTreeMap::new(),
            &DisplayPolicy::default(),
            "fix-login",
            NOW,
        );
        let ran = &drawn[3].line;
        assert_eq!(ran.spans[2].content, "/check");
        assert_eq!(ran.spans[2].style, theme::bold(theme::FG_BRIGHT));
        assert_eq!(ran.spans[3].style.fg, Some(theme::FG_DIM), "arguments");
        let note = &drawn[1].line;
        assert_eq!(note.spans[1].style.fg, Some(theme::FG_MID), "glyph");
        assert_eq!(note.spans[3].content, "  handed to QA");
        assert_eq!(
            text(&summary(&events, NOW)),
            "created 5m ago · 3 skill runs"
        );
    }

    #[test]
    fn a_status_wears_the_dot_the_board_draws_for_that_state() {
        for state in [
            WorkState::Todo,
            WorkState::Working,
            WorkState::Review,
            WorkState::NeedsTesting,
            WorkState::NeedsHuman,
            WorkState::Ready,
            WorkState::Verified,
            WorkState::Dropped,
        ] {
            let lines = timeline(
                &[at(NOW, status(state))],
                &BTreeMap::new(),
                &DisplayPolicy::default(),
                "fix-login",
                NOW,
            );
            let line = &lines[1].line;
            assert_eq!(
                line.spans[1].content,
                format!("{}  ", badges::work_state_glyph(state)),
                "{state:?}"
            );
            assert_eq!(
                line.spans[1].style.fg,
                Some(badges::work_state_color(state))
            );
            assert_eq!(
                line.spans[2].style.fg,
                Some(badges::work_state_color(state))
            );
            assert_eq!(line.spans[2].content, state.as_str());
        }
    }

    fn asserted(to: WorkState, by: Option<&str>, gate: Option<&str>, verify: Option<&str>) -> Kind {
        Kind::Status {
            from: None,
            to,
            by: by.map(Into::into),
            risk: None,
            note: None,
            blocked_on: gate.map(Into::into),
            verify_in: verify.map(Into::into),
        }
    }

    #[test]
    fn a_rows_own_agent_is_not_named_but_anyone_else_is() {
        let events = [
            at(
                NOW - 5 * MINUTE,
                asserted(WorkState::Todo, Some("manager"), None, None),
            ),
            at(
                NOW - 4 * MINUTE,
                asserted(WorkState::Working, Some("fix-login"), None, None),
            ),
            at(
                NOW - 3 * MINUTE,
                asserted(WorkState::Review, Some("other-row"), None, None),
            ),
            at(
                NOW - 2 * MINUTE,
                asserted(WorkState::Ready, None, None, None),
            ),
            at(
                NOW - MINUTE,
                Kind::StatusCleared {
                    from: Some(WorkState::Ready),
                    by: Some("fix-login".into()),
                },
            ),
        ];
        let lines = texts(&events, &BTreeMap::new());
        let line = |needle: &str| lines.iter().find(|line| line.contains(needle)).unwrap();
        assert!(line("todo").ends_with("todo  by manager"), "{lines:?}");
        assert!(line("working").ends_with("working"), "{lines:?}");
        assert!(
            line("review").ends_with("review  by other-row"),
            "{lines:?}"
        );
        // No name is not proof of a person, so it claims no one.
        assert!(line("  ready").ends_with("ready"), "{lines:?}");
        assert!(line("status cleared").ends_with("status cleared  was ready"));
        // Another row's history names this slug like any other.
        let elsewhere = timeline(
            &events,
            &BTreeMap::new(),
            &DisplayPolicy::default(),
            "other-row",
            NOW,
        );
        let elsewhere: Vec<String> = elsewhere.iter().map(|line| text(&line.line)).collect();
        assert!(
            elsewhere
                .iter()
                .any(|line| line.ends_with("working  by fix-login"))
        );
        assert!(elsewhere.iter().any(|line| line.ends_with("review")));
    }

    #[test]
    fn a_gate_and_an_owed_check_are_said_once_when_they_change() {
        let gate = Some("API deploy 4.2");
        let staging = Some("staging");
        let events = [
            at(
                NOW - 9 * MINUTE,
                asserted(WorkState::Working, None, None, None),
            ),
            at(
                NOW - 8 * MINUTE,
                asserted(WorkState::Ready, None, gate, staging),
            ),
            at(NOW - 7 * MINUTE, Kind::Conflict { resolved: false }),
            at(
                NOW - 6 * MINUTE,
                asserted(WorkState::NeedsHuman, None, gate, staging),
            ),
            at(
                NOW - 5 * MINUTE,
                asserted(WorkState::Ready, None, Some("QA sign-off"), staging),
            ),
            at(
                NOW - 4 * MINUTE,
                asserted(WorkState::Ready, None, None, Some("production")),
            ),
            at(
                NOW - 3 * MINUTE,
                asserted(WorkState::Review, None, None, Some("production")),
            ),
            at(
                NOW - 2 * MINUTE,
                Kind::StatusCleared {
                    from: None,
                    by: None,
                },
            ),
            at(
                NOW - MINUTE,
                asserted(WorkState::Ready, None, None, Some("production")),
            ),
        ];
        // Oldest first, each line from its headline on.
        let lines: Vec<String> = texts(&events, &BTreeMap::new())
            .iter()
            .skip(1)
            .rev()
            .map(|line| {
                let (_time, rest) = line.trim_start().split_once("  ").unwrap();
                rest.split_once("  ").unwrap().1.to_owned()
            })
            .collect();
        assert_eq!(
            lines,
            [
                "working",
                "ready  blocked on API deploy 4.2 · verify in staging",
                "conflicts with its base",
                // Compared with the status before it, not the line above.
                "needs-human",
                "ready  blocked on QA sign-off",
                "ready  gate cleared · verify in production",
                "review",
                "status cleared",
                // A cleared status carried nothing forward.
                "ready  verify in production",
            ]
        );
    }

    #[test]
    fn a_flap_under_a_standing_gate_still_folds_and_stays_gated() {
        let gate = Some("deploy");
        let events = [
            at(1, asserted(WorkState::Ready, None, gate, None)),
            at(2, asserted(WorkState::Review, None, gate, None)),
            at(3, asserted(WorkState::Ready, None, gate, None)),
            at(4, asserted(WorkState::Review, None, gate, None)),
        ];
        let entries = collapse(&events);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[1].what,
            What::Flap {
                now: WorkState::Review,
                other: WorkState::Ready,
                changes: 3,
            }
        );
        let shown = show(&entries[1], &DisplayPolicy::default(), "one").unwrap();
        assert_eq!(
            shown.badge,
            badges::asserted_status_badge(WorkState::Review, true)
        );
    }

    #[test]
    fn prepared_local_times_give_calendar_days_and_clocks() {
        let events = [
            at(NOW - 2 * DAY, status(WorkState::Working)),
            at(NOW - DAY, status(WorkState::Review)),
            at(NOW - HOUR, status(WorkState::Ready)),
        ];
        let stamp = |day: &str, clock: &str| LocalStamp {
            day: day.into(),
            clock: clock.into(),
        };
        let mut times: BTreeMap<u64, LocalStamp> = [
            (NOW - 2 * DAY, stamp("Sat 21 Nov", "09:05")),
            (NOW - DAY, stamp("yesterday", "23:40")),
            (NOW - HOUR, stamp("today", "14:32")),
        ]
        .into();
        assert_eq!(
            texts(&events, &times),
            [
                "Today".to_owned(),
                format!("14:32  {}  ready", glyphs::DOT),
                String::new(),
                "Yesterday".to_owned(),
                format!("23:40  {}  review", glyphs::DOT),
                String::new(),
                "Sat 21 Nov".to_owned(),
                format!("09:05  {}  working", glyphs::DOT),
            ]
        );
        // One event without a label: ages throughout, never a mix.
        times.remove(&(NOW - DAY));
        let relative = texts(&events, &times);
        assert_eq!(relative[0], "Past 24 hours");
        assert_eq!(relative[1], format!("   1h  {}  ready", glyphs::DOT));
        assert!(
            relative.iter().all(|line| !line.contains(':')),
            "{relative:?}"
        );
    }

    #[test]
    fn the_summary_says_where_the_row_stands_and_only_what_exists() {
        assert!(summary(&[], NOW).spans.is_empty());
        let mut ready = status(WorkState::Ready);
        let mut events = vec![
            at(NOW - 9 * DAY, Kind::Removed { landed: false }),
            at(NOW - 3 * DAY - HOUR, Kind::Created { base: None }),
            at(NOW - DAY, findings("bot", 3)),
            at(NOW - 5 * HOUR, status(WorkState::Review)),
            at(NOW - 4 * HOUR, findings("bot", 1)),
            at(NOW - 2 * HOUR - MINUTE, ready.clone()),
        ];
        if let Kind::Status { risk, .. } = &mut ready {
            *risk = Some(WorkRisk::Low);
        }
        // The same state asserted again keeps its clock.
        events.push(at(NOW - HOUR, ready));
        let line = summary(&events, NOW);
        assert_eq!(
            text(&line),
            "created 3d ago · ready for 2h · 2 review rounds"
        );
        let state = line
            .spans
            .iter()
            .find(|span| span.content.starts_with("ready"))
            .unwrap();
        assert_eq!(
            state.style.fg,
            Some(badges::work_state_color(WorkState::Ready))
        );
        assert_eq!(
            text(&summary(&story(NOW), NOW)),
            "created 3d ago · removed 10m ago · in production 1h ago · 2 review rounds · 1 automation"
        );
    }

    #[test]
    fn the_modal_draws_a_header_a_rule_and_the_timeline() {
        let now = now_ms();
        let mut model = open(board(story(now)));
        let buffer = draw(&mut model, 100, 44);
        let drawn = rows(&buffer).join("\n");
        println!("{drawn}");
        for needle in [
            " History ",
            "Fix the login redirect  fix-login",
            "created 3d ago · removed 10m ago · in production 1h ago",
            "Past 24 hours",
            "ready ↔ review  ×4",
            "review: 2 findings  2 rounds · coderabbit",
            "created  from main",
            "j/k scroll · g/G top / bottom · esc / q / H close",
        ] {
            assert!(drawn.contains(needle), "missing {needle:?}\n{drawn}");
        }
        let body = body_rows(&buffer);
        let title = body
            .iter()
            .position(|row| row.contains("Fix the login redirect"))
            .unwrap();
        assert!(body[title + 1].starts_with("created 3d ago"));
        assert!(body[title + 2].chars().all(|ch| ch == '─'), "{body:?}");
        assert_eq!(body[title + 3], "Past 24 hours");
        let (x, y) = find(&buffer, "checks failing").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::ERR);
        assert_eq!(buffer[(x - 3, y)].symbol(), glyphs::CHECK_FAIL);
        assert_eq!(buffer[(x - 3, y)].fg, theme::ERR);
        let (x, y) = find(&buffer, "lint, unit").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::FG_DIM);
        let (x, y) = find(&buffer, "fix-login").unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::FG_DIM, "the slug recedes");

        // With local labels prepared for every event, the same modal reads
        // in calendar days and clock times.
        let mut labelled = board(story(now));
        labelled.history_times = story(now)
            .iter()
            .map(|event| {
                let minutes = event.at_ms / MINUTE % (24 * 60);
                let stamp = LocalStamp {
                    day: match (now - event.at_ms) / DAY {
                        0 => "today".into(),
                        1 => "yesterday".into(),
                        days => format!("{days} Nov"),
                    },
                    clock: format!("{:02}:{:02}", minutes / 60, minutes % 60),
                };
                (event.at_ms, stamp)
            })
            .collect();
        let mut model = open(labelled);
        let buffer = draw(&mut model, 100, 44);
        println!("{}", rows(&buffer).join("\n"));
        let body = body_rows(&buffer);
        for heading in ["Today", "Yesterday", "2 Nov", "3 Nov"] {
            assert!(body.iter().any(|row| row == heading), "{heading}: {body:?}");
        }
        assert!(body.iter().all(|row| !row.contains("days ago")));
    }

    #[test]
    fn a_row_without_events_says_when_recording_starts() {
        let mut model = open(board(Vec::new()));
        model.board = Board {
            row_history: Default::default(),
            ..(*model.board).clone()
        }
        .into();
        let buffer = draw(&mut model, 120, 30);
        let (x, y) = find(&buffer, EMPTY).unwrap();
        assert_eq!(buffer[(x, y)].fg, theme::FG_DIM);
        assert!(find(&buffer, "Fix the login redirect").is_some());
        // Only kinds this build cannot read, or a row that left the board:
        // the same sentence, never a panic.
        let mut unknown = open(board(vec![at(1, Kind::Unknown)]));
        assert!(find(&draw(&mut unknown, 120, 30), EMPTY).is_some());
        let mut gone = open(Board::default());
        let buffer = draw(&mut gone, 120, 30);
        assert!(find(&buffer, EMPTY).is_some());
        assert!(find(&buffer, "one").is_some());
    }

    #[test]
    fn long_lines_are_cut_with_an_ellipsis_at_every_width() {
        let now = now_ms();
        let mut events = story(now);
        events.push(at(
            now - MINUTE,
            Kind::Message {
                from: Some("manager".into()),
                text: "word ".repeat(60),
                delivered: Some(true),
            },
        ));
        for (width, height) in [(60, 30), (80, 30), (120, 40), (200, 60), (80, 12), (40, 8)] {
            let mut model = open(board(events.clone()));
            let buffer = draw(&mut model, width, height);
            let drawn = rows(&buffer);
            let corner = |symbol: &str| {
                buffer
                    .area
                    .positions()
                    .find(|position| buffer[*position].symbol() == symbol)
                    .unwrap_or_else(|| panic!("no {symbol} at {width}x{height}"))
            };
            let (top, bottom) = (corner("╭"), corner("╯"));
            let frame = bottom.x - top.x + 1;
            assert!(frame <= 104 && frame + 4 <= width, "{width}: {frame}");
            // Nothing writes past the frame: both borders are intact on
            // every row between the corners.
            for y in top.y + 1..bottom.y {
                assert_eq!(buffer[(top.x, y)].symbol(), "│", "{width}x{height} row {y}");
                assert!(
                    ["│", "┃"].contains(&buffer[(bottom.x, y)].symbol()),
                    "{width}x{height} row {y}"
                );
            }
            if height >= 30 {
                let message = drawn
                    .iter()
                    .find(|row| row.contains("\"word"))
                    .unwrap_or_else(|| panic!("no message at {width}\n{}", drawn.join("\n")));
                assert!(
                    message.trim_end_matches([' ', '│', '┃']).ends_with('…'),
                    "{message}"
                );
            }
        }
    }

    #[test]
    fn scrolling_stops_at_both_ends_of_a_short_terminal() {
        let mut model = open(board(story(now_ms())));
        draw(&mut model, 100, 20);
        let maximum = view(&mut model).max_scroll;
        assert!(maximum > 0, "the story overflows a 20-row terminal");
        assert_eq!(view(&mut model).scroll, 0);
        view(&mut model).scroll = usize::MAX;
        let buffer = draw(&mut model, 100, 20);
        assert_eq!(view(&mut model).scroll, maximum);
        assert!(
            find(&buffer, "created  from main").is_some(),
            "the oldest line"
        );
        assert!(
            find(&buffer, "Fix the login redirect").is_some(),
            "the header stays pinned"
        );
        let key = |code| KeyEvent::from(code);
        assert_eq!(model.input(key(KeyCode::Char('j')), 20), InputResult::Draw);
        assert_eq!(view(&mut model).scroll, maximum);
        model.input(key(KeyCode::Char('k')), 20);
        assert_eq!(view(&mut model).scroll, maximum - 3);
        model.input(key(KeyCode::Char('g')), 20);
        assert_eq!(view(&mut model).scroll, 0);
        model.input(key(KeyCode::Up), 20);
        assert_eq!(view(&mut model).scroll, 0);
        model.input(key(KeyCode::PageDown), 20);
        assert_eq!(view(&mut model).scroll, 10.min(maximum));
        model.input(key(KeyCode::Char('G')), 20);
        assert_eq!(view(&mut model).scroll, maximum);
        // Too short to pin the header: it scrolls away with the rest.
        let mut short = open(board(story(now_ms())));
        let buffer = draw(&mut short, 100, 9);
        assert!(find(&buffer, "Fix the login redirect").is_some());
        view(&mut short).scroll = usize::MAX;
        let buffer = draw(&mut short, 100, 9);
        assert!(find(&buffer, "Fix the login redirect").is_none());
        assert!(find(&buffer, "created  from main").is_some());
    }

    #[test]
    fn new_events_do_not_move_a_reader_who_scrolled_down() {
        let now = now_ms();
        let events = story(now);
        let mut model = open(board(events.clone()));
        draw(&mut model, 100, 20);
        view(&mut model).scroll = 5;
        let before = top_entry(&draw(&mut model, 100, 20));
        let mut newer = events.clone();
        newer.push(at(now - 2 * MINUTE, Kind::Conflict { resolved: false }));
        newer.push(at(now - MINUTE, Kind::Conflict { resolved: true }));
        model.board = board(newer.clone()).into();
        let buffer = draw(&mut model, 100, 20);
        assert_eq!(top_entry(&buffer), before);
        assert_eq!(view(&mut model).scroll, 7);
        // A scroll key after the snapshot still moves from where it is.
        model.input(KeyEvent::from(KeyCode::Char('k')), 20);
        draw(&mut model, 100, 20);
        assert_eq!(view(&mut model).scroll, 4);
        // At the top the reader is following along: the new events show.
        let mut following = open(board(events));
        draw(&mut following, 100, 20);
        following.board = board(newer).into();
        let buffer = draw(&mut following, 100, 20);
        assert_eq!(view(&mut following).scroll, 0);
        assert!(top_entry(&buffer).starts_with("Past 24 hours"));
        assert!(find(&buffer, "conflict resolved").is_some());
    }

    #[test]
    fn h_opens_the_selected_rows_history_and_owns_the_keys_until_closed() {
        let key = |ch| KeyEvent::from(KeyCode::Char(ch));
        let mut model = Model::default();
        model.input(key('H'), 20);
        assert!(matches!(model.interaction, Interaction::None), "no row");
        model.apply(wt_runtime::SourceSnapshot::test_live(
            Some(board(story(now_ms()))),
            wt_runtime::SourceState::Ready,
            wt_core::Domain::Git,
        ));
        assert_eq!(model.input(key('H'), 20), InputResult::Draw);
        assert_eq!(view(&mut model).key, "one");
        // A board key typed over the modal goes nowhere.
        for ch in ['d', 'a', 'h', 'u'] {
            assert_eq!(model.input(key(ch), 20), InputResult::Unchanged, "{ch}");
            assert!(matches!(model.interaction, Interaction::Timeline(_)));
        }
        assert!(!model.history.active);
        for close in [KeyCode::Char('H'), KeyCode::Char('q'), KeyCode::Esc] {
            assert!(matches!(model.interaction, Interaction::Timeline(_)));
            assert_eq!(model.input(KeyEvent::from(close), 20), InputResult::Draw);
            assert!(matches!(model.interaction, Interaction::None));
            model.input(key('H'), 20);
        }
    }

    #[test]
    fn a_removal_record_shows_the_same_timeline_when_the_row_has_history() {
        let mut board = board(story(now_ms()));
        assert!(record_block(&board, "absent", 60).is_empty());
        let block = record_block(&board, "one", 60);
        let lines: Vec<String> = block.iter().map(text).collect();
        assert!(lines[0].starts_with("created 3d ago · removed 10m ago"));
        assert!(lines.iter().any(|line| line.contains("ready ↔ review")));
        assert!(lines.iter().all(|line| line.width() <= 60), "{lines:?}");
        assert!(lines.iter().any(|line| line.trim_end().ends_with('…')));
        board.row_history = [("one".to_owned(), vec![at(1, Kind::Unknown)])].into();
        assert!(record_block(&board, "one", 60).is_empty());
    }
}
