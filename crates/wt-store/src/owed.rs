//! What worktrees owe in the durable state, beyond their pending watches.
//!
//! `state["owed"]` is an array of [`wt_core::OwedRecord`]: steps that came
//! due when a wake was delivered to a worktree whose work had landed. They
//! are kept apart from `state["watches"]` because an older engine deletes a
//! watch that has no action left. Records are rewritten through their typed
//! form, which carries keys this build does not know; an entry this build
//! cannot read is left exactly as it is.
//!
//! When a worktree is removed its due steps move onto its removal record
//! (`removed[].owed`) and its checks onto `removed[].checks`, where they
//! can still be closed, and go when that record ages out. An older build
//! does not know either key and removes a row whatever it owes; this is
//! what keeps the obligation.
//!
//! The per-stage check operations are here too: closing, dropping and
//! putting off a check, on a live row and on a removed one. Each writes its
//! own history event.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use wt_core::{
    Check, Checks, Landing, OWED_PER_SLUG_MAX, OwedChange, OwedRecord, VerifyStage,
    WorktreeEventKind,
    watch::{Watch, WatchActions, WatchCondition},
};

use crate::{
    NewWatch, RemovedWorktree, Store, StoreError, WatchRefusal, WorkStatusRecord, WtState,
    events::EventLog,
    mutations::{array_mut, log_incarnation, log_status, stored_work_status, write_work_status},
    types::{CHECKS, OWED, store_checks},
};

const KEY: &str = "owed";

/// The keys of a removal record that say what the row owes. Only the
/// operations here write them.
pub(crate) const REMOVED_OWNED: [&str; 3] = [CHECKS, crate::types::CHECKS_MIRROR, OWED];

/// The due steps in a state snapshot that this build can read. Steps on
/// removal records are not among them; see [`RemovedWorktree::owed`].
pub fn stored_owed(state: &WtState) -> Vec<OwedRecord> {
    entries(state).iter().filter_map(read).collect()
}

/// Which of a row's checks to close.
#[derive(Clone, Debug, PartialEq)]
pub enum CheckTargets {
    /// What `wt status verified` closes: every check that is due under
    /// this landing at this time, or the earliest stage's when none is.
    Verified { landing: Landing, now_ms: i64 },
    /// The earliest stage's check.
    Earliest,
    /// Every check.
    All,
    /// The checks of these stages. One that is not owed is skipped.
    Stages(Vec<VerifyStage>),
}

impl CheckTargets {
    fn resolve(&self, record: &WorkStatusRecord) -> Vec<VerifyStage> {
        let open = || record.open_checks().into_iter().map(|(stage, _)| stage);
        match self {
            Self::Verified { landing, now_ms } => {
                wt_core::checks_to_close(record.to_core().as_ref(), *landing, *now_ms)
            }
            Self::Earliest => open().take(1).collect(),
            Self::All => open().collect(),
            Self::Stages(stages) => open().filter(|stage| stages.contains(stage)).collect(),
        }
    }
}

/// What [`Store::close_checks`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum ChecksClosed {
    /// The worktree has no status.
    Missing,
    /// None of the named checks is owed: nothing was written.
    NotOwed,
    Closed {
        closed: Vec<(VerifyStage, Check)>,
        /// What the row still owes. Empty means its status was replaced by
        /// the verified one.
        remaining: Vec<(VerifyStage, Check)>,
        /// The status as it now stands.
        record: Box<WorkStatusRecord>,
        /// Watches cancelled with a gate the verified status replaced.
        cancelled_watches: Vec<Watch>,
    },
}

/// What [`Store::close_removed_checks`] did.
#[derive(Clone, Debug, PartialEq)]
pub enum RemovedChecksClosed {
    /// No retained removal record has this slug.
    Missing,
    /// The record owes none of the named checks: nothing was written.
    NotOwed,
    Closed {
        closed: Vec<(VerifyStage, Check)>,
        remaining: Vec<(VerifyStage, Check)>,
    },
}

/// What became of a delivered wake; see [`Store::complete_wake`].
#[derive(Clone, Debug, PartialEq)]
pub enum WakeCompletion {
    /// The watch is gone or owes no wake: nothing was written.
    NotOwed,
    /// The worktree had not landed: delivering the wake closed it.
    Closed,
    /// The worktree had landed, or may have: the wake is now this due step.
    Due(Box<OwedRecord>),
}

