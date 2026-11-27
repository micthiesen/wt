//! Explicit maintenance of a repository's stored state: dropping the slug
//! records of worktrees that are long gone. A record outlives its worktree
//! on purpose (a restore reads its fork base, a pause is remembered on it),
//! so nothing here runs as a side effect of a read, and a record is dropped
//! only when every rule in [`prunable_slug_records`] holds.

use std::collections::BTreeSet;

use serde_json::Value;
use wt_core::{LandedOn, OwedChange, VerifyStage, WorktreeEvent, WorktreeEventKind};

use crate::{
    Store, StoreError, WtState,
    mutations::{stored_work_status, write_work_status},
};

/// How long a record must have gone unchanged before it may be dropped.
pub const SLUG_RECORD_MAX_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Names no worktree can take, whose sessions still have records.
const RESERVED: [&str; 4] = ["main", "manager", "wt", "dotfiles"];

/// How much of a worktree's history is read to date a check: more than any
/// row accumulates between a status that owes a check and its landing.
const HISTORY_READ: usize = 500;

/// Epoch milliseconds are told from durations and counters by their size:
/// this is 2001-09-09.
const EPOCH_MS_FLOOR: i64 = 1_000_000_000_000;

/// The slug records of `state` that may be dropped, given every slug the
/// inventory holds (`live`, the main clone's included). A record goes only
/// when all of these hold:
///
/// - its slug has no worktree, is not reserved and is not a remote row's;
/// - no removal record names it (the Landed block, cleanup and a restore of
///   the branch read those, and a restore then reads this record's base);
/// - nothing is owed on it: no check, gate or verification in its status,
///   no due step, no watch on it or set by it;
/// - its automations are not paused (the pause lives on this record);
/// - no live worktree's fork base names its branch;
/// - it says when it was created, and the newest time anywhere in it is
///   over [`SLUG_RECORD_MAX_AGE_MS`] old. A record mid-creation has no
///   `createdAt` yet, and is kept.
pub fn prunable_slug_records(state: &Value, live: &BTreeSet<String>, now_ms: i64) -> Vec<String> {
    let Some(slugs) = state.get("slugs").and_then(Value::as_object) else {
        return Vec::new();
    };
    let named = |key: &str, fields: &[&str]| -> BTreeSet<&str> {
        state
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|record| fields.iter().filter_map(|field| record.get(*field)))
            .filter_map(Value::as_str)
            .collect()
    };
    let removed = named("removed", &["slug"]);
    let owing = named("owed", &["slug", "by"]);
    let watched = named("watches", &["slug", "by"]);
    let removing = state.get("removals").and_then(Value::as_object);
    let live_bases: BTreeSet<&str> = live
        .iter()
        .filter_map(|slug| slugs.get(slug)?.get("baseBranch")?.as_str())
        .collect();
    slugs
        .iter()
        .filter(|(slug, record)| {
            let slug = slug.as_str();
            let based_on = |branch: &str| {
                live_bases
                    .iter()
                    .any(|base| *base == branch || base.rsplit('/').next() == Some(branch))
            };
            !live.contains(slug)
                && !RESERVED.contains(&slug)
                && !slug.starts_with("@remote/")
                && !removed.contains(slug)
                && !owing.contains(slug)
                && !watched.contains(slug)
                && !removing.is_some_and(|removals| removals.contains_key(slug))
                && !owes(record)
                && record.get("automationsPaused").and_then(Value::as_bool) != Some(true)
                && !based_on(slug)
                && !record
                    .get(wt_core::RECORDED_BRANCH)
                    .and_then(Value::as_str)
                    .is_some_and(based_on)
                && record
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .and_then(wt_core::parse_iso_millis)
                    .is_some()
                && newest_time(record)
                    .is_some_and(|at| now_ms.saturating_sub(at) > SLUG_RECORD_MAX_AGE_MS)
        })
        .map(|(slug, _)| slug.clone())
        .collect()
}

