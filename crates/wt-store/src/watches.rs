//! Watches in the durable state: `state["watches"]`, an array of
//! [`wt_core::watch::Watch`] records.
//!
//! Records are rewritten through their typed form, which carries keys this
//! build does not know. An entry this build cannot read at all is left
//! exactly as it is: it is listed nowhere and changed by nothing but an
//! explicit cancel of its id.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use wt_core::{
    WatchChange, WorktreeEventKind,
    watch::{
        OwnerRemoval, WATCHES_MAX, WATCHES_PER_SLUG_MAX, Watch, WatchAction, WatchActions,
        WatchCondition, WatchStamps,
    },
};

use crate::{RemovedWorktree, Store, StoreError, WtState, events::EventLog};

const KEY: &str = "watches";
const ID_ALPHABET: &[u8; 32] = b"abcdefghjkmnopqrstuvwxyz23456789";
const ID_LENGTH: usize = 6;
const CLEARED_GATE: &str = "clearedGate";

/// The gate text a watch has already cleared, for the wake that follows.
pub fn cleared_gate(watch: &Watch) -> Option<&str> {
    watch.extra.get(CLEARED_GATE)?.as_str()
}

/// A watch to set. The store assigns its id and its owner's incarnation.
#[derive(Clone, Debug, PartialEq)]
pub struct NewWatch {
    pub slug: String,
    pub by: String,
    /// The setter's local UTC offset, for text written about the watch later.
    pub utc_offset_minutes: i32,
    pub when: Vec<WatchCondition>,
    pub then: WatchActions,
}

/// Why [`Store::add_watch`] wrote nothing.
#[derive(Clone, Debug, PartialEq)]
pub enum WatchRefusal {
    NoConditions,
    NoActions,
    TooManyForSlug {
        slug: String,
    },
    TooMany,
    /// The watch would clear a gate the worktree does not have now.
    GateChanged {
        current: Option<String>,
    },
    /// The worktree the setter saw is not the one that has the slug now.
    OwnerChanged,
    /// This exact watch is already pending.
    AlreadySet {
        watch: Box<Watch>,
    },
}

impl std::fmt::Display for WatchRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConditions => f.write_str("a watch needs at least one condition"),
            Self::NoActions => {
                f.write_str("a watch needs something to do once its conditions hold")
            }
            Self::TooManyForSlug { slug } => write!(
                f,
                "{slug} already has {WATCHES_PER_SLUG_MAX} watches; cancel one first"
            ),
            Self::TooMany => write!(
                f,
                "this repository already has {WATCHES_MAX} watches; cancel one first"
            ),
            Self::GateChanged { current: None } => f.write_str("there is no gate to clear"),
            Self::GateChanged {
                current: Some(current),
            } => write!(f, "the gate now reads: {current}"),
            Self::OwnerChanged => f.write_str("the worktree was removed or created again"),
            Self::AlreadySet { watch } => write!(f, "watch {} already waits for that", watch.id),
        }
    }
}

impl std::error::Error for WatchRefusal {}

/// What [`Store::deliver_unblock`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnblockDelivery {
    Cleared,
    /// The watch is gone or no longer owes the gate: nothing was written.
    NotOwed,
    /// The gate reads something else; the watch was dropped.
    GateChanged,
}

/// Which watches [`Store::cancel_watches`] removes.
#[derive(Clone, Copy, Debug)]
pub enum WatchSelector<'a> {
    Ids(&'a [String]),
    /// Every watch on `slug` that `by` set.
    SetBy {
        slug: &'a str,
        by: &'a str,
    },
    /// The watch that clears `slug`'s gate.
    Gate {
        slug: &'a str,
    },
}

/// The watches in a state snapshot that this build can read.
pub fn stored_watches(state: &WtState) -> Vec<Watch> {
    entries(state).iter().filter_map(read).collect()
}