/// Where a due step was closed.
#[derive(Clone, Debug, PartialEq)]
pub struct OwedClosed {
    pub step: OwedRecord,
    /// It was on the removal record of a worktree that is gone.
    pub removed: bool,
}

impl Store {
    /// The due steps this build can read: all of them, or one worktree's.
    /// A slug's steps may belong to an earlier worktree of that name until
    /// the engine settles them; filter with [`wt_core::owed::row_due`].
    pub fn list_owed(&mut self, slug: Option<&str>) -> Result<Vec<OwedRecord>, StoreError> {
        let mut owed = stored_owed(&self.read_wt_state()?);
        owed.retain(|step| slug.is_none_or(|slug| step.slug == slug));
        Ok(owed)
    }

    /// Close a due step with evidence, recording who closed it (`None` for
    /// the person). The step is looked for on the live worktree `slug` and
    /// then on its removal record. Returns the step, or `None` when `slug`
    /// has no due step with this id.
    pub fn close_owed(
        &mut self,
        slug: &str,
        id: &str,
        evidence: &str,
        by: Option<&str>,
    ) -> Result<Option<OwedClosed>, StoreError> {
        self.mutate_wt_state_logged(|state, log| {
            let is_step =
                |entry: &Value| text(entry, "id") == Some(id) && text(entry, "slug") == Some(slug);
            let live = take_where(state, is_step).first().and_then(read);
            let (step, removed) = match live {
                Some(step) => (step, false),
                None => match take_removed_step(state, slug, id) {
                    Some(step) => (step, true),
                    None => return Ok((None, false)),
                },
            };
            log.best_effort(|log| {
                if removed && superseded(log, slug)? {
                    return Ok(());
                }
                record_step(log, &step, OwedChange::Done, Some(evidence), by, None)
            });
            Ok((Some(OwedClosed { step, removed }), true))
        })
    }

    /// Put a due step off instead of closing it: it becomes a wake watch
    /// again, with the same prompt and these conditions, set by `by`. The
    /// step and the watch change in one transaction. `None` when the live
    /// worktree `slug` has no due step with this id; a step on a removal
    /// record has no session left to wake and cannot be set again.
    pub fn rearm_owed(
        &mut self,
        slug: &str,
        id: &str,
        when: Vec<WatchCondition>,
        by: &str,
        utc_offset_minutes: i32,
        now_ms: i64,
    ) -> Result<Option<Result<Watch, WatchRefusal>>, StoreError> {
        let candidates = crate::watches::candidate_ids()?;
        self.mutate_wt_state_logged(|state, log| {
            let Some(step) = entries(state)
                .iter()
                .filter(|entry| text(entry, "id") == Some(id) && text(entry, "slug") == Some(slug))
                .find_map(read)
            else {
                return Ok((None, false));
            };
            let new = NewWatch {
                slug: slug.to_owned(),
                by: by.to_owned(),
                utc_offset_minutes,
                when,
                then: WatchActions {
                    wake: Some(step.what.clone()),
                    ..WatchActions::default()
                },
                before_release: step.before_release,
            };
            let watch =
                match crate::watches::insert_watch(state, log, new, now_ms, None, &candidates)? {
                    Ok(watch) => watch,
                    Err(refusal) => return Ok((Some(Err(refusal)), false)),
                };
            take_where(state, |entry| {
                text(entry, "id") == Some(id) && text(entry, "slug") == Some(slug)
            });
            log.best_effort(|log| {
                let when = watch.when_summary();
                record_step(log, &step, OwedChange::Again, None, Some(by), Some(&when))
            });
            Ok((Some(Ok(watch)), true))
        })
    }

    /// Attach due steps to the worktree `slug` as it exists now: what a
    /// restore does with the steps its removal record held. Each becomes
    /// the current worktree's own.
    pub fn restore_owed(&mut self, slug: &str, steps: &[OwedRecord]) -> Result<bool, StoreError> {
        if steps.is_empty() {
            return Ok(false);
        }
        self.mutate_wt_state(|state| {
            let incarnation = created_at(state, slug).map(str::to_owned);
            for step in steps {
                let mut step = step.clone();
                step.slug = slug.to_owned();
                step.incarnation.clone_from(&incarnation);
                take_where(state, |entry| text(entry, "id") == Some(step.id.as_str()));
                entries_mut(state).push(serde_json::to_value(&step)?);
            }
            Ok((true, true))
        })
    }