/// Whether the record's status still carries anything a person or an agent
/// has to do.
fn owes(record: &Value) -> bool {
    let Some(work) = record.get("work").and_then(Value::as_object) else {
        return false;
    };
    ["blockedOn", "verifyAfterMerge", "verifyIn", "checksMirror"]
        .iter()
        .any(|key| work.get(*key).is_some_and(|value| !value.is_null()))
        || work.get("checks").is_some_and(|checks| match checks {
            Value::Null => false,
            Value::Object(checks) => !checks.is_empty(),
            Value::Array(checks) => !checks.is_empty(),
            _ => true,
        })
}

/// The newest time recorded anywhere in `value`: every string that reads as a
/// timestamp and every epoch-millisecond number, whatever key holds it, so a
/// field added later counts without being listed here.
fn newest_time(value: &Value) -> Option<i64> {
    match value {
        Value::String(text) => wt_core::parse_iso_millis(text),
        Value::Number(number) => number.as_i64().filter(|at| *at >= EPOCH_MS_FLOOR),
        Value::Array(items) => items.iter().filter_map(newest_time).max(),
        Value::Object(fields) => fields.values().filter_map(newest_time).max(),
        Value::Null | Value::Bool(_) => None,
    }
}

impl Store {
    /// Drop the slug records [`prunable_slug_records`] allows, decided on
    /// the state as it is under the write lock, and return their slugs.
    /// `live` must come from an inventory read that succeeded: an unknown
    /// inventory proves no worktree gone.
    pub fn prune_slug_records(
        &mut self,
        live: &BTreeSet<String>,
        now_ms: i64,
    ) -> Result<Vec<String>, StoreError> {
        self.mutate_wt_state(|state| {
            let pruned = prunable_slug_records(state, live, now_ms);
            if let Some(slugs) = state.get_mut("slugs").and_then(Value::as_object_mut) {
                for slug in &pruned {
                    slugs.remove(slug);
                }
            }
            let changed = !pruned.is_empty();
            Ok((pruned, changed))
        })
    }
}

/// A check whose first-due time was moved back to what history shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedatedCheck {
    pub slug: String,
    pub stage: VerifyStage,
    pub from_ms: i64,
    pub to_ms: i64,
}

/// State key that says [`Store::redate_checks_from_history`] has run.
const REDATED: &str = "checksRedatedFromHistory";

/// When `events` (one worktree's history since it was created, oldest
/// first) say the production check it owes now came due: the last time the
/// work was seen in production at or after the check was last owed. `None`
/// where history cannot say: the check was closed, dropped or replaced
/// since, no status event owes it, or it was owed only after the work had
/// shipped.
fn production_due_in_history(events: &[WorktreeEvent]) -> Option<i64> {
    let stage = VerifyStage::Production.as_str();
    let mut owed_since = None;
    let mut landed = None;
    for event in events {
        let at = i64::try_from(event.at_ms).ok()?;
        match &event.event {
            WorktreeEventKind::Status { verify_in, .. } => {
                if verify_in.as_deref() == Some(stage) {
                    owed_since.get_or_insert(at);
                } else {
                    (owed_since, landed) = (None, None);
                }
            }
            WorktreeEventKind::StatusCleared { .. } => (owed_since, landed) = (None, None),
            WorktreeEventKind::Owed {
                stage: Some(closed),
                change: OwedChange::Done | OwedChange::Dropped,
                ..
            } if closed == stage => (owed_since, landed) = (None, None),
            WorktreeEventKind::Landed {
                on: LandedOn::Production,
                ..
            } if owed_since.is_some() => landed = Some(at),
            _ => {}
        }
    }
    landed
}

