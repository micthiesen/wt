use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use wt_core::{Check, Checks, OwedRecord, VerifyStage};

/// The stable identity for a repository namespace in the shared state DB.
///
/// `path` must be the canonical repository path chosen by the caller. Keeping
/// it explicit leaves path/config discovery outside this crate and lets the
/// store reject a colliding id before it can read or write that namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryIdentity {
    pub id: String,
    pub path: String,
}

impl RepositoryIdentity {
    pub fn new(id: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            path: path.into(),
        }
    }
}

/// Lossless JSON envelope for wt's forward-versioned repository state.
///
/// Known records remain available as JSON values so newer builds can add fields
/// without an older Rust build silently deleting them on a read/modify/write.
pub type WtState = Value;

/// A work status as stored, with every text field as it was written.
///
/// What the record owes after merging is its per-stage checks, kept under
/// the `checks` key beside the fields this type names (so they sit in
/// `extra`, as `checksMirror` does) and read and changed through the
/// methods here. `verify_after_merge` and `verify_in` mirror the first
/// check for older builds; see [`wt_core::normalize_checks`]. A record that
/// sets only those two reads as owing that one check. `checks` is always
/// written, as `{}` when nothing is owed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    from = "WorkStatusRepr",
    into = "WorkStatusRepr"
)]
pub struct WorkStatusRecord {
    pub state: String,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_on: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_after_merge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_in: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The record's JSON shape. An empty `checks` is written but not kept in
/// `extra`, so a record reads back equal to the one that was written.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkStatusRepr {
    state: String,
    at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    risk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blocked_on: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verify_after_merge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verify_in: Option<String>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl From<WorkStatusRepr> for WorkStatusRecord {
    fn from(mut repr: WorkStatusRepr) -> Self {
        if repr
            .extra
            .get(CHECKS)
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty)
        {
            repr.extra.remove(CHECKS);
        }
        Self {
            state: repr.state,
            at: repr.at,
            note: repr.note,
            risk: repr.risk,
            sha: repr.sha,
            by: repr.by,
            blocked_on: repr.blocked_on,
            verify_after_merge: repr.verify_after_merge,
            verify_in: repr.verify_in,
            extra: repr.extra,
        }
    }
}

impl From<WorkStatusRecord> for WorkStatusRepr {
    fn from(mut record: WorkStatusRecord) -> Self {
        record
            .extra
            .entry(CHECKS)
            .or_insert_with(|| Value::Object(Map::new()));
        Self {
            state: record.state,
            at: record.at,
            note: record.note,
            risk: record.risk,
            sha: record.sha,
            by: record.by,
            blocked_on: record.blocked_on,
            verify_after_merge: record.verify_after_merge,
            verify_in: record.verify_in,
            extra: record.extra,
        }
    }
}

pub(crate) const CHECKS: &str = "checks";
pub(crate) const CHECKS_MIRROR: &str = "checksMirror";

/// The checks and their mirror stage kept under `checks` and
/// `checksMirror` in a record's or a removal record's other keys.
pub(crate) fn stored_checks(extra: &Map<String, Value>) -> (Checks, Option<VerifyStage>) {
    let checks = extra
        .get(CHECKS)
        .and_then(|checks| serde_json::from_value(checks.clone()).ok())
        .unwrap_or_default();
    let mirror = extra
        .get(CHECKS_MIRROR)
        .and_then(Value::as_str)
        .and_then(VerifyStage::parse);
    (checks, mirror)
}

/// Write checks and their mirror stage back where [`stored_checks`] reads
/// them. `keep_empty` keeps an empty `checks` key, which a removal record
/// needs to say its checks are its own and not its status's.
pub(crate) fn store_checks(
    extra: &mut Map<String, Value>,
    checks: &Checks,
    mirror: Option<VerifyStage>,
    keep_empty: bool,
) {
    match serde_json::to_value(checks) {
        Ok(Value::Object(map)) if map.is_empty() && !keep_empty => {
            extra.remove(CHECKS);
        }
        Ok(value) => {
            extra.insert(CHECKS.to_owned(), value);
        }
        Err(_) => {}
    }
    match mirror {
        Some(stage) => {
            extra.insert(
                CHECKS_MIRROR.to_owned(),
                Value::String(stage.as_str().into()),
            );
        }
        None => {
            extra.remove(CHECKS_MIRROR);
        }
    }
}

impl WorkStatusRecord {
    /// Match the persisted-claim equality contract: `at` and `by` do not
    /// change the underlying claim, while every claim field does.
    pub fn same_claim(&self, other: &Self) -> bool {
        self.state == other.state
            && self.note == other.note
            && self.risk == other.risk
            && self.sha == other.sha
            && self.blocked_on == other.blocked_on
            && self.verify_after_merge == other.verify_after_merge
            && self.verify_in == other.verify_in
            && self.all_checks() == other.all_checks()
    }