    /// Close checks a live worktree owes, with the evidence for it. Once
    /// no check remains the status becomes `verified` (which must be a
    /// verified status; its author is who closed the checks); while one
    /// remains the state, note and time are left as they were. One
    /// transaction, with a history event per closed check.
    pub fn close_checks(
        &mut self,
        slug: &str,
        targets: &CheckTargets,
        verified: &WorkStatusRecord,
        evidence: Option<&str>,
    ) -> Result<ChecksClosed, StoreError> {
        let closed = self.close_checks_seen(slug, targets, verified, evidence, |_| true)?;
        Ok(closed.unwrap_or(ChecksClosed::Missing))
    }

    /// [`Self::close_checks`] for a caller that showed someone the status
    /// first. `admit` is asked about the status as it stands (`None` when
    /// the worktree has none), under the write's own transaction; when it
    /// refuses, nothing is written and `None` comes back.
    pub fn close_checks_seen(
        &mut self,
        slug: &str,
        targets: &CheckTargets,
        verified: &WorkStatusRecord,
        evidence: Option<&str>,
        admit: impl FnOnce(Option<&WorkStatusRecord>) -> bool,
    ) -> Result<Option<ChecksClosed>, StoreError> {
        self.mutate_wt_state_logged(|state, log| {
            let previous = stored_work_status(state, slug);
            if !admit(previous.as_ref()) {
                return Ok((None, false));
            }
            let closed = close_in(state, log, slug, previous, targets, verified, evidence)?;
            let changed = matches!(closed, ChecksClosed::Closed { .. });
            Ok((Some(closed), changed))
        })
    }

    /// Stop owing a live worktree's check in `stage` without doing it,
    /// recording who dropped it (`None` for the person). Returns the check,
    /// or `None` when none was owed there.
    pub fn drop_check(
        &mut self,
        slug: &str,
        stage: VerifyStage,
        by: Option<&str>,
    ) -> Result<Option<Check>, StoreError> {
        self.edit_check(slug, stage, by, OwedChange::Dropped, None, |record| {
            record.drop_check(stage)
        })
    }

    /// Put a check off: it stays owed and is not due before
    /// `not_before_ms`. `when` is that time in words for the history
    /// (`at 17:00`). Works on a live worktree and on a removed row. Returns
    /// the check as it now stands, or `None` when none is owed there.
    pub fn defer_check(
        &mut self,
        slug: &str,
        stage: VerifyStage,
        not_before_ms: i64,
        when: &str,
        by: Option<&str>,
    ) -> Result<Option<Check>, StoreError> {
        let defer = |record: &mut WorkStatusRecord| {
            record
                .defer_check(stage, Some(not_before_ms))
                .then(|| record.open_check(stage))
                .flatten()
        };
        if let Some(check) =
            self.edit_check(slug, stage, by, OwedChange::Again, Some(when), defer)?
        {
            return Ok(Some(check));
        }
        self.mutate_wt_state_logged(|state, log| {
            let Some((entry, parsed)) = removed_entry(state, slug) else {
                return Ok((None, false));
            };
            let Some(mut work) = parsed.work_with_checks() else {
                return Ok((None, false));
            };
            let Some(check) = defer(&mut work) else {
                return Ok((None, false));
            };
            write_removed(entry, work);
            log.best_effort(|log| {
                if superseded(log, slug)? {
                    return Ok(());
                }
                record_check(
                    log,
                    slug,
                    stage,
                    &check,
                    OwedChange::Again,
                    None,
                    by,
                    Some(when),
                )
            });
            Ok((Some(check), true))
        })
    }

