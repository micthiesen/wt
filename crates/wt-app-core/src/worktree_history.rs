//! Writing to a worktree's history, and deciding what an observation adds.
//!
//! wt writes most events where the change happens (the store's status,
//! creation and removal writes; message delivery; automation fires). Pull
//! request state, checks, reviews, the merge queue, conflicts and landings
//! change outside wt, so a host observes them. [`transitions`] turns what it
//! observes into the events the log does not hold yet. It reads only the log,
//! never process memory, so a restarted host and a second host on the same
//! database both add nothing for a fact that is already recorded.

use wt_core::{
    ChecksChange, LandedOn, PrChange, QueueChange, ReviewChange, WorktreeEvent, WorktreeEventKind,
};

use crate::context::AppContext;

/// Events of one row a board carries. Every host frame, board clone and
/// remembered remote snapshot holds them, so the board takes fewer than
/// `wt history` can print.
pub const BOARD_EVENTS_PER_ROW: usize = 40;

/// Append one event for a local worktree. History is a record of the action,
/// not part of it: a failed append is logged and never fails the caller.
pub async fn record(context: &AppContext, slug: &str, event: WorktreeEventKind) {
    let owner = slug.to_owned();
    let at_ms = crate::feeds::epoch_ms();
    let appended = context
        .database
        .call(move |store| Ok(store.append_worktree_event(&owner, at_ms, &event)?))
        .await;
    if let Err(error) = appended {
        tracing::warn!(slug, %error, "could not record worktree history");
    }
}

/// Strip terminal control sequences from an event's free text. Notes,
/// messages and check names are written by agents and remote services, and
/// the board draws them.
pub fn sanitize(event: &mut WorktreeEventKind) {
    let clean = |text: &mut String| *text = wt_core::sanitize_terminal_text(text);
    let clean_some = |text: &mut Option<String>| {
        if let Some(text) = text {
            *text = wt_core::sanitize_terminal_text(text);
        }
    };
    match event {
        WorktreeEventKind::Created { base } => clean_some(base),
        WorktreeEventKind::Status {
            by,
            note,
            blocked_on,
            verify_in,
            from: _,
            to: _,
            risk: _,
        } => [by, note, blocked_on, verify_in]
            .into_iter()
            .for_each(clean_some),
        WorktreeEventKind::StatusCleared { by, from: _ } => clean_some(by),
        WorktreeEventKind::Checks { failed, state: _ } => failed.iter_mut().for_each(clean),
        WorktreeEventKind::Review {
            reviewer,
            change: _,
        } => clean_some(reviewer),
        WorktreeEventKind::Automation {
            rule,
            reason,
            outcome: _,
        } => {
            clean(rule);
            clean_some(reason);
        }
        WorktreeEventKind::Message {
            from,
            text,
            delivered: _,
        } => {
            clean_some(from);
            clean(text);
        }
        WorktreeEventKind::Restack { onto, ok: _ } => clean_some(onto),
        WorktreeEventKind::Watch {
            what,
            by,
            reason,
            then,
            change: _,
            id: _,
        } => {
            clean(what);
            clean_some(by);
            clean_some(reason);
            then.iter_mut().for_each(clean);
        }
        WorktreeEventKind::Pr { .. }
        | WorktreeEventKind::MergeQueue { .. }
        | WorktreeEventKind::Conflict { .. }
        | WorktreeEventKind::Landed { .. }
        | WorktreeEventKind::Removed { .. }
        | WorktreeEventKind::Unknown => {}
    }
}

/// What a confirmed read says about one worktree now. `None` anywhere means
/// "not known from this read", which records nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObservedRow {
    pub pr: Option<ObservedPr>,
    /// Whether the branch conflicts with its base.
    pub conflicted: Option<bool>,
    pub landed: Option<LandedOn>,
    /// The stored work status as the metadata read showed it: `Some(None)`
    /// for no status, `None` when metadata is not confirmed. It only says
    /// that the status may have changed; [`status_transition`] compares the
    /// status the store reads in the deciding transaction.
    pub work: Option<Option<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedPr {
    pub number: u64,
    pub phase: PrPhase,
    /// A finished verdict; `None` while checks run or there are none.
    pub checks: Option<ObservedChecks>,
    /// The review bot's configured name and its verdict on the current head.
    pub bot_review: Option<(String, ReviewChange)>,
    pub people_review: Option<ReviewChange>,
    /// `Some(false)` only when the queue read is known to be complete: one
    /// page of a long queue cannot prove a pull request left it.
    pub queued: Option<bool>,
}

