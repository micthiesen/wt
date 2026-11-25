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
    /// The worktree's session was sent its prompt.
    Woken,
    /// The person was told.
    Notified,
    Cancelled,
    /// wt gave it up; the event's `reason` says why.
    Dropped,
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

    #[test]
    fn a_watch_event_is_tagged_like_every_other_kind() {
        let event = WorktreeEventKind::Watch {
            change: WatchChange::ClearedGate,
            what: "#3806 in production".into(),
            by: None,
            reason: None,
        };
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
                change: WatchChange::Dropped,
                what: "merged".into(),
                by: Some("one".into()),
                reason: Some("gone".into()),
            }
        );
    }

    #[test]
    fn event_text_keeps_one_bounded_line() {
        assert_eq!(event_text("\n  first line \nsecond"), "first line");
        let long = "x".repeat(400);
        let cut = event_text(&long);
        assert_eq!(cut.chars().count(), EVENT_TEXT_MAX);
        assert!(cut.ends_with('…'));
    }
}