    fn edit_check(
        &mut self,
        slug: &str,
        stage: VerifyStage,
        by: Option<&str>,
        change: OwedChange,
        when: Option<&str>,
        edit: impl FnOnce(&mut WorkStatusRecord) -> Option<Check>,
    ) -> Result<Option<Check>, StoreError> {
        self.mutate_wt_state_logged(|state, log| {
            let Some(previous) = stored_work_status(state, slug) else {
                return Ok((None, false));
            };
            let mut next = previous.clone();
            let Some(check) = edit(&mut next) else {
                return Ok((None, false));
            };
            write_work_status(state, slug, Some(&previous), Some(&next))?;
            log.best_effort(|log| {
                log_incarnation(log, state, slug)?;
                record_check(log, slug, stage, &check, change, None, by, when)?;
                log_status(log, slug, Some(&previous), Some(&next), None)
            });
            Ok((Some(check), true))
        })
    }

    /// [`Self::close_checks`] for a removed row: its checks live on its
    /// removal record. Once none remains `verified` becomes the row's
    /// status, as a settled row has always read.
    pub fn close_removed_checks(
        &mut self,
        slug: &str,
        targets: &CheckTargets,
        verified: &WorkStatusRecord,
        evidence: Option<&str>,
    ) -> Result<RemovedChecksClosed, StoreError> {
        self.settle_removed(slug, targets, verified, evidence)
    }

    /// Close the targeted checks of a removed row. `outcome` becomes the
    /// row's status once no check remains; an outcome that is neither
    /// verified nor dropped becomes it at once and leaves the checks owed.
    pub(crate) fn settle_removed(
        &mut self,
        slug: &str,
        targets: &CheckTargets,
        outcome: &WorkStatusRecord,
        evidence: Option<&str>,
    ) -> Result<RemovedChecksClosed, StoreError> {
        self.mutate_wt_state_logged(|state, log| {
            let Some((entry, parsed)) = removed_entry(state, slug) else {
                return Ok((RemovedChecksClosed::Missing, false));
            };
            let Some(previous) = parsed
                .work_with_checks()
                .filter(|work| !work.open_checks().is_empty())
            else {
                return Ok((RemovedChecksClosed::NotOwed, false));
            };
            let mut next = previous.clone();
            let closed: Vec<(VerifyStage, Check)> = targets
                .resolve(&previous)
                .into_iter()
                .filter_map(|stage| Some((stage, next.drop_check(stage)?)))
                .collect();
            let restates = !outcome.is_terminal();
            if closed.is_empty() && !restates {
                return Ok((RemovedChecksClosed::NotOwed, false));
            }
            let remaining = next.open_checks();
            if remaining.is_empty() || restates {
                next = outcome.clone();
                next.set_checks(remaining.clone());
            }
            write_removed(entry, next.clone());
            let by = outcome.by.as_deref();
            let change = if outcome.state == "dropped" {
                OwedChange::Dropped
            } else {
                OwedChange::Done
            };
            log.best_effort(|log| {
                // The slug may name a new worktree by now. The outcome is
                // about the removed one, so it is recorded only while that
                // is still the slug's last incarnation.
                if superseded(log, slug)? {
                    return Ok(());
                }
                for (stage, check) in &closed {
                    record_check(log, slug, *stage, check, change, evidence, by, None)?;
                }
                log_status(log, slug, Some(&previous), Some(&next), None)
            });
            Ok((RemovedChecksClosed::Closed { closed, remaining }, true))
        })
    }
}

/// The checks of `slug`'s stored status that an older build discharged
/// since this build last wrote it; see [`wt_core::normalize_checks`].
pub(crate) fn discharged_by_an_older_build(
    state: &WtState,
    slug: &str,
) -> Vec<(VerifyStage, Check)> {
    state
        .get("slugs")
        .and_then(|slugs| slugs.get(slug)?.get("work"))
        .and_then(|work| serde_json::from_value::<WorkStatusRecord>(work.clone()).ok())
        .map(|mut record| record.normalize())
        .unwrap_or_default()
}

/// Record checks an older build discharged, which it could not: it knows
/// one check and no stages. Written with the first status this build
/// writes after it.
pub(crate) fn log_discharged(
    log: &EventLog<'_>,
    slug: &str,
    discharged: &[(VerifyStage, Check)],
) -> Result<(), StoreError> {
    discharged.iter().try_for_each(|(stage, check)| {
        record_check(log, slug, *stage, check, OwedChange::Done, None, None, None)
    })
}