impl Store {
    /// Set a watch. A watch that clears the gate must name the gate text the
    /// worktree has at this moment, and replaces the pending one that would
    /// have cleared it; given no wake of its own, it keeps that one's.
    /// Refused as [`WatchRefusal::AlreadySet`] when the same setter already
    /// has a pending watch there with the same conditions and actions.
    pub fn add_watch(
        &mut self,
        new: NewWatch,
        now_ms: i64,
    ) -> Result<Result<Watch, WatchRefusal>, StoreError> {
        self.add_watch_checked(new, now_ms, None)
    }

    /// Set a watch from a menu that showed the worktree as `incarnation`
    /// (its `createdAt`). Refused when the slug names another worktree now.
    pub fn add_watch_seen(
        &mut self,
        new: NewWatch,
        now_ms: i64,
        incarnation: Option<&str>,
    ) -> Result<Result<Watch, WatchRefusal>, StoreError> {
        self.add_watch_checked(new, now_ms, Some(incarnation))
    }

    fn add_watch_checked(
        &mut self,
        mut new: NewWatch,
        now_ms: i64,
        seen: Option<Option<&str>>,
    ) -> Result<Result<Watch, WatchRefusal>, StoreError> {
        let candidates = candidate_ids()?;
        self.mutate_wt_state_logged(|state, log| {
            let refuse = |refusal| Ok((Err(refusal), false));
            if new.when.is_empty() {
                return refuse(WatchRefusal::NoConditions);
            }
            if new.then.is_empty() {
                return refuse(WatchRefusal::NoActions);
            }
            if let Some(incarnation) = seen
                && created_at(state, &new.slug) != incarnation
            {
                return refuse(WatchRefusal::OwnerChanged);
            }
            if new.then.unblock.is_some() && new.then.wake.is_none() {
                new.then.wake = entries(state)
                    .iter()
                    .filter(|entry| pending_gate(entry, &new.slug) == new.then.unblock.as_deref())
                    .filter_map(read)
                    .find_map(|replaced| replaced.then.wake);
            }
            let incarnation = created_at(state, &new.slug);
            let same = entries(state).iter().filter_map(read).find(|watch| {
                watch.slug == new.slug
                    && watch.by == new.by
                    && !watch.owner_gone
                    && watch.owned_by(incarnation)
                    && watch.then == new.then
                    && watch
                        .when
                        .iter()
                        .map(|cond| (&cond.cond, cond.delay))
                        .eq(new.when.iter().map(|cond| (&cond.cond, cond.delay)))
            });
            if let Some(same) = same {
                return refuse(WatchRefusal::AlreadySet {
                    watch: Box::new(same),
                });
            }
            let current = gate(state, &new.slug);
            if new.then.unblock.is_some() && new.then.unblock.as_deref() != current {
                return refuse(WatchRefusal::GateChanged {
                    current: current.map(str::to_owned),
                });
            }
            let (slug, clears_gate) = (new.slug.clone(), new.then.unblock.is_some());
            let replaces = move |entry: &Value| clears_gate && pending_gate(entry, &slug).is_some();
            let staying = || entries(state).iter().filter(|entry| !replaces(entry));
            if staying()
                .filter(|entry| text(entry, "slug") == Some(new.slug.as_str()))
                .count()
                >= WATCHES_PER_SLUG_MAX
            {
                return refuse(WatchRefusal::TooManyForSlug {
                    slug: new.slug.clone(),
                });
            }
            if staying().count() >= WATCHES_MAX {
                return refuse(WatchRefusal::TooMany);
            }
            let unused = |id: &&String| {
                !entries(state)
                    .iter()
                    .any(|entry| text(entry, "id") == Some(id.as_str()))
            };
            let Some(id) = candidates.iter().find(unused) else {
                return Err(StoreError::Io(std::io::Error::other(
                    "could not choose an unused watch id",
                )));
            };
            let watch = Watch {
                id: id.clone(),
                incarnation: created_at(state, &new.slug).map(str::to_owned),
                slug: new.slug,
                by: new.by,
                created_at_ms: now_ms,
                utc_offset_minutes: new.utc_offset_minutes,
                when: new.when,
                then: new.then,
                met_at_ms: None,
                last_error: None,
                owner_gone: false,
                extra: Map::new(),
            };
            let replaced = remove_where(state, &replaces);
            entries_mut(state).push(serde_json::to_value(&watch)?);
            log.best_effort(|log| {
                crate::mutations::log_incarnation(log, state, &watch.slug)?;
                for replaced in replaced.iter().filter_map(read) {
                    record(
                        log,
                        &replaced,
                        WatchChange::Cancelled,
                        Some(&watch.by),
                        None,
                    )?;
                }
                record(log, &watch, WatchChange::Set, Some(&watch.by), None)
            });
            Ok((Ok(watch), true))
        })
    }