    /// Whether the state discharges every check: verified or dropped.
    pub fn is_terminal(&self) -> bool {
        matches!(self.state.as_str(), "verified" | "dropped")
    }

    /// Run `edit` on the record's checks and the fields that mirror them,
    /// in their typed form, and write back what it leaves. A stage text
    /// this build cannot name is kept unless the mirror moves.
    fn with_checks<R>(
        &mut self,
        edit: impl FnOnce(
            &mut Checks,
            &mut Option<String>,
            &mut Option<VerifyStage>,
            &mut Option<VerifyStage>,
        ) -> R,
    ) -> R {
        let (mut checks, mut mirror) = stored_checks(&self.extra);
        let was = self.verify_in.as_deref().and_then(VerifyStage::parse);
        let mut stage = was;
        let result = edit(
            &mut checks,
            &mut self.verify_after_merge,
            &mut stage,
            &mut mirror,
        );
        if stage != was || (stage.is_none() && self.verify_after_merge.is_none()) {
            self.verify_in = stage.map(|stage| stage.as_str().to_owned());
        }
        store_checks(&mut self.extra, &checks, mirror, false);
        result
    }

    /// Reconcile the checks with the fields older builds read; see
    /// [`wt_core::normalize_checks`]. Returns the checks an older build
    /// discharged. The store does this to every record it writes.
    pub fn normalize(&mut self) -> Vec<(VerifyStage, Check)> {
        let terminal = self.is_terminal();
        self.with_checks(|checks, steps, stage, mirror| {
            wt_core::normalize_checks(terminal, checks, steps, stage, mirror)
        })
    }

    /// Every check on the record as [`Self::normalize`] would leave them,
    /// whatever the state.
    pub fn all_checks(&self) -> Checks {
        let mut view = self.clone();
        view.normalize();
        stored_checks(&view.extra).0
    }

    /// The checks still owed, staging before production. A verified or
    /// dropped record owes none.
    pub fn open_checks(&self) -> Vec<(VerifyStage, Check)> {
        if self.is_terminal() {
            return Vec::new();
        }
        self.all_checks().clear()
    }

    /// The check still owed in `stage`.
    pub fn open_check(&self, stage: VerifyStage) -> Option<Check> {
        self.open_checks()
            .into_iter()
            .find_map(|(owed, check)| (owed == stage).then_some(check))
    }

    fn edit_checks<R>(&mut self, edit: impl FnOnce(&mut Checks) -> R) -> R {
        self.normalize();
        self.with_checks(|checks, steps, stage, mirror| {
            let result = edit(checks);
            wt_core::mirror_checks(checks, steps, stage, mirror);
            result
        })
    }

    /// Owe a check in `stage` with these steps, leaving any other stage's
    /// check alone. The same steps keep a not-before the check had; new
    /// steps are a new check.
    pub fn set_check(&mut self, stage: VerifyStage, steps: impl Into<String>) {
        let steps = steps.into();
        self.edit_checks(|checks| {
            if checks.get(stage).is_none_or(|check| check.steps != steps) {
                checks.set(stage, Check::new(steps));
            }
        });
    }

    /// [`Self::set_check`] on a record being built.
    #[must_use]
    pub fn with_check(mut self, stage: VerifyStage, steps: impl Into<String>) -> Self {
        self.set_check(stage, steps);
        self
    }

    /// Stop owing the check in `stage`, returning it.
    pub fn drop_check(&mut self, stage: VerifyStage) -> Option<Check> {
        self.edit_checks(|checks| checks.take(stage))
    }

    /// Stop owing every check, returning them.
    pub fn clear_checks(&mut self) -> Vec<(VerifyStage, Check)> {
        self.edit_checks(Checks::clear)
    }

    /// Put the check in `stage` off: it stays owed and is not due before
    /// `not_before_ms`. `None` clears that. Returns whether such a check
    /// is owed.
    pub fn defer_check(&mut self, stage: VerifyStage, not_before_ms: Option<i64>) -> bool {
        self.edit_checks(|checks| match checks.take(stage) {
            Some(mut check) => {
                check.not_before_ms = not_before_ms;
                checks.set(stage, check);
                true
            }
            None => false,
        })
    }

    /// Replace every check with `checks`.
    pub fn set_checks(&mut self, checks: Vec<(VerifyStage, Check)>) {
        self.edit_checks(|owed| {
            owed.clear();
            for (stage, check) in checks {
                owed.set(stage, check);
            }
        });
    }