/// The body of [`Store::close_checks`], inside its transaction.
fn close_in(
    state: &mut WtState,
    log: &EventLog<'_>,
    slug: &str,
    previous: Option<WorkStatusRecord>,
    targets: &CheckTargets,
    verified: &WorkStatusRecord,
    evidence: Option<&str>,
) -> Result<ChecksClosed, StoreError> {
    let Some(previous) = previous else {
        return Ok(ChecksClosed::Missing);
    };
    let mut next = previous.clone();
    let closed: Vec<(VerifyStage, Check)> = targets
        .resolve(&previous)
        .into_iter()
        .filter_map(|stage| Some((stage, next.drop_check(stage)?)))
        .collect();
    if closed.is_empty() {
        return Ok(ChecksClosed::NotOwed);
    }
    let remaining = next.open_checks();
    if remaining.is_empty() {
        next = verified.clone();
        next.clear_checks();
    }
    let written = write_work_status(state, slug, Some(&previous), Some(&next))?;
    let by = verified.by.as_deref();
    log.best_effort(|log| {
        log_incarnation(log, state, slug)?;
        for (stage, check) in &closed {
            record_check(
                log,
                slug,
                *stage,
                check,
                OwedChange::Done,
                evidence,
                by,
                None,
            )?;
        }
        log_status(log, slug, Some(&previous), Some(&next), None)?;
        crate::watches::log_cancelled(log, &written.cancelled_watches, by)
    });
    Ok(ChecksClosed::Closed {
        closed,
        remaining,
        record: Box::new(next),
        cancelled_watches: written.cancelled_watches,
    })
}

/// A removal is being recorded as `next`. Its status came from the live
/// row with its checks inside; they move beside it, where an older build
/// settling the row leaves them. Recording the same removal again changes
/// nothing.
///
/// `recorded` is the slug's earlier removal record when the status in
/// `next` was read from the live slug. Creating a worktree clears its
/// slug's removal record, so that record is this same worktree's: from the
/// first recording on a check may be closed on either copy, and neither
/// copy learns of the other's. A status not written since then therefore
/// owes only what both still owe; without this the stale copy's legacy
/// fields would bring a closed check back (normalize rule a). A status
/// written since is a fresh assertion and replaces what was recorded.
pub(crate) fn settle_removed_checks(next: &mut Map<String, Value>, recorded: Option<&Value>) {
    let Ok(parsed) = serde_json::from_value::<RemovedWorktree>(Value::Object(next.clone())) else {
        return;
    };
    let recorded = recorded
        .and_then(|entry| serde_json::from_value::<RemovedWorktree>(entry.clone()).ok())
        .filter(|recorded| recorded.extra.contains_key(CHECKS));
    if let (Some(recorded), Some(mut live)) = (recorded, parsed.work.clone()) {
        let written = |at: &str| wt_core::parse_iso_millis(at);
        if written(&live.at) > written(&recorded.removed_at) {
            next.remove(CHECKS);
            next.remove(crate::types::CHECKS_MIRROR);
            return settle_removed_checks(next, None);
        }
        live.normalize();
        let still: Vec<VerifyStage> = live.open_checks().iter().map(|(stage, _)| *stage).collect();
        let mut kept = recorded.open_checks();
        kept.retain(|(stage, _)| still.contains(stage));
        live.set_checks(kept);
        write_removed(next, live);
        return;
    }
    let Some(work) = parsed.work_with_checks() else {
        return;
    };
    let carried = parsed
        .work
        .as_ref()
        .is_some_and(|work| work.extra.contains_key(CHECKS));
    if work.open_checks().is_empty() && !carried && !next.contains_key(CHECKS) {
        return;
    }
    write_removed(next, work);
}

/// A removal is being recorded as `next`: the due steps of the worktree
/// that has the slug now move onto it.
pub(crate) fn move_to_removed(state: &mut WtState, next: &mut Map<String, Value>) {
    let Some(slug) = next.get("slug").and_then(Value::as_str).map(str::to_owned) else {
        return;
    };
    let incarnation = created_at(state, &slug).map(str::to_owned);
    let moved = take_where(state, |entry| {
        read(entry).is_some_and(|step| step.slug == slug && step.owned_by(incarnation.as_deref()))
    });
    attach(next, moved);
}