    /// The watches this build can read: all of them, or one worktree's.
    pub fn list_watches(&mut self, slug: Option<&str>) -> Result<Vec<Watch>, StoreError> {
        let mut watches = stored_watches(&self.read_wt_state()?);
        watches.retain(|watch| slug.is_none_or(|slug| watch.slug == slug));
        Ok(watches)
    }

    /// Remove watches, recording who cancelled them (`None` for the person).
    /// Returns the ones removed.
    pub fn cancel_watches(
        &mut self,
        which: WatchSelector<'_>,
        by: Option<&str>,
    ) -> Result<Vec<Watch>, StoreError> {
        self.mutate_wt_state_logged(|state, log| {
            let removed = remove_where(state, |entry| match which {
                WatchSelector::Ids(ids) => {
                    text(entry, "id").is_some_and(|id| ids.iter().any(|wanted| wanted == id))
                }
                WatchSelector::SetBy { slug, by } => {
                    text(entry, "slug") == Some(slug) && text(entry, "by") == Some(by)
                }
                WatchSelector::Gate { slug } => {
                    text(entry, "slug") == Some(slug) && entry["then"]["unblock"].is_string()
                }
            });
            let changed = !removed.is_empty();
            let cancelled: Vec<Watch> = removed.iter().filter_map(read).collect();
            log.best_effort(|log| {
                cancelled
                    .iter()
                    .try_for_each(|watch| record(log, watch, WatchChange::Cancelled, by, None))
            });
            Ok((cancelled, changed))
        })
    }

    /// Cancel one watch from a menu that showed it on the worktree
    /// `incarnation` names, set by `by`. `None` when that watch is no longer
    /// there to cancel: it fired, was cancelled, or its slug names another
    /// worktree now. Recorded as cancelled by the person.
    pub fn cancel_watch_seen(
        &mut self,
        id: &str,
        slug: &str,
        by: &str,
        incarnation: Option<&str>,
    ) -> Result<Option<Watch>, StoreError> {
        self.mutate_wt_state_logged(|state, log| {
            if created_at(state, slug) != incarnation {
                return Ok((None, false));
            }
            let removed = remove_where(state, |entry| {
                text(entry, "id") == Some(id)
                    && text(entry, "slug") == Some(slug)
                    && text(entry, "by") == Some(by)
            });
            let cancelled = removed.first().and_then(read);
            log.best_effort(|log| match &cancelled {
                Some(watch) => record(log, watch, WatchChange::Cancelled, None, None),
                None => Ok(()),
            });
            Ok((cancelled, !removed.is_empty()))
        })
    }

    /// Store what evaluations found. Writes only when a record changes, and
    /// returns whether it wrote. A watch met by this call gets its history
    /// event here, once.
    pub fn apply_evaluation(&mut self, stamps: &[WatchStamps]) -> Result<bool, StoreError> {
        if stamps.is_empty() {
            return Ok(false);
        }
        self.mutate_wt_state_logged(|state, log| {
            let mut met = Vec::new();
            let mut changed = false;
            for entry in stored_mut(state) {
                let Some(stamps) = stamps
                    .iter()
                    .find(|stamps| text(entry, "id") == Some(stamps.id.as_str()))
                else {
                    continue;
                };
                let Some(mut watch) = read(entry) else {
                    continue;
                };
                let applied = stamps.apply(&mut watch);
                if applied.changed {
                    *entry = serde_json::to_value(&watch)?;
                    changed = true;
                }
                if applied.newly_met {
                    met.push(watch);
                }
            }
            log.best_effort(|log| {
                met.iter()
                    .try_for_each(|watch| record(log, watch, WatchChange::Held, None, None))
            });
            Ok((changed, changed))
        })
    }

