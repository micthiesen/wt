//! One worktree's history: the durable record of how a row got where it is.
//! Events are facts as they happened; presentation chooses what to show.
use serde::{Deserialize, Serialize};

use crate::{WorkRisk, WorkState};

/// Longest free text an event keeps (a note's first line, a message's first
/// line). History is a timeline, not an archive of what was said.
pub const EVENT_TEXT_MAX: usize = 160;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeEvent {
    pub at_ms: u64,
    pub event: WorktreeEventKind,
}

/// What happened. Stored as JSON, so a build that meets a kind it does not
/// know reads it as `Unknown` and skips it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeEventKind {
    /// The worktree was created. A slug's history starts at its latest one.
    Created {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
    },
    /// The asserted work status changed: its state, risk, merge gate or
    /// post-merge check. A re-assertion that changes only the note is not
    /// an event.
    Status {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<WorkState>,
        to: WorkState,
        /// Who asserted it: a worktree slug, `manager`, or `None` for the
        /// person.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        risk: Option<WorkRisk>,
        /// First line of the note.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blocked_on: Option<String>,
        /// `staging` or `production` when a post-merge check is owed there.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verify_in: Option<String>,
    },
    /// The status was cleared.
    StatusCleared {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<WorkState>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    Pr {
        number: u64,
        change: PrChange,
    },
    Checks {
        state: ChecksChange,
        /// Failing check names, when failing.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        failed: Vec<String>,
    },
    Review {
        /// The review bot's configured name, or `None` for people.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reviewer: Option<String>,
        change: ReviewChange,
    },
    MergeQueue {
        change: QueueChange,
    },
    /// The branch stopped, or started, rebasing cleanly onto its base.
    Conflict {
        resolved: bool,
    },
    Landed {
        on: LandedOn,
    },
    Automation {
        rule: String,
        outcome: AutomationOutcomeKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A message sent to this worktree's agent with `wt agent send`,
    /// `wt agent start` or the manager.
    Message {
        /// The sending worktree's slug, `manager`, or `None` for the person.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        /// First line of the text.
        text: String,
        /// `Some(true)` confirmed in the transcript, `Some(false)` known not
        /// to have arrived, `None` submitted without confirmation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivered: Option<bool>,
    },
    Restack {
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        onto: Option<String>,
    },
    Removed {
        /// Whether the branch had landed when it was removed.
        landed: bool,
    },
    /// Something the worktree waits for was set, came true, was acted on or
    /// was given up.
    Watch {
        /// The watch's id. Absent from events recorded before v0.1.73.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        change: WatchChange,
        /// What was waited for, in words.
        what: String,
        /// Who set or cancelled it: a worktree slug, `manager`, or `None`
        /// for the person and for wt itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
        /// Why wt dropped it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// What it does once every condition holds, as
        /// `WatchAction::as_str` names in delivery order. Recorded when it
        /// is set.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        then: Vec<String>,
        /// A wake was sent without proof it reached the session.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        unconfirmed: bool,
    },
    /// Something the worktree owes after landing came due, was done, was
    /// put off or was given up: a step it was woken for, or a post-merge
    /// check.
    Owed {
        /// A step's id, as `wt done <id>` takes it. Absent for a check,
        /// which is named by its `stage`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        change: OwedChange,
        /// First line of what is owed: the step's prompt or the check's
        /// steps.
        what: String,
        /// What was done and seen, given when it was closed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<String>,
        /// Who closed, re-armed or dropped it: a worktree slug, `manager`,
        /// or `None` for the person and for wt itself.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
        /// `staging` or `production` for a check.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stage: Option<String>,
        /// What it waits for, in words: the conditions that held when it
        /// came due, or the ones it was put off to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        when: Option<String>,
        /// It came due by a wake that was sent without proof it reached the
        /// session.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        unconfirmed: bool,
    },
    /// A skill or slash command was used in the worktree's session, as its
    /// harness reported through `wt mark`.
    Skill {
        /// Without the leading slash.
        name: String,
        /// First line of what followed the name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
        /// The session's agent (a worktree slug or `manager`) when it
        /// invoked the skill itself, `None` when the person typed it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    /// A one-line note added with `wt mark`.
    Mark {
        text: String,
        /// Who added it: a worktree slug, `manager`, or `None` for the
        /// person.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchChange {
    Set,
    /// Every condition held.
    Held,
    /// wt cleared the merge gate.
    ClearedGate,
    /// The worktree's session was sent its prompt; the event's
    /// `unconfirmed` says when nothing proved it arrived.
    Woken,
    /// The person was told.
    Notified,
    Cancelled,
    /// wt gave it up; the event's `reason` says why.
    Dropped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwedChange {
    /// It came due and stays owed until it is closed.
    Due,
    /// Closed with evidence.
    Done,
    /// Put off: it waits again.
    Again,
    /// Given up without being done.
    Dropped,
}

/// How an owed event reads in one sentence: `due: <what> (wt done <id>)`,
/// `done: <what>: <evidence>`, `again <when>: <what>`, `dropped: <what>`.
/// A check is named by its stage. `kind` must be the `Owed` variant;
/// anything else reads as nothing.
pub fn owed_sentence(kind: &WorktreeEventKind) -> Option<String> {
    let WorktreeEventKind::Owed {
        id,
        change,
        what,
        evidence,
        stage,
        when,
        unconfirmed,
        by: _,
    } = kind
    else {
        return None;
    };
    let what = match stage {
        Some(stage) => format!("{stage} check: {what}"),
        None => what.clone(),
    };
    Some(match change {
        OwedChange::Due => {
            let close = id.as_deref().or(stage.as_deref());
            let close = close.map(|target| format!(" (wt done {target})"));
            let unsure = if *unconfirmed {
                "; wake sent, delivery not confirmed"
            } else {
                ""
            };
            format!("due: {what}{}{unsure}", close.unwrap_or_default())
        }
        OwedChange::Done => match evidence {
            Some(evidence) => format!("done: {what}: {evidence}"),
            None => format!("done: {what}"),
        },
        OwedChange::Again => match when {
            Some(when) => format!("again {when}: {what}"),
            None => format!("again: {what}"),
        },
        OwedChange::Dropped => match evidence {
            Some(reason) => format!("dropped: {what}: {reason}"),
            None => format!("dropped: {what}"),
        },
    })
}

impl WatchChange {
    /// Whether this is something wt did about a met watch.
    pub const fn is_action(self) -> bool {
        matches!(self, Self::ClearedGate | Self::Woken | Self::Notified)
    }
}

/// A watch that holds and is acted on within this long reads as one event.
pub const WATCH_IMMEDIATE_MS: u64 = 60 * 1000;
/// A watch that holds this soon after it was set never really waited.
pub const WATCH_JUST_SET_MS: u64 = 10 * 1000;

/// One recorded event's place in a history that tells a watch in two lines:
/// when it was set, and what wt did about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryLine<'a> {
    Event(&'a WorktreeEvent),
    /// Said by another line. A status among these still stands.
    Folded(&'a WorktreeEvent),
    Fired(WatchFired<'a>),
}

/// What wt did about one met watch, from the events that recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchFired<'a> {
    /// When its first action happened: stable while later ones join it.
    pub first_ms: u64,
    pub at_ms: u64,
    pub what: &'a str,
    /// The actions (see [`WatchChange::is_action`]) as they happened.
    pub done: Vec<WatchChange>,
    /// The wake was sent without proof it reached the session.
    pub unconfirmed: bool,
    /// It held within [`WATCH_JUST_SET_MS`] of being set; this line
    /// stands for that one too.
    pub just_set: bool,
}

/// Read events, oldest first, as lines. For each watch: `held` is said by
/// what wt then did or by wt giving up, actions within
/// [`WATCH_IMMEDIATE_MS`] of each other are one line, the status change
/// that is wt clearing the gate is that line, and so is the message event
/// earlier builds recorded for the wake's prompt. Events match a watch by
/// id, and by wording when recorded before they carried one.
pub fn fold_watch_history(events: &[WorktreeEvent]) -> Vec<HistoryLine<'_>> {
    use WorktreeEventKind as Kind;
    let watch = |index: usize| match &events[index].event {
        Kind::Watch {
            id,
            change,
            what,
            unconfirmed,
            ..
        } => Some((id.as_deref(), *change, what.as_str(), *unconfirmed)),
        _ => None,
    };
    let same = |one: usize, other: usize| match (watch(one), watch(other)) {
        (Some((Some(id), ..)), Some((Some(other), ..))) => id == other,
        (Some((_, _, what, _)), Some((_, _, other, _))) => what == other,
        _ => false,
    };
    let change = |index: usize| watch(index).map(|(_, change, ..)| change);
    // A wake delivered after its worktree landed records that the step came
    // due where another wake records "woken": that line says it held.
    let came_due = |held: usize| {
        let Some((Some(held_id), ..)) = watch(held) else {
            return false;
        };
        events[held..].iter().any(|event| {
            matches!(
                &event.event,
                Kind::Owed { id: Some(id), change: OwedChange::Due, .. } if id == held_id
            )
        })
    };
    // A step set again is a new wake: the line that says "again at 17:00"
    // says the wake was set, so the set itself is not another line.
    let set_again = |set: usize| {
        let Some((_, WatchChange::Set, what, _)) = watch(set) else {
            return false;
        };
        events.iter().any(|event| {
            event.at_ms.abs_diff(events[set].at_ms) <= WATCH_IMMEDIATE_MS
                && matches!(
                    &event.event,
                    Kind::Owed { change: OwedChange::Again, when: Some(when), .. }
                        if when.strip_prefix("at ").or(when.strip_prefix("when ")) == Some(what)
                )
        })
    };
    // A `held` that what wt did, or its giving up, says later.
    let mut folded: Vec<bool> = (0..events.len())
        .map(|held| {
            set_again(held)
                || change(held) == Some(WatchChange::Held)
                    && ((held + 1..events.len())
                        .filter(|later| same(held, *later))
                        .map_while(|later| {
                            change(later).filter(|change| *change != WatchChange::Set)
                        })
                        .any(|change| change.is_action() || change == WatchChange::Dropped)
                        || came_due(held))
        })
        .collect();
    let mut fired: Vec<Option<WatchFired<'_>>> = vec![None; events.len()];
    for first in 0..events.len() {
        let Some((_, _, what, _)) = watch(first).filter(|(_, change, ..)| change.is_action())
        else {
            continue;
        };
        if folded[first] || fired[first].is_some() {
            continue;
        }
        let mut members = vec![first];
        for next in first + 1..events.len() {
            let last = events[members[members.len() - 1]].at_ms;
            if events[next].at_ms.saturating_sub(last) > WATCH_IMMEDIATE_MS {
                break;
            }
            if same(first, next) && change(next).is_some_and(WatchChange::is_action) {
                members.push(next);
            }
        }
        let mut unconfirmed = false;
        for &member in &members {
            match watch(member) {
                Some((_, WatchChange::Woken, _, unsure)) => {
                    unconfirmed |= unsure;
                    // The prompt's own event, and those of tries before it.
                    let prompts = (0..member).rev().take_while(|index| {
                        matches!(
                            &events[*index].event,
                            Kind::Message { from: Some(from), text, .. }
                                if from == "wt" && text.starts_with("[wt] ")
                        )
                    });
                    for (nth, prompt) in prompts.enumerate() {
                        folded[prompt] = true;
                        if nth == 0
                            && let Kind::Message {
                                delivered: Some(false),
                                ..
                            } = events[prompt].event
                        {
                            unconfirmed = true;
                        }
                    }
                }
                Some((_, WatchChange::ClearedGate, ..)) if member > 0 => {
                    let standing = events[..member - 1]
                        .iter()
                        .rev()
                        .find_map(|event| match &event.event {
                            Kind::Status { to, blocked_on, .. } => {
                                Some(Some((*to, blocked_on.is_some())))
                            }
                            Kind::StatusCleared { .. } => Some(None),
                            _ => None,
                        });
                    if let Kind::Status {
                        to,
                        blocked_on: None,
                        ..
                    } = &events[member - 1].event
                        && standing == Some(Some((*to, true)))
                    {
                        folded[member - 1] = true;
                    }
                }
                _ => {}
            }
        }
        let last = members[members.len() - 1];
        for &member in &members[..members.len() - 1] {
            folded[member] = true;
        }
        let set = (0..first)
            .rev()
            .filter(|earlier| same(first, *earlier))
            .find(|earlier| change(*earlier) == Some(WatchChange::Set));
        let held = set.and_then(|set| {
            (set + 1..first)
                .filter(|index| same(first, *index))
                .find(|index| change(*index) == Some(WatchChange::Held))
        });
        let just_set = match (set, held) {
            (Some(set), Some(held)) => {
                events[held].at_ms.saturating_sub(events[set].at_ms) <= WATCH_JUST_SET_MS
                    && events[first].at_ms.saturating_sub(events[held].at_ms) <= WATCH_IMMEDIATE_MS
                    // Nothing else happened that the set's line stands before.
                    && (set + 1..last).all(|between| folded[between] || members.contains(&between))
            }
            _ => false,
        };
        if let (true, Some(set)) = (just_set, set) {
            folded[set] = true;
        }
        fired[last] = Some(WatchFired {
            first_ms: events[first].at_ms,
            at_ms: events[last].at_ms,
            what,
            done: members
                .iter()
                .filter_map(|member| change(*member))
                .collect(),
            unconfirmed,
            just_set,
        });
    }
    events
        .iter()
        .zip(folded)
        .zip(fired)
        .map(|((event, folded), fired)| match fired {
            Some(fired) => HistoryLine::Fired(fired),
            None if folded => HistoryLine::Folded(event),
            None => HistoryLine::Event(event),
        })
        .collect()
}

/// A watch line's words: a headline and what it is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchWords {
    pub headline: String,
    /// `at 17:00`, `when merged`, or what held.
    pub about: String,
    /// What a set watch does after its first action: `then wakes the session`.
    pub then: Option<&'static str>,
    /// `about` carries on the headline's sentence; otherwise it follows a
    /// colon.
    flows: bool,
}

impl WatchWords {
    /// The line as one sentence.
    pub fn sentence(&self) -> String {
        let mut sentence = if self.flows {
            format!("{} {}", self.headline, self.about)
        } else {
            format!("{}: {}", self.headline, self.about)
        };
        if let Some(then) = self.then {
            sentence.push_str(", ");
            sentence.push_str(then);
        }
        sentence
    }
}

/// How a newly set watch reads. `then` is the event's list; names this
/// build does not know are left out, and with none known the line only says
/// what is waited for.
pub fn watch_set_words(then: &[String], what: &str) -> WatchWords {
    let does = |action: &str| then.iter().any(|known| known == action);
    let (headline, after) = match (does("unblock"), does("notify"), does("wake")) {
        (true, true, true) => ("gate clears", Some("then tells you and wakes the session")),
        (true, true, false) => ("gate clears", Some("then tells you")),
        (true, false, true) => ("gate clears", Some("then wakes the session")),
        (true, false, false) => ("gate clears", None),
        (false, true, true) => ("will tell you and wake the session", None),
        (false, true, false) => ("will tell you", None),
        (false, false, true) => ("will wake the session", None),
        (false, false, false) => {
            return WatchWords {
                headline: "waiting".into(),
                about: format!("for {what}"),
                then: None,
                flows: true,
            };
        }
    };
    WatchWords {
        headline: headline.into(),
        about: crate::watch::when_phrase(what),
        then: after,
        flows: true,
    }
}

/// How what wt did about a met watch reads: `woke the session at 23:08`,
/// `cleared the gate and woke the session: check flag=go held`.
pub fn watch_fired_words(done: &[WatchChange], unconfirmed: bool, what: &str) -> WatchWords {
    let did: Vec<&str> = [
        (WatchChange::ClearedGate, "cleared the gate"),
        (WatchChange::Notified, "told you"),
        (WatchChange::Woken, "woke the session"),
    ]
    .into_iter()
    .filter(|(change, _)| done.contains(change))
    .filter(|(change, _)| !(unconfirmed && *change == WatchChange::Woken))
    .map(|(_, words)| words)
    .collect();
    let mut headline = match did.split_last() {
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    };
    if unconfirmed {
        if !headline.is_empty() {
            headline.push_str("; ");
        }
        headline.push_str("wake sent, delivery not confirmed");
    }
    let timed = crate::watch::is_time_phrase(what);
    WatchWords {
        headline,
        about: if timed {
            format!("at {what}")
        } else {
            // "check flag=go" names a check, not what became of it.
            what.split(" and ")
                .map(|part| {
                    if part.starts_with("check ") {
                        format!("{part} held")
                    } else {
                        part.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join(" and ")
        },
        then: None,
        flows: timed,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrChange {
    Opened,
    OpenedDraft,
    ReadyForReview,
    BackToDraft,
    Closed,
    Reopened,
    Merged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksChange {
    Failing,
    Passing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewChange {
    /// Unresolved findings, with their count.
    Findings {
        count: u32,
    },
    /// Nothing unresolved on the current head.
    Clean,
    ChangesRequested,
    Approved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueChange {
    Queued,
    /// Left the queue without merging.
    Left,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandedOn {
    Base,
    Production,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationOutcomeKind {
    Delivered,
    Skipped,
    Failed,
    /// Sent without proof it arrived.
    Unconfirmed,
}

/// The first line of `text`, trimmed and cut to [`EVENT_TEXT_MAX`] characters.
pub fn event_text(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty());
    let line = line.unwrap_or_default();
    if line.chars().count() <= EVENT_TEXT_MAX {
        return line.to_owned();
    }
    let mut cut: String = line.chars().take(EVENT_TEXT_MAX - 1).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_kind_from_a_newer_build_reads_as_unknown() {
        let known: WorktreeEventKind =
            serde_json::from_str(r#"{"kind":"conflict","resolved":true}"#).unwrap();
        assert_eq!(known, WorktreeEventKind::Conflict { resolved: true });
        let newer: WorktreeEventKind =
            serde_json::from_str(r#"{"kind":"teleported","to":"mars"}"#).unwrap();
        assert_eq!(newer, WorktreeEventKind::Unknown);
    }

    fn watch(id: &str, change: WatchChange, what: &str) -> WorktreeEventKind {
        WorktreeEventKind::Watch {
            id: (!id.is_empty()).then(|| id.to_owned()),
            change,
            what: what.into(),
            by: None,
            reason: None,
            then: Vec::new(),
            unconfirmed: false,
        }
    }

    fn at(at_ms: u64, event: WorktreeEventKind) -> WorktreeEvent {
        WorktreeEvent { at_ms, event }
    }

    #[test]
    fn a_watch_event_is_tagged_like_every_other_kind() {
        let event = watch("", WatchChange::ClearedGate, "#3806 in production");
        let stored = serde_json::to_value(&event).unwrap();
        assert_eq!(
            stored,
            serde_json::json!({
                "kind": "watch", "change": "cleared_gate", "what": "#3806 in production"
            })
        );
        assert_eq!(
            serde_json::from_value::<WorktreeEventKind>(stored).unwrap(),
            event
        );
        let dropped: WorktreeEventKind = serde_json::from_str(
            r#"{"kind":"watch","change":"dropped","what":"merged","by":"one","reason":"gone"}"#,
        )
        .unwrap();
        assert_eq!(
            dropped,
            WorktreeEventKind::Watch {
                id: None,
                change: WatchChange::Dropped,
                what: "merged".into(),
                by: Some("one".into()),
                reason: Some("gone".into()),
                then: Vec::new(),
                unconfirmed: false,
            }
        );
        let set = WorktreeEventKind::Watch {
            id: Some("aaaaaa".into()),
            change: WatchChange::Set,
            what: "merged".into(),
            by: None,
            reason: None,
            then: vec!["unblock".into(), "wake".into()],
            unconfirmed: false,
        };
        let stored = serde_json::to_value(&set).unwrap();
        assert_eq!(stored["then"], serde_json::json!(["unblock", "wake"]));
        assert_eq!(stored["id"], "aaaaaa");
        assert_eq!(
            serde_json::from_value::<WorktreeEventKind>(stored).unwrap(),
            set
        );
        let unsure = WorktreeEventKind::Watch {
            id: None,
            change: WatchChange::Woken,
            what: "17:00".into(),
            by: None,
            reason: None,
            then: Vec::new(),
            unconfirmed: true,
        };
        let stored = serde_json::to_value(&unsure).unwrap();
        assert_eq!(stored["unconfirmed"], true);
        assert_eq!(
            serde_json::from_value::<WorktreeEventKind>(stored).unwrap(),
            unsure
        );
    }

    #[test]
    fn a_set_watch_reads_as_what_it_will_do_and_when() {
        let words = |then: &[&str], what: &str| {
            let then: Vec<String> = then.iter().map(|action| (*action).to_owned()).collect();
            watch_set_words(&then, what).sentence()
        };
        assert_eq!(words(&["wake"], "23:08"), "will wake the session at 23:08");
        assert_eq!(
            words(&["wake"], "check flag=go"),
            "will wake the session when check flag=go"
        );
        assert_eq!(words(&["notify"], "merged"), "will tell you when merged");
        assert_eq!(
            words(&["notify", "wake"], "01:01 tomorrow"),
            "will tell you and wake the session at 01:01 tomorrow"
        );
        assert_eq!(
            words(&["unblock", "wake"], "#3806 in production"),
            "gate clears when #3806 in production, then wakes the session"
        );
        assert_eq!(
            words(&["unblock", "notify", "teleport", "wake"], "merged"),
            "gate clears when merged, then tells you and wakes the session"
        );
        assert_eq!(words(&["unblock"], "17:00"), "gate clears at 17:00");
        // Nothing this build knows: only what is waited for.
        assert_eq!(words(&["teleport"], "merged"), "waiting for merged");
        assert_eq!(words(&[], "merged"), "waiting for merged");
    }

    #[test]
    fn what_wt_did_reads_as_one_sentence() {
        use WatchChange::{ClearedGate, Notified, Woken};
        let words =
            |done: &[WatchChange], unsure, what| watch_fired_words(done, unsure, what).sentence();
        assert_eq!(words(&[Woken], false, "23:08"), "woke the session at 23:08");
        assert_eq!(
            words(&[Woken], false, "check flag=go"),
            "woke the session: check flag=go held"
        );
        assert_eq!(words(&[Notified], false, "merged"), "told you: merged");
        assert_eq!(
            words(&[ClearedGate], false, "check flag=go"),
            "cleared the gate: check flag=go held"
        );
        assert_eq!(
            words(&[ClearedGate, Woken], false, "merged and check up for 10m"),
            "cleared the gate and woke the session: merged and check up for 10m held"
        );
        assert_eq!(
            words(&[ClearedGate, Notified, Woken], false, "merged"),
            "cleared the gate, told you and woke the session: merged"
        );
        assert_eq!(
            words(&[Woken], true, "merged"),
            "wake sent, delivery not confirmed: merged"
        );
        assert_eq!(
            words(&[ClearedGate, Woken], true, "23:08"),
            "cleared the gate; wake sent, delivery not confirmed at 23:08"
        );
    }

    fn message(delivered: Option<bool>) -> WorktreeEventKind {
        WorktreeEventKind::Message {
            from: Some("wt".into()),
            text: "[wt] What you were waiting for has happened: merged.".into(),
            delivered,
        }
    }

    fn status(blocked_on: Option<&str>) -> WorktreeEventKind {
        WorktreeEventKind::Status {
            from: Some(WorkState::Ready),
            to: WorkState::Ready,
            by: None,
            risk: None,
            note: None,
            blocked_on: blocked_on.map(str::to_owned),
            verify_in: None,
        }
    }

    /// What each event became: `e` shown, `-` folded, `F` the fired line.
    fn shape(events: &[WorktreeEvent]) -> String {
        fold_watch_history(events)
            .iter()
            .map(|line| match line {
                HistoryLine::Event(_) => 'e',
                HistoryLine::Folded(_) => '-',
                HistoryLine::Fired(_) => 'F',
            })
            .collect()
    }

    fn fired(events: &[WorktreeEvent]) -> Vec<WatchFired<'_>> {
        fold_watch_history(events)
            .into_iter()
            .filter_map(|line| match line {
                HistoryLine::Fired(fired) => Some(fired),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn one_watch_folds_to_when_it_was_set_and_what_wt_did() {
        use WatchChange::{Cancelled, ClearedGate, Dropped, Held, Notified, Set, Woken};
        const HOUR: u64 = 3_600_000;
        // A wake delivered after the work landed records the step that
        // came due instead of "woken": that line says it held, here and
        // in `wt history` alike. Another watch's `held` stands.
        let due = WorktreeEventKind::Owed {
            id: Some("a".into()),
            change: OwedChange::Due,
            what: "confirm prod logs".into(),
            evidence: None,
            by: None,
            stage: None,
            when: None,
            unconfirmed: false,
        };
        let became_due = [
            at(0, watch("a", Set, "merged")),
            at(HOUR, watch("a", Held, "merged")),
            at(HOUR + 1, due),
            at(HOUR + 2, watch("b", Held, "17:00")),
        ];
        assert_eq!(shape(&became_due), "e-ee");
        // Set again: one line, "again at 17:00", stands for the new wake's
        // "will wake the session" too. A wake set by hand still shows.
        let again = WorktreeEventKind::Owed {
            id: Some("a".into()),
            change: OwedChange::Again,
            what: "confirm prod logs".into(),
            evidence: None,
            by: None,
            stage: None,
            when: Some("at 17:00".into()),
            unconfirmed: false,
        };
        let rearmed = [
            at(0, watch("c", Set, "17:00")),
            at(1, again),
            at(2 * HOUR, watch("d", Set, "17:00")),
        ];
        assert_eq!(shape(&rearmed), "-ee");
        // A wake as earlier builds recorded it: held, the prompt's own
        // message, then the wake.
        let wake = [
            at(0, watch("a", Set, "merged")),
            at(HOUR, watch("a", Held, "merged")),
            at(HOUR + 1, message(Some(true))),
            at(HOUR + 2, watch("a", Woken, "merged")),
        ];
        assert_eq!(shape(&wake), "e--F");
        assert_eq!(
            fired(&wake),
            [WatchFired {
                first_ms: HOUR + 2,
                at_ms: HOUR + 2,
                what: "merged",
                done: vec![Woken],
                unconfirmed: false,
                just_set: false,
            }]
        );
        // The prompt known not to have arrived, after tries that did not.
        let unsure = [
            at(0, watch("a", Held, "merged")),
            at(1, message(Some(false))),
            at(2, message(Some(false))),
            at(3, watch("a", Woken, "merged")),
        ];
        assert_eq!(shape(&unsure), "---F");
        assert!(fired(&unsure)[0].unconfirmed);
        // As this build records it.
        let mut woken = watch("a", Woken, "merged");
        if let WorktreeEventKind::Watch { unconfirmed, .. } = &mut woken {
            *unconfirmed = true;
        }
        assert!(fired(&[at(0, woken)])[0].unconfirmed);
        // Someone else's message is not the prompt.
        let other = [
            at(
                0,
                WorktreeEventKind::Message {
                    from: Some("manager".into()),
                    text: "[wt] hello".into(),
                    delivered: Some(false),
                },
            ),
            at(1, watch("a", Woken, "merged")),
        ];
        assert_eq!(shape(&other), "eF");
        assert!(!fired(&other)[0].unconfirmed);

        // A gate: the status that drops it is wt clearing it, and the wake
        // that follows at once is the same line.
        let gate = [
            at(0, status(Some("the release"))),
            at(1, watch("a", Set, "merged")),
            at(HOUR, watch("a", Held, "merged")),
            at(HOUR + 1, status(None)),
            at(HOUR + 2, watch("a", ClearedGate, "merged")),
            at(HOUR + 3, watch("a", Woken, "merged")),
        ];
        assert_eq!(shape(&gate), "ee---F");
        let line = &fired(&gate)[0];
        assert_eq!(line.done, [ClearedGate, Woken]);
        assert_eq!((line.first_ms, line.at_ms), (HOUR + 2, HOUR + 3));
        // A status that changed anything else is its own line.
        let mut moved = gate.clone();
        moved[3] = at(
            HOUR + 1,
            WorktreeEventKind::Status {
                from: Some(WorkState::Ready),
                to: WorkState::Working,
                by: None,
                risk: None,
                note: None,
                blocked_on: None,
                verify_in: None,
            },
        );
        assert_eq!(shape(&moved), "ee-e-F");
        // A wake delivered long after the gate cleared is a second fact.
        let late = [
            at(0, watch("a", Held, "merged")),
            at(1, watch("a", ClearedGate, "merged")),
            at(WATCH_IMMEDIATE_MS + 2, watch("a", Woken, "merged")),
        ];
        assert_eq!(shape(&late), "-FF");

        // Held with nothing done yet stands alone; giving up says it too.
        assert_eq!(
            shape(&[
                at(0, watch("a", Set, "merged")),
                at(1, watch("a", Held, "merged"))
            ]),
            "ee"
        );
        assert_eq!(
            shape(&[
                at(0, watch("a", Held, "merged")),
                at(1, watch("a", Dropped, "merged"))
            ]),
            "-e"
        );
        assert_eq!(
            shape(&[
                at(0, watch("a", Set, "merged")),
                at(1, watch("a", Cancelled, "merged"))
            ]),
            "ee"
        );
        // Another watch's action says nothing about this one, by id or, for
        // events without one, by wording.
        assert_eq!(
            shape(&[
                at(0, watch("a", Held, "merged")),
                at(1, watch("b", Notified, "merged"))
            ]),
            "eF"
        );
        assert_eq!(
            shape(&[
                at(0, watch("", Held, "merged")),
                at(1, watch("", Notified, "ready"))
            ]),
            "eF"
        );
        assert_eq!(
            shape(&[
                at(0, watch("", Held, "merged")),
                at(1, watch("", Notified, "merged"))
            ]),
            "-F"
        );
        // Delivery that waited (paused automations) still says "held".
        assert_eq!(
            shape(&[
                at(0, watch("a", Held, "merged")),
                at(1, status(Some("x"))),
                at(HOUR, watch("a", Notified, "merged")),
            ]),
            "-eF"
        );
    }

    #[test]
    fn a_watch_that_fires_as_soon_as_it_is_set_is_one_line() {
        use WatchChange::{Held, Notified, Set};
        let instant = [
            at(0, watch("a", Set, "17:00")),
            at(1_000, watch("a", Held, "17:00")),
            at(2_000, watch("a", Notified, "17:00")),
        ];
        assert_eq!(shape(&instant), "--F");
        assert!(fired(&instant)[0].just_set);
        // A wait is a wait.
        let waited = [
            at(0, watch("a", Set, "17:00")),
            at(WATCH_JUST_SET_MS + 1, watch("a", Held, "17:00")),
            at(WATCH_JUST_SET_MS + 2, watch("a", Notified, "17:00")),
        ];
        assert_eq!(shape(&waited), "e-F");
        assert!(!fired(&waited)[0].just_set);
        // Something else happened after it was set: its line stays before it.
        let between = [
            at(0, watch("a", Set, "17:00")),
            at(1, status(None)),
            at(1_000, watch("a", Held, "17:00")),
            at(2_000, watch("a", Notified, "17:00")),
        ];
        assert_eq!(shape(&between), "ee-F");
    }

    #[test]
    fn event_text_keeps_one_bounded_line() {
        assert_eq!(event_text("\n  first line \nsecond"), "first line");
        let long = "x".repeat(400);
        let cut = event_text(&long);
        assert_eq!(cut.chars().count(), EVENT_TEXT_MAX);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn owed_events_read_as_one_sentence_each() {
        let owed = |id: Option<&str>, change, stage: Option<&str>| WorktreeEventKind::Owed {
            id: id.map(str::to_owned),
            change,
            what: "confirm prod logs".into(),
            evidence: Some("saw 200s for an hour".into()),
            by: Some("one".into()),
            stage: stage.map(str::to_owned),
            when: Some("at 17:00".into()),
            unconfirmed: false,
        };
        let said = |event: &WorktreeEventKind| owed_sentence(event).unwrap();
        assert_eq!(
            said(&owed(Some("93co6q"), OwedChange::Due, None)),
            "due: confirm prod logs (wt done 93co6q)"
        );
        assert_eq!(
            said(&owed(None, OwedChange::Due, Some("staging"))),
            "due: staging check: confirm prod logs (wt done staging)"
        );
        assert_eq!(
            said(&owed(Some("93co6q"), OwedChange::Done, None)),
            "done: confirm prod logs: saw 200s for an hour"
        );
        assert_eq!(
            said(&owed(Some("93co6q"), OwedChange::Again, None)),
            "again at 17:00: confirm prod logs"
        );
        let mut dropped = owed(Some("93co6q"), OwedChange::Dropped, None);
        if let WorktreeEventKind::Owed { evidence, .. } = &mut dropped {
            *evidence = None;
        }
        assert_eq!(said(&dropped), "dropped: confirm prod logs");
        let mut unsure = owed(Some("93co6q"), OwedChange::Due, None);
        if let WorktreeEventKind::Owed { unconfirmed, .. } = &mut unsure {
            *unconfirmed = true;
        }
        assert_eq!(
            said(&unsure),
            "due: confirm prod logs (wt done 93co6q); wake sent, delivery not confirmed"
        );
        assert_eq!(owed_sentence(&WorktreeEventKind::Unknown), None);
        // A build that does not know the kind skips it; this one reads it back.
        let stored = serde_json::to_value(&unsure).unwrap();
        assert_eq!(stored["kind"], "owed");
        assert_eq!(stored["change"], "due");
        assert_eq!(
            serde_json::from_value::<WorktreeEventKind>(stored).unwrap(),
            unsure
        );
    }
}