impl Store {
    /// Once for a repository: move the first-due time of production checks
    /// that were already due when wt began recording it back to when
    /// history shows they came due. Those checks carry no `sinceMs`, and
    /// were dated with the time the first build that dates checks started,
    /// which makes "due for" read short. A time is only moved earlier.
    ///
    /// A staging check is left alone: with a deploy probe it came due at
    /// the deploy, which history does not record. A later call returns
    /// nothing: the state records that this ran.
    pub fn redate_checks_from_history(&mut self) -> Result<Vec<RedatedCheck>, StoreError> {
        let state = self.read_wt_state()?;
        if state.get(REDATED).and_then(Value::as_bool) == Some(true) {
            return Ok(Vec::new());
        }
        let undated = |state: &WtState, slug: &str| -> Option<wt_core::Check> {
            stored_work_status(state, slug)?
                .open_checks()
                .into_iter()
                .find(|(stage, check)| {
                    *stage == VerifyStage::Production
                        && check.since_ms.is_none()
                        && check.due_since_ms.is_some()
                })
                .map(|(_, check)| check)
        };
        let slugs: Vec<String> = state
            .get("slugs")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|slugs| slugs.keys())
            .filter(|slug| undated(&state, slug).is_some())
            .cloned()
            .collect();
        let history = if slugs.is_empty() {
            Default::default()
        } else {
            self.read_worktree_events_for(&slugs, HISTORY_READ)?
        };
        let mut wanted = Vec::new();
        for slug in &slugs {
            if let Some(check) = undated(&state, slug)
                && let Some(from_ms) = check.due_since_ms
                && let Some(to_ms) = history
                    .get(slug)
                    .and_then(|events| production_due_in_history(events))
                && to_ms < from_ms
            {
                wanted.push((
                    check.steps,
                    RedatedCheck {
                        slug: slug.clone(),
                        stage: VerifyStage::Production,
                        from_ms,
                        to_ms,
                    },
                ));
            }
        }
        self.mutate_wt_state(|state| {
            if state.get(REDATED).and_then(Value::as_bool) == Some(true) {
                return Ok((Vec::new(), false));
            }
            let mut redated = Vec::new();
            for (steps, wanted) in &wanted {
                let Some(previous) = stored_work_status(state, &wanted.slug) else {
                    continue;
                };
                let mut checks = previous.open_checks();
                // The check as it was read: one closed or set again since
                // is another check.
                let Some((_, check)) = checks.iter_mut().find(|(stage, check)| {
                    *stage == wanted.stage
                        && check.steps == *steps
                        && check.since_ms.is_none()
                        && check.due_since_ms == Some(wanted.from_ms)
                }) else {
                    continue;
                };
                check.due_since_ms = Some(wanted.to_ms);
                let mut next = previous.clone();
                next.set_checks(checks);
                write_work_status(state, &wanted.slug, Some(&previous), Some(&next))?;
                redated.push(wanted.clone());
            }
            state[REDATED] = Value::Bool(true);
            Ok((redated, true))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DAY: i64 = 24 * 60 * 60 * 1000;
    const NOW: i64 = 1_795_000_000_000;

    fn iso(ms: i64) -> String {
        let secs = ms / 1000;
        let days = secs.div_euclid(86_400);
        let (h, m, s) = (
            secs.rem_euclid(86_400) / 3600,
            secs.rem_euclid(3600) / 60,
            secs.rem_euclid(60),
        );
        // Civil date from days since the epoch.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let (d, mo) = (
            doy - (153 * mp + 2) / 5 + 1,
            if mp < 10 { mp + 3 } else { mp - 9 },
        );
        let y = yoe + era * 400 + i64::from(mo <= 2);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.000Z")
    }

    fn old() -> Value {
        json!({
            "createdAt": iso(NOW - 90 * DAY),
            "baseBranch": "main",
            "order": 3.0,
            "section": null,
            "work": {"state": "verified", "at": iso(NOW - 60 * DAY)},
        })
    }

    fn with(mut record: Value, key: &str, value: Value) -> Value {
        record[key] = value;
        record
    }

    fn prunable(state: &Value, live: &[&str]) -> Vec<String> {
        let live = live.iter().map(|slug| (*slug).to_owned()).collect();
        prunable_slug_records(state, &live, NOW)
    }

    #[test]
    fn only_a_long_gone_record_that_owes_nothing_is_prunable() {
        let mut owing = old();
        owing["work"]["checks"] = json!({"staging": {"what": "smoke"}});
        let mut gated = old();
        gated["work"]["blockedOn"] = json!("a deploy");
        let state = json!({
            "slugs": {
                "gone": old(),
                "live": old(),
                "main": old(),
                "@remote/box/gone": old(),
                "recent": with(old(), "guide", json!({"at": iso(NOW - 2 * DAY)})),
                "recent-ms": with(old(), "otherBranches", json!({"b": {"checkedAtMs": NOW - DAY}})),
                "mid-creation": {"baseBranch": "main", "work": {"state": "ready", "at": iso(NOW - 60 * DAY)}},
                "undated": {"order": 1.0, "section": null},
                "owing": owing,
                "gated": gated,
                "paused": with(old(), "automationsPaused", json!(true)),
                "in-landed-block": old(),
                "being-removed": old(),
                "has-step": old(),
                "watched": old(),
                "watcher": old(),
                "parent": with(old(), "branch", json!("me/parent-branch")),
                "base-by-name": old(),
                "child": with(old(), "baseBranch", json!("me/parent-branch")),
                "child-two": with(old(), "baseBranch", json!("me/base-by-name")),
            },
            "removed": [{"slug": "in-landed-block", "removedAt": iso(NOW - DAY)}],
            "removals": {"being-removed": {"startedAt": 1}},
            "owed": [{"id": "a", "slug": "has-step", "by": "has-step"}],
            "watches": [{"id": "b", "slug": "watched", "by": "watcher"}],
        });
        assert_eq!(
            prunable(&state, &["live", "child", "child-two"]),
            ["gone"],
            "every other record has a reason to stay"
        );
        // Not a day sooner.
        let young = json!({"slugs": {"gone": with(old(), "work", json!({"state": "ready", "at": iso(NOW - 29 * DAY)}))}});
        assert!(prunable(&young, &[]).is_empty());
    }

    #[test]
    fn pruning_writes_only_what_the_rules_allow_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(
            dir.path().join("state.sqlite"),
            crate::RepositoryIdentity::new("fixture", "/fixture"),
        )
        .unwrap();
        let mut state = store.read_wt_state().unwrap();
        state["slugs"] = json!({"gone": old(), "live": old(), "paused": with(old(), "automationsPaused", json!(true))});
        store
            .write_repository_state_json(&serde_json::to_string(&state).unwrap())
            .unwrap();
        let live = BTreeSet::from(["live".to_owned()]);
        assert_eq!(store.prune_slug_records(&live, NOW).unwrap(), ["gone"]);
        let after = store.read_wt_state().unwrap();
        let kept: Vec<&String> = after["slugs"].as_object().unwrap().keys().collect();
        assert_eq!(kept, ["live", "paused"]);
        assert!(store.prune_slug_records(&live, NOW).unwrap().is_empty());
    }