/// Settle the due steps whose worktree is gone; see
/// [`Store::drop_orphans`] for `live`. A step moves onto its worktree's
/// removal record when one is retained, and is dropped otherwise. Returns
/// the dropped steps and whether anything changed.
pub(crate) fn settle_orphans(
    state: &mut WtState,
    live: &BTreeMap<String, Option<String>>,
) -> (Vec<OwedRecord>, bool) {
    let orphans = take_where(state, |entry| {
        read(entry).is_some_and(|step| {
            !live
                .get(&step.slug)
                .is_some_and(|created_at| step.owned_by(created_at.as_deref()))
        })
    });
    let changed = !orphans.is_empty();
    let mut dropped = Vec::new();
    for orphan in orphans {
        let Some(step) = read(&orphan) else {
            continue;
        };
        // A record older than the step is an earlier incarnation's.
        let record = array_mut(state, "removed").iter_mut().find(|record| {
            record.get("slug").and_then(Value::as_str) == Some(step.slug.as_str())
                && record
                    .get("removedAt")
                    .and_then(Value::as_str)
                    .and_then(wt_core::parse_iso_millis)
                    .is_some_and(|removed_at| removed_at >= step.due_at_ms)
        });
        match record.and_then(Value::as_object_mut) {
            Some(record) => attach(record, vec![orphan]),
            None => dropped.push(step),
        }
    }
    (dropped, changed)
}

/// Record steps [`settle_orphans`] dropped.
pub(crate) fn log_dropped(
    log: &EventLog<'_>,
    dropped: &[OwedRecord],
    live: &BTreeMap<String, Option<String>>,
) -> Result<(), StoreError> {
    dropped
        .iter()
        // The slug's history now belongs to a later worktree.
        .filter(|step| !live.contains_key(&step.slug))
        .try_for_each(|step| {
            let reason = Some("its worktree was removed");
            record_step(log, step, OwedChange::Dropped, reason, None, None)
        })
}

/// A wake was delivered to a worktree that had landed: keep it as a due
/// step. A worktree at its limit loses its oldest step, which is returned.
pub(crate) fn push_due(
    state: &mut WtState,
    step: &OwedRecord,
) -> Result<Vec<OwedRecord>, StoreError> {
    take_where(state, |entry| text(entry, "id") == Some(step.id.as_str()));
    let mut held: Vec<OwedRecord> = entries(state)
        .iter()
        .filter_map(read)
        .filter(|held| held.slug == step.slug)
        .collect();
    held.sort_by_key(|held| held.due_at_ms);
    let excess = (held.len() + 1).saturating_sub(OWED_PER_SLUG_MAX);
    let evicted: Vec<OwedRecord> = held.into_iter().take(excess).collect();
    take_where(state, |entry| {
        evicted
            .iter()
            .any(|evicted| text(entry, "id") == Some(evicted.id.as_str()))
    });
    entries_mut(state).push(serde_json::to_value(step)?);
    Ok(evicted)
}

/// Record that a wake became the due step `step`, and any step it pushed
/// out.
pub(crate) fn log_due(
    log: &EventLog<'_>,
    step: &OwedRecord,
    evicted: &[OwedRecord],
) -> Result<(), StoreError> {
    for evicted in evicted {
        let reason = format!("more than {OWED_PER_SLUG_MAX} steps were due");
        record_step(log, evicted, OwedChange::Dropped, Some(&reason), None, None)?;
    }
    record_step(
        log,
        step,
        OwedChange::Due,
        None,
        None,
        Some(&step.when_text),
    )
}

fn attach(record: &mut Map<String, Value>, steps: Vec<Value>) {
    if steps.is_empty() {
        return;
    }
    let held = record
        .entry(OWED)
        .or_insert_with(|| Value::Array(Vec::new()));
    if !held.is_array() {
        *held = Value::Array(Vec::new());
    }
    let held = held.as_array_mut().expect("array shape established");
    for step in steps {
        held.retain(|existing| text(existing, "id") != text(&step, "id"));
        held.push(step);
    }
}

fn take_removed_step(state: &mut WtState, slug: &str, id: &str) -> Option<OwedRecord> {
    let (entry, _) = removed_entry(state, slug)?;
    let held = entry.get_mut(OWED)?.as_array_mut()?;
    let index = held.iter().position(|step| text(step, "id") == Some(id))?;
    let step = read(&held[index])?;
    held.remove(index);
    Some(step)
}

