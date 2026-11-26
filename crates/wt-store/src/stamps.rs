//! Review stamps: the mark a worktree keeps after a round of a stamping
//! skill. A skill that starts leaves a pending stamp on the slug entry; the
//! next status that is not `working` or `review` earns it, onto the status
//! record, where every later status write carries it forward.

use serde_json::{Value, json};
use wt_core::{STAMP_KEY, STAMP_PENDING_KEY, Stamp, WorktreeEventKind};

use crate::{
    Store, StoreError, WorkStatusRecord, WtState,
    events::EventLog,
    mutations::{log_incarnation, stored_work_status},
};

fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

fn author(by: Option<&str>) -> Option<String> {
    by.map(str::trim)
        .filter(|by| !by.is_empty() && *by != "human")
        .map(str::to_owned)
}

fn slug_object<'a>(
    state: &'a mut WtState,
    slug: &str,
) -> Option<&'a mut serde_json::Map<String, Value>> {
    state.get_mut("slugs")?.get_mut(slug)?.as_object_mut()
}

/// The stamp the worktree's status carries.
pub fn stored_stamp(state: &WtState, slug: &str) -> Option<Stamp> {
    Stamp::parse(state.get("slugs")?.get(slug)?.get("work")?.get(STAMP_KEY)?)
}

/// How long a round may take. A stamp still pending after this was left by
/// a round that was abandoned, and a later status must not earn it.
const PENDING_STAMP_MS: i64 = 24 * 60 * 60 * 1000;

fn now_ms() -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .unwrap_or(i64::MAX)
}

/// The skill of a stamp that is waiting to be earned. One left pending for
/// more than a day is not.
pub fn stored_pending_stamp(state: &WtState, slug: &str) -> Option<String> {
    pending_stamp_at(state, slug, now_ms())
}

fn pending_stamp_at(state: &WtState, slug: &str, now_ms: i64) -> Option<String> {
    let pending = state.get("slugs")?.get(slug)?.get(STAMP_PENDING_KEY)?;
    let at = wt_core::parse_iso_millis(pending.get("at")?.as_str()?)?;
    if now_ms.saturating_sub(at) > PENDING_STAMP_MS {
        return None;
    }
    pending
        .get("skill")?
        .as_str()
        .filter(|skill| wt_core::is_stamp_name(skill))
        .map(str::to_owned)
}

/// Drop a pending stamp nothing can earn any more. Returns whether the
/// state changed.
pub(crate) fn drop_expired_pending(state: &mut WtState, slug: &str) -> bool {
    let present = state
        .get("slugs")
        .and_then(|slugs| slugs.get(slug))
        .is_some_and(|entry| entry.get(STAMP_PENDING_KEY).is_some());
    present && stored_pending_stamp(state, slug).is_none() && clear_pending(state, slug)
}

fn put(state: &mut WtState, slug: &str, stamp: &Stamp) -> bool {
    let Some(entry) = slug_object(state, slug) else {
        return false;
    };
    entry.remove(STAMP_PENDING_KEY);
    let Some(work) = entry.get_mut("work").and_then(Value::as_object_mut) else {
        return false;
    };
    work.insert(STAMP_KEY.to_owned(), json!(stamp));
    true
}

fn log_stamped(log: &EventLog<'_>, state: &WtState, slug: &str, stamp: &Stamp) {
    log.best_effort(|log| {
        log_incarnation(log, state, slug)?;
        log.append(
            slug,
            &WorktreeEventKind::Mark {
                text: format!("stamped {}", stamp.skill),
                by: stamp.by.clone(),
            },
        )
    });
}

/// Turn a pending stamp into the stamp of the status just written, when
/// that status ends the round: anything but `working` and `review`. An
/// interrupted round asserts neither, so it never stamps.
pub(crate) fn earn(
    state: &mut WtState,
    log: &EventLog<'_>,
    slug: &str,
    record: &WorkStatusRecord,
) -> Option<Stamp> {
    if matches!(record.state.as_str(), "working" | "review") {
        return None;
    }
    let stamp = Stamp {
        skill: stored_pending_stamp(state, slug)?,
        sha: record.sha.clone(),
        at: now_iso(),
        by: author(record.by.as_deref()),
        extra: Default::default(),
    };
    put(state, slug, &stamp).then(|| {
        log_stamped(log, state, slug, &stamp);
        stamp
    })
}

/// Forget a stamp that was never earned.
pub(crate) fn clear_pending(state: &mut WtState, slug: &str) -> bool {
    slug_object(state, slug).is_some_and(|entry| entry.remove(STAMP_PENDING_KEY).is_some())
}

impl Store {
    /// A stamping skill started in the worktree: its stamp is earned by the
    /// next status that ends the round. Returns whether it was recorded.
    pub fn set_pending_stamp(
        &mut self,
        slug: &str,
        skill: &str,
        by: Option<&str>,
    ) -> Result<bool, StoreError> {
        if !wt_core::is_stamp_name(skill) {
            return Ok(false);
        }
        self.mutate_wt_state(|state| {
            if !state.get("slugs").is_some_and(Value::is_object) {
                state["slugs"] = json!({});
            }
            if !state["slugs"].get(slug).is_some_and(Value::is_object) {
                state["slugs"][slug] = json!({});
            }
            let Some(entry) = slug_object(state, slug) else {
                return Ok((false, false));
            };
            let mut pending = json!({"skill": skill, "at": now_iso()});
            if let Some(by) = author(by) {
                pending["by"] = json!(by);
            }
            entry.insert(STAMP_PENDING_KEY.to_owned(), pending);
            Ok((true, true))
        })
    }

    /// Stamp the worktree now, at `sha`, in place of any stamp it has. A
    /// worktree with no status has nothing to carry a stamp: it is left
    /// pending, for its next status to earn, and `None` comes back.
    pub fn stamp_now(
        &mut self,
        slug: &str,
        skill: &str,
        sha: Option<&str>,
        by: Option<&str>,
    ) -> Result<Option<Stamp>, StoreError> {
        if !wt_core::is_stamp_name(skill) {
            return Ok(None);
        }
        if stored_work_status(&self.read_wt_state()?, slug).is_none() {
            self.set_pending_stamp(slug, skill, by)?;
            return Ok(None);
        }
        self.mutate_wt_state_logged(|state, log| {
            let stamp = Stamp {
                skill: skill.to_owned(),
                sha: sha.map(str::to_owned),
                at: now_iso(),
                by: author(by),
                extra: Default::default(),
            };
            if !put(state, slug, &stamp) {
                return Ok((None, false));
            }
            log_stamped(log, state, slug, &stamp);
            Ok((Some(stamp), true))
        })
    }
}