    /// A production check dated by the first build that dates checks, with
    /// the time it started, is moved back to when history shows it came
    /// due, only where history can say, and only once.
    #[test]
    fn a_legacy_production_checks_due_time_is_read_back_from_history_once() {
        use wt_core::{WorkState, WorktreeEventKind as Event};
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(
            dir.path().join("state.sqlite"),
            crate::RepositoryIdentity::new("fixture", "/fixture"),
        )
        .unwrap();
        const BOOT: i64 = NOW - DAY;
        let legacy = |stage: &str| {
            json!({
                "createdAt": iso(NOW - 9 * DAY),
                "work": {
                    "state": "ready", "at": iso(NOW - 8 * DAY),
                    "checks": {stage: {"steps": "open the page", "dueSinceMs": BOOT}},
                },
            })
        };
        let mut dated = legacy("production");
        dated["work"]["checks"]["production"]["sinceMs"] = json!(NOW - 8 * DAY);
        let slugs = [
            "shipped",
            "owed-late",
            "owed-again",
            "staged",
            "dated",
            "unsaid",
        ];
        let mut state = store.read_wt_state().unwrap();
        for slug in slugs {
            state["slugs"][slug] = match slug {
                "staged" => legacy("staging"),
                "dated" => dated.clone(),
                _ => legacy("production"),
            };
        }
        store
            .write_repository_state_json(&serde_json::to_string(&state).unwrap())
            .unwrap();
        let status = |verify_in: Option<&str>| Event::Status {
            from: None,
            to: WorkState::Ready,
            by: None,
            risk: None,
            note: None,
            blocked_on: None,
            verify_in: verify_in.map(str::to_owned),
        };
        let owes = |stage: &str| status(Some(stage));
        let landed = |on| Event::Landed {
            on,
            head: None,
            landed_at_ms: None,
        };
        let closed = Event::Owed {
            id: None,
            change: wt_core::OwedChange::Done,
            what: "open the page".into(),
            evidence: None,
            by: None,
            stage: Some("production".into()),
            when: None,
            unconfirmed: false,
        };
        let at = |days: i64| u64::try_from(NOW - days * DAY).unwrap();
        let production = LandedOn::Production;
        for (slug, events) in [
            (
                "shipped",
                vec![
                    (8, owes("production")),
                    (6, landed(LandedOn::Base)),
                    (4, landed(production)),
                ],
            ),
            // Owed only after it had shipped: history has no later sighting.
            (
                "owed-late",
                vec![(6, landed(production)), (3, owes("production"))],
            ),
            // Closed and owed again: the first round's landing is not this
            // check's.
            (
                "owed-again",
                vec![
                    (8, owes("production")),
                    (6, landed(production)),
                    (5, closed),
                    (3, owes("production")),
                ],
            ),
            (
                "staged",
                vec![(8, owes("staging")), (6, landed(LandedOn::Base))],
            ),
            (
                "dated",
                vec![(8, owes("production")), (4, landed(production))],
            ),
            ("unsaid", vec![(4, landed(production))]),
        ] {
            store
                .append_worktree_event(slug, at(9), &Event::Created { base: None })
                .unwrap();
            for (days, event) in events {
                store.append_worktree_event(slug, at(days), &event).unwrap();
            }
        }

        let moved = store.redate_checks_from_history().unwrap();
        let moved: Vec<(&str, i64)> = moved
            .iter()
            .map(|check| (check.slug.as_str(), (NOW - check.to_ms) / DAY))
            .collect();
        assert_eq!(moved, [("shipped", 4)]);
        let due = |store: &mut Store, slug: &str| {
            let state = store.read_wt_state().unwrap();
            let checks = &state["slugs"][slug]["work"]["checks"];
            checks["production"]["dueSinceMs"]
                .as_i64()
                .or(checks["staging"]["dueSinceMs"].as_i64())
        };
        assert_eq!(due(&mut store, "shipped"), Some(NOW - 4 * DAY));
        for slug in ["owed-late", "owed-again", "staged", "dated", "unsaid"] {
            assert_eq!(due(&mut store, slug), Some(BOOT), "{slug}");
        }

        // It ran: a check that would qualify now is not touched again.
        let mut state = store.read_wt_state().unwrap();
        state["slugs"]["shipped"]["work"]["checks"]["production"]["dueSinceMs"] = json!(BOOT);
        store
            .write_repository_state_json(&serde_json::to_string(&state).unwrap())
            .unwrap();
        assert!(store.redate_checks_from_history().unwrap().is_empty());
        assert_eq!(due(&mut store, "shipped"), Some(BOOT));
    }
}