fn removed_entry<'a>(
    state: &'a mut WtState,
    slug: &str,
) -> Option<(&'a mut Map<String, Value>, RemovedWorktree)> {
    let entry = array_mut(state, "removed")
        .iter_mut()
        .find(|entry| entry.get("slug").and_then(Value::as_str) == Some(slug))?;
    let parsed = serde_json::from_value(entry.clone()).ok()?;
    Some((entry.as_object_mut()?, parsed))
}

/// Write a removed row's status, given as a live record with its checks
/// inside: the checks go beside the status, which keeps their mirror.
fn write_removed(entry: &mut Map<String, Value>, mut work: WorkStatusRecord) {
    work.normalize();
    let (checks, mirror) = crate::types::stored_checks(&work.extra);
    let (checks, mirror) = if work.is_terminal() {
        (Checks::default(), None)
    } else {
        (checks, mirror)
    };
    store_checks(&mut work.extra, &Checks::default(), None, false);
    store_checks(entry, &checks, mirror, true);
    if let Ok(work) = serde_json::to_value(&work) {
        entry.insert("work".to_owned(), work);
    }
}

/// Whether the slug's history belongs to a later worktree than the removed
/// one an event would be about.
fn superseded(log: &EventLog<'_>, slug: &str) -> Result<bool, StoreError> {
    Ok(log.lifecycle(slug)?.is_some_and(|latest| !latest.removed))
}

fn author(by: Option<&str>) -> Option<String> {
    by.map(str::trim)
        .filter(|by| !by.is_empty() && *by != "human")
        .map(str::to_owned)
}

fn record_step(
    log: &EventLog<'_>,
    step: &OwedRecord,
    change: OwedChange,
    evidence: Option<&str>,
    by: Option<&str>,
    when: Option<&str>,
) -> Result<(), StoreError> {
    let mut event = WorktreeEventKind::Owed {
        id: Some(step.id.clone()),
        change,
        what: wt_core::watch::headline(&step.what),
        evidence: evidence.map(str::to_owned),
        by: author(by),
        stage: None,
        when: when.map(str::to_owned),
        unconfirmed: change == OwedChange::Due && step.unconfirmed,
    };
    crate::bound_worktree_event(&mut event);
    log.append(&step.slug, &event)
}

#[allow(clippy::too_many_arguments)]
fn record_check(
    log: &EventLog<'_>,
    slug: &str,
    stage: VerifyStage,
    check: &Check,
    change: OwedChange,
    evidence: Option<&str>,
    by: Option<&str>,
    when: Option<&str>,
) -> Result<(), StoreError> {
    let mut event = WorktreeEventKind::Owed {
        id: None,
        change,
        what: wt_core::watch::headline(&check.steps),
        evidence: evidence.map(str::to_owned),
        by: author(by),
        stage: Some(stage.as_str().to_owned()),
        when: when.map(str::to_owned),
        unconfirmed: false,
    };
    crate::bound_worktree_event(&mut event);
    log.append(slug, &event)
}

fn entries(state: &WtState) -> &[Value] {
    state
        .get(KEY)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn entries_mut(state: &mut WtState) -> &mut Vec<Value> {
    if !state.get(KEY).is_some_and(Value::is_array) {
        state[KEY] = Value::Array(Vec::new());
    }
    state[KEY].as_array_mut().expect("array shape established")
}

/// Remove the entries `take` picks, without creating the key in a state
/// that has none.
fn take_where(state: &mut WtState, take: impl Fn(&Value) -> bool) -> Vec<Value> {
    let Some(entries) = state.get_mut(KEY).and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    let (taken, kept): (Vec<Value>, Vec<Value>) =
        std::mem::take(entries).into_iter().partition(&take);
    *entries = kept;
    taken
}

fn read(entry: &Value) -> Option<OwedRecord> {
    serde_json::from_value(entry.clone()).ok()
}

fn text<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    entry.get(key)?.as_str()
}

fn created_at<'a>(state: &'a WtState, slug: &str) -> Option<&'a str> {
    state.get("slugs")?.get(slug)?.get("createdAt")?.as_str()
}