    /// Record that one action of a watch is done: it is no longer owed, and
    /// a watch that owes nothing more is deleted. Returns whether the action
    /// was still owed, so a second call changes nothing.
    pub fn complete_action(&mut self, id: &str, action: WatchAction) -> Result<bool, StoreError> {
        self.mutate_wt_state_logged(|state, log| {
            let Some(index) = entries(state)
                .iter()
                .position(|entry| text(entry, "id") == Some(id))
            else {
                return Ok((false, false));
            };
            let Some(mut watch) = read(&entries(state)[index]) else {
                return Ok((false, false));
            };
            let Some(text) = watch.then.take(action) else {
                return Ok((false, false));
            };
            // The wake that follows tells the session its gate is gone.
            if action == WatchAction::Unblock {
                watch
                    .extra
                    .insert(CLEARED_GATE.to_owned(), Value::String(text));
            }
            if watch.then.is_empty() {
                entries_mut(state).remove(index);
            } else {
                entries_mut(state)[index] = serde_json::to_value(&watch)?;
            }
            let change = match action {
                WatchAction::Unblock => WatchChange::ClearedGate,
                WatchAction::Notify => WatchChange::Notified,
                WatchAction::Wake => WatchChange::Woken,
            };
            log.best_effort(|log| record(log, &watch, change, None, None));
            Ok((true, true))
        })
    }

    /// Clear the gate a met watch was set to clear, in one transaction with
    /// the record that it did, so no crash leaves one without the other and
    /// a second call finds nothing owed. The gate is cleared only while it
    /// still reads the text the watch witnessed; state, risk, note, author
    /// and timestamp are kept. When it reads anything else the gate the
    /// watch belonged to is gone, and the whole watch goes with it, its
    /// wake included.
    pub fn deliver_unblock(&mut self, id: &str) -> Result<UnblockDelivery, StoreError> {
        self.mutate_wt_state_logged(|state, log| {
            let Some(index) = entries(state)
                .iter()
                .position(|entry| text(entry, "id") == Some(id))
            else {
                return Ok((UnblockDelivery::NotOwed, false));
            };
            let Some(mut watch) = read(&entries(state)[index]) else {
                return Ok((UnblockDelivery::NotOwed, false));
            };
            let Some(witness) = watch.then.unblock.clone() else {
                return Ok((UnblockDelivery::NotOwed, false));
            };
            if gate(state, &watch.slug) != Some(witness.as_str()) {
                entries_mut(state).remove(index);
                log.best_effort(|log| {
                    record(
                        log,
                        &watch,
                        WatchChange::Dropped,
                        None,
                        Some("the gate it was to clear was cleared or changed"),
                    )
                });
                return Ok((UnblockDelivery::GateChanged, true));
            }
            watch.then.unblock = None;
            // The wake that follows tells the session its gate is gone.
            watch
                .extra
                .insert(CLEARED_GATE.to_owned(), Value::String(witness));
            // Before the status write, which drops what still clears the
            // slug's gate.
            if watch.then.is_empty() {
                entries_mut(state).remove(index);
            } else {
                entries_mut(state)[index] = serde_json::to_value(&watch)?;
            }
            crate::mutations::clear_gate(state, log, &watch.slug)?;
            log.best_effort(|log| record(log, &watch, WatchChange::ClearedGate, None, None));
            Ok((UnblockDelivery::Cleared, true))
        })
    }