impl ObservedRow {
    /// This observation with every unknown fact filled from `last`. A fetch
    /// gap (GitHub failing or refreshing, Git facts not yet recomputed) then
    /// compares equal to what was known before it, instead of looking like a
    /// change on every row.
    pub fn or_last_known(mut self, last: &ObservedRow) -> ObservedRow {
        match (&mut self.pr, &last.pr) {
            (None, Some(known)) => self.pr = Some(known.clone()),
            (Some(pr), Some(known)) if pr.number == known.number => {
                pr.queued = pr.queued.or(known.queued);
            }
            _ => {}
        }
        self.conflicted = self.conflicted.or(last.conflicted);
        self.landed = self.landed.or(last.landed);
        if self.work.is_none() {
            self.work.clone_from(&last.work);
        }
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrPhase {
    Draft,
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservedChecks {
    Passing,
    Failing(Vec<String>),
}

/// The events `observed` adds to `history` (one slug's events since its
/// latest `Created`, oldest first). Empty when the log already says it.
///
/// The first observation of a worktree records its current facts once, as a
/// baseline dated when wt first saw them: an open pull request, a failing
/// check, a conflict. It never reconstructs earlier transitions.
pub fn transitions(history: &[WorktreeEvent], observed: &ObservedRow) -> Vec<WorktreeEventKind> {
    let mut log: Vec<WorktreeEventKind> = history.iter().map(|event| event.event.clone()).collect();
    let known = log.len();
    if let Some(pr) = &observed.pr {
        observe_pr(&mut log, pr);
    }
    match (observed.conflicted, latest(&log, 0, conflict)) {
        (Some(true), previous) if previous != Some(false) => {
            log.push(WorktreeEventKind::Conflict { resolved: false });
        }
        // A clean branch is news only after a conflict.
        (Some(false), Some(false)) => log.push(WorktreeEventKind::Conflict { resolved: true }),
        _ => {}
    }
    if let Some(on) = observed.landed
        && latest(&log, 0, |event| match event {
            WorktreeEventKind::Landed { on } => Some(*on),
            _ => None,
        }) != Some(on)
    {
        log.push(WorktreeEventKind::Landed { on });
    }
    log.split_off(known)
}

/// The status event the log is missing, with the time it should carry.
///
/// Status writes record their own event, but only in builds that have the
/// log. Sessions keep the build that started them for days, and an agent's
/// `wt status` from such a session changes the state without an event. So
/// the observer compares the stored status with what the slug's latest
/// status event says, and records the difference: dated at the record's own
/// `at`, by its author. A status that matches the log records nothing, which
/// is also why a write that did log its event is never recorded twice. The
/// first observation of a worktree with a status and no status event
/// records it once.
///
/// `record` must be the status read in the same transaction as `history`.
pub fn status_transition(
    history: &[WorktreeEvent],
    record: Option<&wt_store::WorkStatusRecord>,
    now_ms: u64,
) -> Option<(Option<u64>, WorktreeEventKind)> {
    let logged = history.iter().rev().find_map(|event| match &event.event {
        WorktreeEventKind::Status {
            to,
            risk,
            blocked_on,
            verify_in,
            ..
        } => Some(Some((*to, *risk, blocked_on.clone(), verify_in.clone()))),
        WorktreeEventKind::StatusCleared { .. } => Some(None),
        _ => None,
    });
    let logged = logged.flatten();
    let Some(record) = record else {
        // Who cleared it, and when, was never stored.
        let (from, ..) = logged?;
        return Some((
            None,
            WorktreeEventKind::StatusCleared {
                from: Some(from),
                by: None,
            },
        ));
    };
    let event = wt_store::work_status_event(logged.as_ref().map(|(state, ..)| *state), record)?;
    let WorktreeEventKind::Status {
        to,
        risk,
        blocked_on,
        verify_in,
        ..
    } = &event
    else {
        return None;
    };
    if logged == Some((*to, *risk, blocked_on.clone(), verify_in.clone())) {
        return None;
    }
    let at =
        time::OffsetDateTime::parse(&record.at, &time::format_description::well_known::Rfc3339)
            .ok()
            .and_then(|at| u64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok());
    // A status older than anything the log keeps would be pruned by the
    // next append and recorded again after it, forever.
    let kept_from = now_ms.saturating_sub(wt_store::WORKTREE_EVENT_MAX_AGE_MS as u64);
    if at.is_some_and(|at| at < kept_from) {
        return None;
    }
    Some((at, event))
}

fn conflict(event: &WorktreeEventKind) -> Option<bool> {
    match event {
        WorktreeEventKind::Conflict { resolved } => Some(*resolved),
        _ => None,
    }
}

/// The newest value `pick` finds at or after `from`.
fn latest<T>(
    log: &[WorktreeEventKind],
    from: usize,
    pick: impl Fn(&WorktreeEventKind) -> Option<T>,
) -> Option<T> {
    log[from.min(log.len())..].iter().rev().find_map(pick)
}

fn observe_pr(log: &mut Vec<WorktreeEventKind>, pr: &ObservedPr) {
    // Two steps cover the longest path: a closed pull request reopened as a
    // draft.
    for _ in 0..2 {
        let recorded = latest(log, 0, |event| match event {
            WorktreeEventKind::Pr { number, change } => Some((*number, *change)),
            _ => None,
        });
        let change = match recorded {
            Some((number, change)) if number == pr.number => {
                match (phase_after(change), pr.phase) {
                    (was, now) if was == now => break,
                    (_, PrPhase::Merged) => PrChange::Merged,
                    (_, PrPhase::Closed) => PrChange::Closed,
                    (PrPhase::Draft, PrPhase::Open) => PrChange::ReadyForReview,
                    (PrPhase::Open, PrPhase::Draft) => PrChange::BackToDraft,
                    (_, PrPhase::Open | PrPhase::Draft) => PrChange::Reopened,
                }
            }
            _ => match pr.phase {
                PrPhase::Draft => PrChange::OpenedDraft,
                PrPhase::Open => PrChange::Opened,
                PrPhase::Closed => PrChange::Closed,
                PrPhase::Merged => PrChange::Merged,
            },
        };
        log.push(WorktreeEventKind::Pr {
            number: pr.number,
            change,
        });
    }
    // Verdicts recorded for an earlier pull request of this branch say
    // nothing about this one.
    let other = log
        .iter()
        .rposition(
            |event| matches!(event, WorktreeEventKind::Pr { number, .. } if *number != pr.number),
        )
        .map_or(0, |other| other + 1);
    let since = log[other..]
        .iter()
        .position(|event| matches!(event, WorktreeEventKind::Pr { .. }))
        .map_or(log.len(), |first| other + first);
    let queued = latest(log, since, |event| match event {
        WorktreeEventKind::MergeQueue { change } => Some(*change == QueueChange::Queued),
        _ => None,
    }) == Some(true);
    if pr.queued == Some(true) && !queued {
        log.push(WorktreeEventKind::MergeQueue {
            change: QueueChange::Queued,
        });
    } else if pr.queued == Some(false) && queued && pr.phase != PrPhase::Merged {
        log.push(WorktreeEventKind::MergeQueue {
            change: QueueChange::Left,
        });
    }
    if !matches!(pr.phase, PrPhase::Open | PrPhase::Draft) {
        return;
    }
    let checks = latest(log, since, |event| match event {
        WorktreeEventKind::Checks { state, failed } => Some((*state, failed.clone())),
        _ => None,
    });
    match &pr.checks {
        Some(ObservedChecks::Failing(names)) => {
            let mut failed = names.clone();
            failed.sort();
            failed.dedup();
            // Compared as it would be stored, or a long list of names would
            // never equal its own record.
            let mut failing = WorktreeEventKind::Checks {
                state: ChecksChange::Failing,
                failed,
            };
            wt_store::bound_worktree_event(&mut failing);
            let recorded =
                checks.map(|(state, failed)| WorktreeEventKind::Checks { state, failed });
            if recorded.as_ref() != Some(&failing) {
                log.push(failing);
            }
        }
        // Passing is the first verdict or a recovery, never a repeat.
        Some(ObservedChecks::Passing)
            if checks.is_none_or(|(state, _)| state == ChecksChange::Failing) =>
        {
            log.push(WorktreeEventKind::Checks {
                state: ChecksChange::Passing,
                failed: Vec::new(),
            });
        }
        _ => {}
    }
    for (reviewer, verdict) in [
        pr.bot_review
            .as_ref()
            .map(|(name, verdict)| (Some(name.clone()), *verdict)),
        pr.people_review.map(|verdict| (None, verdict)),
    ]
    .into_iter()
    .flatten()
    {
        let bot = reviewer.is_some();
        let previous = latest(log, since, |event| match event {
            WorktreeEventKind::Review { reviewer, change } if reviewer.is_some() == bot => {
                Some(*change)
            }
            _ => None,
        });
        if previous != Some(verdict) {
            log.push(WorktreeEventKind::Review {
                reviewer,
                change: verdict,
            });
        }
    }
}

fn phase_after(change: PrChange) -> PrPhase {
    match change {
        PrChange::Opened | PrChange::ReadyForReview | PrChange::Reopened => PrPhase::Open,
        PrChange::OpenedDraft | PrChange::BackToDraft => PrPhase::Draft,
        PrChange::Closed => PrPhase::Closed,
        PrChange::Merged => PrPhase::Merged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, phase: PrPhase) -> ObservedPr {
        ObservedPr {
            number,
            phase,
            checks: None,
            bot_review: None,
            people_review: None,
            queued: Some(false),
        }
    }

    /// Observe, append what it adds, and return what it added.
    fn observe(history: &mut Vec<WorktreeEvent>, row: &ObservedRow) -> Vec<WorktreeEventKind> {
        let added = transitions(history, row);
        history.extend(
            added
                .iter()
                .cloned()
                .map(|event| WorktreeEvent { at_ms: 1, event }),
        );
        assert!(
            transitions(history, row).is_empty(),
            "observing the same facts again adds nothing"
        );
        added
    }

    fn with_pr(pr: ObservedPr) -> ObservedRow {
        ObservedRow {
            pr: Some(pr),
            ..ObservedRow::default()
        }
    }

    #[test]
    fn the_first_observation_records_the_current_facts_once_as_a_baseline() {
        let mut history = Vec::new();
        let row = ObservedRow {
            pr: Some(ObservedPr {
                checks: Some(ObservedChecks::Failing(vec!["test".into(), "lint".into()])),
                bot_review: Some(("Reviewer".into(), ReviewChange::Findings { count: 2 })),
                people_review: Some(ReviewChange::Approved),
                queued: Some(true),
                ..pr(7, PrPhase::Open)
            }),
            conflicted: Some(true),
            landed: None,
            work: None,
        };
        assert_eq!(
            observe(&mut history, &row),
            vec![
                WorktreeEventKind::Pr {
                    number: 7,
                    change: PrChange::Opened
                },
                WorktreeEventKind::MergeQueue {
                    change: QueueChange::Queued
                },
                WorktreeEventKind::Checks {
                    state: ChecksChange::Failing,
                    failed: vec!["lint".into(), "test".into()]
                },
                WorktreeEventKind::Review {
                    reviewer: Some("Reviewer".into()),
                    change: ReviewChange::Findings { count: 2 }
                },
                WorktreeEventKind::Review {
                    reviewer: None,
                    change: ReviewChange::Approved
                },
                WorktreeEventKind::Conflict { resolved: false },
            ]
        );
        // Nothing is known: nothing is recorded, and nothing is retracted.
        assert!(transitions(&history, &ObservedRow::default()).is_empty());
        // A clean branch with no pull request has no history to invent.
        let quiet = ObservedRow {
            conflicted: Some(false),
            ..ObservedRow::default()
        };
        assert!(transitions(&[], &quiet).is_empty());
    }

    #[test]
    fn a_pull_request_moves_through_its_states_one_event_at_a_time() {
        let mut history = Vec::new();
        let change = |history: &mut Vec<WorktreeEvent>, phase| {
            observe(history, &with_pr(pr(7, phase)))
                .into_iter()
                .map(|event| match event {
                    WorktreeEventKind::Pr { change, .. } => change,
                    other => panic!("unexpected {other:?}"),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            change(&mut history, PrPhase::Draft),
            [PrChange::OpenedDraft]
        );
        assert_eq!(
            change(&mut history, PrPhase::Open),
            [PrChange::ReadyForReview]
        );
        assert_eq!(
            change(&mut history, PrPhase::Draft),
            [PrChange::BackToDraft]
        );
        assert_eq!(change(&mut history, PrPhase::Closed), [PrChange::Closed]);
        assert_eq!(
            change(&mut history, PrPhase::Draft),
            [PrChange::Reopened, PrChange::BackToDraft]
        );
        assert_eq!(change(&mut history, PrPhase::Merged), [PrChange::Merged]);
        // A new pull request for the branch starts over.
        assert_eq!(
            observe(&mut history, &with_pr(pr(8, PrPhase::Open))),
            [WorktreeEventKind::Pr {
                number: 8,
                change: PrChange::Opened
            }]
        );
    }

    #[test]
    fn passing_checks_are_recorded_first_or_after_a_failure_only() {
        let mut history = Vec::new();
        let checks = |history: &mut Vec<WorktreeEvent>, number, checks| {
            observe(
                history,
                &with_pr(ObservedPr {
                    checks,
                    ..pr(number, PrPhase::Open)
                }),
            )
            .into_iter()
            .filter_map(|event| match event {
                WorktreeEventKind::Checks { state, failed } => Some((state, failed)),
                _ => None,
            })
            .collect::<Vec<_>>()
        };
        let failing = |names: &[&str]| {
            Some(ObservedChecks::Failing(
                names.iter().map(|name| (*name).to_owned()).collect(),
            ))
        };
        assert!(
            checks(&mut history, 7, None).is_empty(),
            "running is not a verdict"
        );
        assert_eq!(
            checks(&mut history, 7, Some(ObservedChecks::Passing)),
            [(ChecksChange::Passing, vec![])]
        );
        assert!(checks(&mut history, 7, None).is_empty());
        assert!(checks(&mut history, 7, Some(ObservedChecks::Passing)).is_empty());
        assert_eq!(
            checks(&mut history, 7, failing(&["test"])),
            [(ChecksChange::Failing, vec!["test".to_owned()])]
        );
        assert_eq!(
            checks(&mut history, 7, failing(&["test", "lint"])),
            [(
                ChecksChange::Failing,
                vec!["lint".to_owned(), "test".to_owned()]
            )]
        );
        assert_eq!(
            checks(&mut history, 7, Some(ObservedChecks::Passing)),
            [(ChecksChange::Passing, vec![])]
        );
        // The next pull request's first verdict is recorded although the
        // log's last check event already says passing.
        assert_eq!(
            checks(&mut history, 8, Some(ObservedChecks::Passing)),
            [(ChecksChange::Passing, vec![])]
        );
    }

    #[test]
    fn reviews_queue_conflicts_and_landings_record_changes_only() {
        let mut history = Vec::new();
        let review = |count| {
            with_pr(ObservedPr {
                bot_review: Some((
                    "Reviewer".into(),
                    if count == 0 {
                        ReviewChange::Clean
                    } else {
                        ReviewChange::Findings { count }
                    },
                )),
                ..pr(7, PrPhase::Open)
            })
        };
        assert_eq!(observe(&mut history, &review(3)).len(), 2);
        assert_eq!(
            observe(&mut history, &review(1)),
            [WorktreeEventKind::Review {
                reviewer: Some("Reviewer".into()),
                change: ReviewChange::Findings { count: 1 }
            }]
        );
        assert_eq!(observe(&mut history, &review(0)).len(), 1);
        // A person's verdict is tracked apart from the bot's.
        let requested = with_pr(ObservedPr {
            people_review: Some(ReviewChange::ChangesRequested),
            bot_review: Some(("Reviewer".into(), ReviewChange::Clean)),
            ..pr(7, PrPhase::Open)
        });
        assert_eq!(
            observe(&mut history, &requested),
            [WorktreeEventKind::Review {
                reviewer: None,
                change: ReviewChange::ChangesRequested
            }]
        );

        let queued = |queued: bool, phase| {
            with_pr(ObservedPr {
                queued: Some(queued),
                ..pr(7, phase)
            })
        };
        let unknown = |phase| {
            with_pr(ObservedPr {
                queued: None,
                ..pr(7, phase)
            })
        };
        assert_eq!(
            observe(&mut history, &queued(true, PrPhase::Open)),
            [WorktreeEventKind::MergeQueue {
                change: QueueChange::Queued
            }]
        );
        assert_eq!(
            observe(&mut history, &queued(false, PrPhase::Open)),
            [WorktreeEventKind::MergeQueue {
                change: QueueChange::Left
            }]
        );
        observe(&mut history, &queued(true, PrPhase::Open));
        // One page of a long queue without this pull request proves nothing.
        assert!(observe(&mut history, &unknown(PrPhase::Open)).is_empty());
        // Leaving the queue by merging is the merge, not a second event.
        assert_eq!(
            observe(&mut history, &queued(false, PrPhase::Merged)),
            [WorktreeEventKind::Pr {
                number: 7,
                change: PrChange::Merged
            }]
        );

        let git = |conflicted, landed| ObservedRow {
            pr: None,
            conflicted,
            landed,
            work: None,
        };
        assert_eq!(
            observe(&mut history, &git(Some(true), None)),
            [WorktreeEventKind::Conflict { resolved: false }]
        );
        assert!(observe(&mut history, &git(None, None)).is_empty());
        assert_eq!(
            observe(&mut history, &git(Some(false), Some(LandedOn::Base))),
            [
                WorktreeEventKind::Conflict { resolved: true },
                WorktreeEventKind::Landed { on: LandedOn::Base }
            ]
        );
        assert_eq!(
            observe(&mut history, &git(Some(false), Some(LandedOn::Production))),
            [WorktreeEventKind::Landed {
                on: LandedOn::Production
            }]
        );
    }

    #[test]
    fn a_long_list_of_failing_checks_equals_its_own_bounded_record() {
        let mut history = Vec::new();
        let names: Vec<String> = (0..20).map(|n| format!("check-{n:02}")).collect();
        let failing = with_pr(ObservedPr {
            checks: Some(ObservedChecks::Failing(names)),
            ..pr(7, PrPhase::Open)
        });
        let added = observe(&mut history, &failing);
        let WorktreeEventKind::Checks { failed, .. } = &added[1] else {
            panic!("{added:?}");
        };
        assert_eq!(failed.len(), wt_store::WORKTREE_EVENT_CHECKS_MAX + 1);
        assert_eq!(failed.last().unwrap(), "+14 more");
    }

    #[test]
    fn an_unknown_fact_keeps_the_last_known_one() {
        let known = ObservedRow {
            pr: Some(ObservedPr {
                queued: Some(true),
                checks: Some(ObservedChecks::Passing),
                ..pr(7, PrPhase::Open)
            }),
            conflicted: Some(false),
            landed: Some(LandedOn::Base),
            work: None,
        };
        // GitHub and the Git facts are both unavailable for a while.
        assert_eq!(ObservedRow::default().or_last_known(&known), known);
        // What is known replaces what was; only the unknown part is kept.
        let partial = ObservedRow {
            pr: Some(ObservedPr {
                queued: None,
                ..pr(7, PrPhase::Draft)
            }),
            conflicted: Some(true),
            landed: None,
            work: None,
        };
        let merged = partial.or_last_known(&known);
        let merged_pr = merged.pr.as_ref().unwrap();
        assert_eq!(
            (merged_pr.phase, merged_pr.queued, &merged_pr.checks),
            (PrPhase::Draft, Some(true), &None)
        );
        assert_eq!(
            (merged.conflicted, merged.landed),
            (Some(true), Some(LandedOn::Base))
        );
        // Another pull request's queue state is not carried over.
        let other = with_pr(ObservedPr {
            queued: None,
            ..pr(8, PrPhase::Open)
        });
        assert_eq!(other.or_last_known(&known).pr.unwrap().queued, None);
    }

    fn work(state: &str, at: &str) -> wt_store::WorkStatusRecord {
        wt_store::WorkStatusRecord {
            state: state.into(),
            at: at.into(),
            note: Some("first line\nsecond".into()),
            risk: Some("low".into()),
            sha: None,
            by: Some("one".into()),
            blocked_on: None,
            verify_after_merge: None,
            verify_in: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn a_status_the_log_lacks_is_recorded_from_the_record_and_a_match_is_not() {
        // 2026-11-01T00:00:00Z
        const AT: u64 = 1_793_491_200_000;
        let now = AT + 60_000;
        let ready = work("ready", "2026-11-01T00:00:00Z");
        let (at, event) = status_transition(&[], Some(&ready), now).unwrap();
        assert_eq!(at, Some(AT), "dated when the status was asserted");
        assert_eq!(
            event,
            WorktreeEventKind::Status {
                from: None,
                to: wt_core::WorkState::Ready,
                by: Some("one".into()),
                risk: Some(wt_core::WorkRisk::Low),
                note: Some("first line".into()),
                blocked_on: None,
                verify_in: None,
            }
        );
        let history = vec![WorktreeEvent { at_ms: AT, event }];
        assert_eq!(status_transition(&history, Some(&ready), now), None);
        // A new note alone is not a transition, as for a status write.
        let mut renoted = ready.clone();
        renoted.note = Some("still ready".into());
        renoted.at = "2026-11-01T00:00:30Z".into();
        assert_eq!(status_transition(&history, Some(&renoted), now), None);
        // A changed gate is, and it follows from the logged state.
        let mut gated = ready.clone();
        gated.blocked_on = Some("deploy".into());
        gated.at = "not a time".into();
        let (at, event) = status_transition(&history, Some(&gated), now).unwrap();
        assert_eq!(at, None, "an unreadable time falls back to now");
        assert!(matches!(
            event,
            WorktreeEventKind::Status { from: Some(wt_core::WorkState::Ready), blocked_on: Some(ref gate), .. }
                if gate == "deploy"
        ));
        // Cleared by a build that logs nothing.
        assert_eq!(
            status_transition(&history, None, now),
            Some((
                None,
                WorktreeEventKind::StatusCleared {
                    from: Some(wt_core::WorkState::Ready),
                    by: None
                }
            ))
        );
        let cleared = vec![
            history[0].clone(),
            WorktreeEvent {
                at_ms: AT + 1,
                event: WorktreeEventKind::StatusCleared {
                    from: None,
                    by: None,
                },
            },
        ];
        assert_eq!(status_transition(&cleared, None, now), None);
        assert_eq!(status_transition(&[], None, now), None);
        assert!(status_transition(&cleared, Some(&ready), now).is_some());
        // Older than the log keeps: recording it would repeat forever.
        let later = AT + wt_store::WORKTREE_EVENT_MAX_AGE_MS as u64 + 1;
        assert_eq!(status_transition(&[], Some(&ready), later), None);
        // A state from a newer build has no name here.
        assert_eq!(
            status_transition(&[], Some(&work("teleported", "x")), now),
            None
        );
    }
}