    /// Carry forward every check `previous` still owes in a stage this
    /// record sets none for: a fresh assertion does not discharge a check.
    pub fn carry_checks(&mut self, previous: &Self) {
        let carried = previous.open_checks();
        self.edit_checks(|checks| {
            for (stage, check) in carried {
                if checks.get(stage).is_none() {
                    checks.set(stage, check);
                }
            }
        });
    }

    /// The record as the pure rules in `wt_core` read it. `None` when a
    /// newer build wrote a state this one cannot name.
    pub fn to_core(&self) -> Option<wt_core::WorkStatusRecord> {
        wt_core::parse_work_status(&serde_json::to_value(self).ok()?)
    }
}

/// What `Store::update_slug_work_status` read and wrote in its transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkStatusWrite {
    pub previous: Option<WorkStatusRecord>,
    pub record: Option<WorkStatusRecord>,
    pub changed: bool,
    /// Watches that were to clear the gate this write removed or replaced,
    /// cancelled with it.
    pub cancelled_watches: Vec<wt_core::watch::Watch>,
    /// The review stamp this status earned; see `Store::set_pending_stamp`.
    pub stamped: Option<wt_core::Stamp>,
}

/// One removed worktree, as `removed[]` keeps it.
///
/// What the removed row still owes is kept beside the fields this type
/// names (so in `extra`), not inside `work`: an older build settling the
/// row's check replaces `work` whole. `checks` and `checksMirror` hold its
/// per-stage checks, with `work` keeping the mirror older builds read, and
/// `owed` holds the steps that had come due when it was removed. Read them
/// with [`Self::open_checks`] and [`Self::owed`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovedWorktree {
    pub slug: String,
    pub branch: String,
    pub removed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<WorkStatusRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automations_paused: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRequestDismissal {
    pub url: String,
    pub updated_at: String,
    pub dismissed_at: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub strength: String,
    pub at: String,
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_sha: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub(crate) const OWED: &str = "owed";

impl RemovedWorktree {
    /// The checks the removed row still owes, staging before production,
    /// after reconciling what an older build may have written to `work`
    /// since (see [`wt_core::normalize_checks`]). A record from before
    /// per-stage checks owes the one check its `work` names. No `work`, or
    /// a verified or dropped one, owes none.
    pub fn open_checks(&self) -> Vec<(VerifyStage, Check)> {
        self.settled()
            .map(|(checks, ..)| checks)
            .unwrap_or_default()
            .clear()
    }

    /// The checks, their mirror stage and `work` with its mirror fields
    /// written from them and no checks of its own. `None` without a `work`.
    pub(crate) fn settled(&self) -> Option<(Checks, Option<VerifyStage>, WorkStatusRecord)> {
        let mut work = self.work.clone()?;
        let (mut checks, mut mirror) = if self.extra.contains_key(CHECKS) {
            stored_checks(&self.extra)
        } else {
            // A status carried over from a live row with its checks inside,
            // or a record from before the checks moved out of it.
            work.normalize();
            stored_checks(&work.extra)
        };
        let terminal = work.is_terminal();
        work.with_checks(|own, steps, stage, own_mirror| {
            own.clear();
            *own_mirror = None;
            // A settled row keeps the text of what was verified.
            if !terminal {
                wt_core::normalize_checks(false, &mut checks, steps, stage, &mut mirror);
            }
        });
        if terminal {
            return Some((Checks::default(), None, work));
        }
        Some((checks, mirror, work))
    }

    /// The steps that had come due and were not closed when the row was
    /// removed. `wt done --slug` closes them here.
    pub fn owed(&self) -> Vec<OwedRecord> {
        self.extra
            .get(OWED)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|step| serde_json::from_value(step.clone()).ok())
            .collect()
    }

    /// Whether the removed row still owes anything: a check or a due step.
    pub fn owes_anything(&self) -> bool {
        !self.open_checks().is_empty() || !self.owed().is_empty()
    }

    /// The status as a live worktree holds it, with the checks the row
    /// still owes attached again. What a restore writes back.
    pub fn work_with_checks(&self) -> Option<WorkStatusRecord> {
        let (checks, _, mut work) = self.settled()?;
        work.set_checks(checks.clone().clear());
        Some(work)
    }
}

/// A repository snapshot found in an arbitrary database during state repair.
#[derive(Clone, Debug, PartialEq)]
pub struct ForeignRepositoryRow {
    pub repo_id: String,
    pub repo_path: String,
    pub data: String,
    pub updated_at: i64,
    pub archived: BTreeSet<String>,
}

/// A worktree's change guide: a published walkthrough of the change, and the
/// HEAD it described when it was recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuideRecord {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    pub at: String,
}