    /// Settle the watches whose worktree is gone. `live` maps every live
    /// worktree's slug to its `createdAt` and must come from a confirmed
    /// inventory: a slug missing from it is treated as removed.
    /// `landing_commits` gives, per removed slug, the commit that carried
    /// its work onto the base branch, where one is known.
    ///
    /// A watch that only tells the person outlives an owner whose removal
    /// record says it landed, with its own-row conditions rewritten onto the
    /// landing commit. Every other ownerless watch is dropped, with a
    /// history event saying why. Returns the dropped watches.
    pub fn drop_orphans(
        &mut self,
        live: &BTreeMap<String, Option<String>>,
        landing_commits: &BTreeMap<String, String>,
        now_ms: i64,
    ) -> Result<Vec<Watch>, StoreError> {
        self.mutate_wt_state_logged(|state, log| {
            let removed: Vec<RemovedWorktree> = state
                .get("removed")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|entry| serde_json::from_value(entry.clone()).ok())
                .collect();
            let mut dropped = Vec::new();
            let mut changed = false;
            let mut kept = Vec::new();
            for entry in entries(state) {
                let orphan = read(entry).filter(|watch| {
                    !watch.owner_gone
                        && !live
                            .get(&watch.slug)
                            .is_some_and(|created_at| watch.owned_by(created_at.as_deref()))
                });
                let Some(watch) = orphan else {
                    kept.push(entry.clone());
                    continue;
                };
                changed = true;
                // A record older than the watch is an earlier incarnation's.
                let landed = removed.iter().any(|record| {
                    record.slug == watch.slug
                        && wt_core::parse_iso_millis(&record.removed_at)
                            .is_some_and(|removed_at| removed_at >= watch.created_at_ms)
                        && crate::is_merged_removal(record)
                });
                let removal = OwnerRemoval {
                    landed,
                    landing_commit: landing_commits.get(&watch.slug).map(String::as_str),
                };
                match watch.without_owner(removal, now_ms) {
                    Ok(next) => kept.push(serde_json::to_value(&next)?),
                    Err(reason) => dropped.push((watch, reason)),
                }
            }
            if changed {
                *entries_mut(state) = kept;
            }
            log.best_effort(|log| {
                dropped
                    .iter()
                    // The slug's history now belongs to a later worktree.
                    .filter(|(watch, _)| !live.contains_key(&watch.slug))
                    .try_for_each(|(watch, reason)| {
                        record(
                            log,
                            watch,
                            WatchChange::Dropped,
                            None,
                            Some(reason.as_str()),
                        )
                    })
            });
            let dropped = dropped.into_iter().map(|(watch, _)| watch).collect();
            Ok((dropped, changed))
        })
    }
}

/// A removal is being recorded. When its record says the branch landed, the
/// owner's own `merged` conditions are held for good, so they stay true once
/// the row that showed it is gone.
pub(crate) fn owner_removed(state: &mut WtState, record: &Map<String, Value>, now_ms: i64) {
    let Ok(record) = serde_json::from_value::<RemovedWorktree>(Value::Object(record.clone()))
    else {
        return;
    };
    if !crate::is_merged_removal(&record) {
        return;
    }
    let incarnation = created_at(state, &record.slug).map(str::to_owned);
    let removed_at = wt_core::parse_iso_millis(&record.removed_at);
    for entry in stored_mut(state) {
        let Some(mut watch) = read(entry).filter(|watch| {
            watch.slug == record.slug
                && watch.owned_by(incarnation.as_deref())
                && removed_at.is_none_or(|removed_at| removed_at >= watch.created_at_ms)
        }) else {
            continue;
        };
        if watch.latch_own_merged(now_ms)
            && let Ok(next) = serde_json::to_value(&watch)
        {
            *entry = next;
        }
    }
}

/// A worktree's status was written: its gate read `previous` and now reads
/// `gate`. A watch that was to clear another gate text has nothing left to
/// clear. Neither has any watch set before the gate changed, met or not:
/// the new gate may read the same as one it witnessed earlier (cleared by a
/// build that cancels no watches, then set again), and is not its to clear.
/// Each goes whole, with the wake that was to follow its gate. Returns the
/// watches cancelled.
pub(crate) fn gate_written(
    state: &mut WtState,
    slug: &str,
    previous: Option<&str>,
    gate: Option<&str>,
) -> Vec<Watch> {
    remove_where(state, |entry| {
        text(entry, "slug") == Some(slug)
            && entry["then"]["unblock"]
                .as_str()
                .is_some_and(|witness| Some(witness) != gate || previous != gate)
    })
    .iter()
    .filter_map(read)
    .collect()
}

/// Record watches a status write cancelled, by whoever wrote the status.
pub(crate) fn log_cancelled(
    log: &EventLog<'_>,
    cancelled: &[Watch],
    by: Option<&str>,
) -> Result<(), StoreError> {
    cancelled
        .iter()
        .try_for_each(|watch| record(log, watch, WatchChange::Cancelled, by, None))
}

fn entries(state: &WtState) -> &[Value] {
    state
        .get(KEY)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// The stored entries, without creating the key in a state that has none.
fn stored_mut(state: &mut WtState) -> impl Iterator<Item = &mut Value> {
    state
        .get_mut(KEY)
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
}

fn entries_mut(state: &mut WtState) -> &mut Vec<Value> {
    if !state.get(KEY).is_some_and(Value::is_array) {
        state[KEY] = Value::Array(Vec::new());
    }
    state[KEY].as_array_mut().expect("array shape established")
}

fn remove_where(state: &mut WtState, remove: impl Fn(&Value) -> bool) -> Vec<Value> {
    let Some(entries) = state.get_mut(KEY).and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    let (removed, kept): (Vec<Value>, Vec<Value>) =
        std::mem::take(entries).into_iter().partition(&remove);
    *entries = kept;
    removed
}

fn read(entry: &Value) -> Option<Watch> {
    serde_json::from_value(entry.clone()).ok()
}

fn text<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    entry.get(key)?.as_str()
}

/// The gate text an unmet watch on `slug` is to clear.
fn pending_gate<'a>(entry: &'a Value, slug: &str) -> Option<&'a str> {
    (text(entry, "slug") == Some(slug) && entry["metAtMs"].is_null())
        .then(|| entry["then"]["unblock"].as_str())
        .flatten()
}

fn slug_field<'a>(state: &'a WtState, slug: &str, field: &[&str]) -> Option<&'a str> {
    field
        .iter()
        .try_fold(state.get("slugs")?.get(slug)?, |value, key| value.get(key))?
        .as_str()
}

fn created_at<'a>(state: &'a WtState, slug: &str) -> Option<&'a str> {
    slug_field(state, slug, &["createdAt"])
}

fn gate<'a>(state: &'a WtState, slug: &str) -> Option<&'a str> {
    slug_field(state, slug, &["work", "blockedOn"])
}

fn record(
    log: &EventLog<'_>,
    watch: &Watch,
    change: WatchChange,
    by: Option<&str>,
    reason: Option<&str>,
) -> Result<(), StoreError> {
    // The slug may name a new worktree by now; an outlived owner's watch is
    // not part of that one's history.
    if watch.owner_gone
        && log
            .lifecycle(&watch.slug)?
            .is_some_and(|latest| !latest.removed)
    {
        return Ok(());
    }
    log.append(
        &watch.slug,
        &WorktreeEventKind::Watch {
            id: Some(watch.id.clone()),
            change,
            what: watch.summary(),
            by: by
                .map(str::trim)
                .filter(|by| !by.is_empty() && *by != "human")
                .map(str::to_owned),
            reason: reason.map(str::to_owned),
            then: if change == WatchChange::Set {
                watch
                    .then
                    .remaining()
                    .map(|action| action.as_str().to_owned())
                    .collect()
            } else {
                Vec::new()
            },
        },
    )
}

/// Random ids to choose an unused one from. An id is typed by people and
/// names the watch's fires in the automation ledger after the watch is gone,
/// so it is short but not reused in practice.
fn candidate_ids() -> Result<Vec<String>, StoreError> {
    let mut bytes = [0u8; ID_LENGTH * 8];
    getrandom::fill(&mut bytes).map_err(|error| StoreError::Io(std::io::Error::other(error)))?;
    Ok(bytes
        .chunks(ID_LENGTH)
        .map(|chunk| {
            chunk
                .iter()
                .map(|byte| char::from(ID_ALPHABET[usize::from(byte & 31)]))
                .collect()
        })
        .collect())
}
