use crate::time::parse_iso_millis;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const WORK_STATES: [WorkState; 8] = [
    WorkState::Todo,
    WorkState::Working,
    WorkState::Review,
    WorkState::NeedsTesting,
    WorkState::NeedsHuman,
    WorkState::Ready,
    WorkState::Verified,
    WorkState::Dropped,
];
pub const WORK_RISKS: [WorkRisk; 3] = [WorkRisk::Low, WorkRisk::Medium, WorkRisk::High];
pub const GATED_TODO_RANK: u8 = 8;
pub const BLOCKED_RANK: u8 = 5;
pub const NO_STATUS_RANK: u8 = 6;
pub const VERIFY_OVERDUE_DAYS: i64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkState {
    Todo,
    Working,
    Review,
    NeedsTesting,
    NeedsHuman,
    Ready,
    Verified,
    Dropped,
}

/// Where an owed post-merge check runs, which decides when it is due:
/// `Staging` once the branch lands on `[branch] base`, `Production` once the
/// landing reaches `[branch] production` (or lands on base when no separate
/// production branch is configured).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyStage {
    Staging,
    Production,
}

impl VerifyStage {
    /// In the order a change reaches them.
    pub const ALL: [Self; 2] = [Self::Staging, Self::Production];

    /// The stage a check recorded without one is owed in. Records written
    /// before the stage existed were due on the base landing.
    pub fn or_default(stage: Option<Self>) -> Self {
        stage.unwrap_or(Self::Staging)
    }

    /// [`Self::or_default`] for a stage kept as text. A stage this build
    /// cannot name reads as absent.
    pub fn of_stored(stage: Option<&str>) -> Self {
        Self::or_default(stage.and_then(Self::parse))
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "staging" => Some(Self::Staging),
            "production" => Some(Self::Production),
            _ => None,
        }
    }

    /// The Git ref in the main clone that holds the commit a deploy probe
    /// last reported for this stage.
    pub fn deployed_ref(self) -> String {
        format!("refs/wt/deployed/{}", self.as_str())
    }
}

/// The merge commit of a PR merged at exactly `current_head`, if any. Only
/// that commit (or HEAD itself) proves where the branch's work landed; a PR
/// merged at an older head says nothing about the current one.
pub fn exact_merged_pr_oid<'a>(
    pr_head: Option<&'a str>,
    current_head: &str,
    pr_state: &str,
    merge_oid: Option<&'a str>,
) -> Option<&'a str> {
    (pr_head == Some(current_head) && pr_state.eq_ignore_ascii_case("MERGED"))
        .then_some(merge_oid)
        .flatten()
}

/// An unknown stage (from a newer writer) reads as absent rather than
/// failing the whole record.
fn lenient_stage<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<VerifyStage>, D::Error> {
    Ok(Option::<Value>::deserialize(deserializer)?
        .as_ref()
        .and_then(Value::as_str)
        .and_then(VerifyStage::parse))
}

/// Whether a landing is deployed, per stage. `None` means the stage has no
/// deploy probe (`[environments.<stage>]`), so the Git landing alone decides.
/// `Some(false)` means a probe exists and has not shown the landing deployed:
/// not deployed yet, unknown, or never looked up. A default value has not
/// met the configuration yet: see [`StageDeploys::for_probes`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct StageDeploys {
    pub staging: Option<bool>,
    pub production: Option<bool>,
}

impl StageDeploys {
    pub const UNPROBED: Self = Self {
        staging: None,
        production: None,
    };

    pub const fn get(self, stage: VerifyStage) -> Option<bool> {
        match stage {
            VerifyStage::Staging => self.staging,
            VerifyStage::Production => self.production,
        }
    }

    /// These facts under a configuration: a stage `probed` names that has
    /// no answer here was not looked up, which is not the same as having no
    /// probe, and reads as not shown deployed. A stage without a probe has
    /// no deploy fact at all. Every place that hands deploy facts to a
    /// decision passes them through this.
    #[must_use]
    pub fn for_probes(self, probed: impl Fn(VerifyStage) -> bool) -> Self {
        let fact = |stage| probed(stage).then(|| self.get(stage) == Some(true));
        Self {
            staging: fact(VerifyStage::Staging),
            production: fact(VerifyStage::Production),
        }
    }

    pub fn set(&mut self, stage: VerifyStage, deployed: Option<bool>) {
        match stage {
            VerifyStage::Staging => self.staging = deployed,
            VerifyStage::Production => self.production = deployed,
        }
    }
}

/// How far a worktree's branch has landed, as far as an owed check cares.
/// `production` is true once the landing reached production, and also for any
/// base landing when the repository configures no separate production branch.
/// `base` and `production` are the Git landing; `deploys` adds whether a
/// probed stage is actually running it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Landing {
    pub base: bool,
    pub production: bool,
    pub deploys: StageDeploys,
}

impl Landing {
    pub const NONE: Self = Self {
        base: false,
        production: false,
        deploys: StageDeploys::UNPROBED,
    };

    /// `separate_production` is whether `[branch] production` names a branch
    /// other than base; without one a base landing is the release.
    pub fn new(base: bool, in_production: bool, separate_production: bool) -> Self {
        let base = base || in_production;
        Self {
            base,
            production: in_production || (base && !separate_production),
            deploys: StageDeploys::UNPROBED,
        }
    }

    pub fn with_deploys(mut self, deploys: StageDeploys) -> Self {
        self.deploys = deploys;
        self
    }

    /// The Git landing alone, as if no stage had a deploy probe.
    pub fn git_only(self) -> Self {
        self.with_deploys(StageDeploys::UNPROBED)
    }

    /// The landing reached the stage's branch in Git.
    pub fn landed(self, stage: VerifyStage) -> bool {
        match stage {
            VerifyStage::Staging => self.base,
            VerifyStage::Production => self.production,
        }
    }

    /// The stage has the change: landed in Git and, when the stage has a
    /// deploy probe, deployed there.
    pub fn reached(self, stage: VerifyStage) -> bool {
        self.landed(stage) && self.deploys.get(stage) != Some(false)
    }

    /// Landed in Git on a probed stage that is not running it yet.
    pub fn awaiting_deploy(self, stage: VerifyStage) -> bool {
        self.landed(stage) && self.deploys.get(stage) == Some(false)
    }
}

/// One post-merge check a worktree owes in a stage.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub steps: String,
    /// The check is not due before this instant, even where its stage
    /// already has the change. Set when a check is put off (`wt done
    /// <stage> --again <time>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before_ms: Option<i64>,
    /// When this check began to be owed. Set when a status first owes it,
    /// and again when it comes due for a later pull request of the same
    /// worktree, so each is prompted for once. Absent on checks set before
    /// wt recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<i64>,
    /// When the check was first seen due. Asserting the status again keeps
    /// it, so what is overdue is counted from here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_since_ms: Option<i64>,
    /// The merged pull request the check came due for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_pr: Option<u64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Check {
    pub fn new(steps: impl Into<String>) -> Self {
        Self {
            steps: steps.into(),
            not_before_ms: None,
            since_ms: None,
            due_since_ms: None,
            due_pr: None,
            extra: BTreeMap::new(),
        }
    }

    /// Since when the check has been due, where that was recorded: when it
    /// was first seen due, or the time it was put off to when that is later.
    pub fn due_at_ms(&self) -> Option<i64> {
        let since = self.due_since_ms?;
        Some(self.not_before_ms.map_or(since, |at| at.max(since)))
    }

    /// Whether the check was put off to a time that has not come.
    pub fn deferred(&self, now_ms: i64) -> bool {
        self.not_before_ms.is_some_and(|at| at > now_ms)
    }
}

/// A check seen due, for the store to record when it first was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckDue {
    pub slug: String,
    /// The owner's `createdAt`: a later worktree with the same slug owes
    /// its own check.
    pub created_at: Option<String>,
    pub stage: VerifyStage,
    pub steps: String,
    /// Since when it has been due, as far as that can be told.
    pub since_ms: i64,
    /// The worktree's merged pull request.
    pub pr: Option<u64>,
}

/// The post-merge checks a status record owes, at most one per stage. Stored
/// as `"checks": { "staging": {..}, "production": {..} }` and always written,
/// as `{}` when none is owed. A stage this build cannot name is carried
/// through untouched.
///
/// A record also mirrors its first check in the fields older builds read
/// (`verifyAfterMerge`, `verifyIn`). Change a record's checks through the
/// record's own methods, which keep that mirror true; a record whose checks
/// were edited here and not mirrored reads, on its next write, like one an
/// older build wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checks {
    staging: Option<Check>,
    production: Option<Check>,
    other: BTreeMap<String, Value>,
}

impl Checks {
    pub fn get(&self, stage: VerifyStage) -> Option<&Check> {
        match stage {
            VerifyStage::Staging => self.staging.as_ref(),
            VerifyStage::Production => self.production.as_ref(),
        }
    }

    fn slot(&mut self, stage: VerifyStage) -> &mut Option<Check> {
        match stage {
            VerifyStage::Staging => &mut self.staging,
            VerifyStage::Production => &mut self.production,
        }
    }

    /// The owed checks, staging before production.
    pub fn iter(&self) -> impl Iterator<Item = (VerifyStage, &Check)> {
        VerifyStage::ALL
            .into_iter()
            .filter_map(|stage| Some((stage, self.get(stage)?)))
    }

    /// Whether no stage this build can name owes a check.
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    /// Owe `check` in `stage`, in place of any check owed there, which is
    /// returned. A check with blank steps is none.
    pub fn set(&mut self, stage: VerifyStage, check: Check) -> Option<Check> {
        let check = Some(check).filter(|check| !check.steps.trim().is_empty());
        std::mem::replace(self.slot(stage), check)
    }

    pub fn take(&mut self, stage: VerifyStage) -> Option<Check> {
        self.slot(stage).take()
    }

    /// Drop every check this build can name, returning them.
    pub fn clear(&mut self) -> Vec<(VerifyStage, Check)> {
        VerifyStage::ALL
            .into_iter()
            .filter_map(|stage| Some((stage, self.take(stage)?)))
            .collect()
    }

    /// The same checks with every step text passed through `clean`.
    fn cleaned(mut self, clean: impl Fn(&str) -> String) -> Self {
        for stage in VerifyStage::ALL {
            if let Some(mut check) = self.take(stage) {
                check.steps = clean(&check.steps);
                self.set(stage, check);
            }
        }
        self
    }
}

impl Serialize for Checks {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        for (stage, check) in self.iter() {
            map.serialize_entry(stage.as_str(), check)?;
        }
        for (key, value) in &self.other {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

/// Anything but a map reads as no checks, and an entry that is no check is
/// dropped, rather than failing the whole record.
impl<'de> Deserialize<'de> for Checks {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut checks = Self::default();
        let Some(Value::Object(entries)) = Option::<Value>::deserialize(deserializer)? else {
            return Ok(checks);
        };
        for (key, value) in entries {
            match VerifyStage::parse(&key) {
                Some(stage) => {
                    if let Ok(check) = serde_json::from_value::<Check>(value) {
                        checks.set(stage, check);
                    }
                }
                None => {
                    checks.other.insert(key, value);
                }
            }
        }
        Ok(checks)
    }
}

/// Make a record's per-stage checks and the fields older builds read agree.
/// Older builds know only `verifyAfterMerge` and `verifyIn` (here `steps`
/// and `stage`), and carry `checks` and `checksMirror` (here `mirror`:
/// the stage those fields were last written from) through their writes
/// untouched. So what such a build did shows as a difference between the
/// two, and is taken up here:
///
/// 1. Steps in the legacy fields are the check of their stage, whatever
///    `checks` says: an older build set or re-set it. This is also how a
///    record from before `checks` existed is read.
/// 2. A `terminal` record (verified or dropped) without legacy steps owes
///    nothing: an older build discharged what it saw, and this build never
///    writes such a state while a check remains.
/// 3. Otherwise no legacy steps while `mirror` names a stage that still
///    has a check: an older build discharged that one.
///
/// Then the legacy fields are written from the first check, staging before
/// production. Returns the checks rules 2 and 3 dropped. Pure, and applying
/// it twice changes nothing.
pub fn normalize_checks(
    terminal: bool,
    checks: &mut Checks,
    steps: &mut Option<String>,
    stage: &mut Option<VerifyStage>,
    mirror: &mut Option<VerifyStage>,
) -> Vec<(VerifyStage, Check)> {
    let legacy = steps.as_deref().filter(|steps| !steps.trim().is_empty());
    let discharged = if let Some(legacy) = legacy {
        let owed_in = VerifyStage::or_default(*stage);
        if checks
            .get(owed_in)
            .is_none_or(|check| check.steps != legacy)
        {
            checks.set(owed_in, Check::new(legacy));
        }
        Vec::new()
    } else if terminal {
        checks.clear()
    } else {
        mirror
            .and_then(|stage| Some((stage, checks.take(stage)?)))
            .into_iter()
            .collect()
    };
    mirror_checks(checks, steps, stage, mirror);
    discharged
}

/// Write the fields older builds read from the first check. A staging check
/// keeps an absent stage absent, as records from before the stage existed
/// have it.
pub fn mirror_checks(
    checks: &Checks,
    steps: &mut Option<String>,
    stage: &mut Option<VerifyStage>,
    mirror: &mut Option<VerifyStage>,
) {
    match checks.iter().next() {
        Some((first, check)) => {
            *steps = Some(check.steps.clone());
            if first != VerifyStage::Staging || stage.is_some() {
                *stage = Some(first);
            }
            *mirror = Some(first);
        }
        None => {
            *steps = None;
            *stage = None;
            *mirror = None;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkRisk {
    Low,
    Medium,
    High,
}

/// Persisted work status. Unknown keys are retained for forward-compatible
/// read/modify/write operations; timestamps and author do not change claim identity.
///
/// `checks` is what the record owes after merging. `verify_after_merge`
/// and `verify_in` mirror its first check for older builds, and
/// `checks_mirror` says which stage they mirror; see [`normalize_checks`].
/// Reading a record normalizes it. Change the checks with the methods here,
/// not through the fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "WorkStatusRecordRepr")]
pub struct WorkStatusRecord {
    pub state: WorkState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk: Option<WorkRisk>,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_on: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_after_merge: Option<String>,
    /// Absent on records written before the stage existed; those were due
    /// on the base landing, so they read as staging.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_in: Option<VerifyStage>,
    pub checks: Checks,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks_mirror: Option<VerifyStage>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// The record as stored, before its checks are normalized.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkStatusRecordRepr {
    state: WorkState,
    note: Option<String>,
    risk: Option<WorkRisk>,
    at: String,
    sha: Option<String>,
    by: Option<String>,
    blocked_on: Option<String>,
    verify_after_merge: Option<String>,
    #[serde(default, deserialize_with = "lenient_stage")]
    verify_in: Option<VerifyStage>,
    #[serde(default)]
    checks: Checks,
    #[serde(default, deserialize_with = "lenient_stage")]
    checks_mirror: Option<VerifyStage>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl From<WorkStatusRecordRepr> for WorkStatusRecord {
    fn from(repr: WorkStatusRecordRepr) -> Self {
        let mut record = Self {
            state: repr.state,
            note: repr.note,
            risk: repr.risk,
            at: repr.at,
            sha: repr.sha,
            by: repr.by,
            blocked_on: repr.blocked_on,
            verify_after_merge: repr.verify_after_merge,
            verify_in: repr.verify_in,
            checks: repr.checks,
            checks_mirror: repr.checks_mirror,
            extra: repr.extra,
        };
        record.normalize();
        record
    }
}

impl WorkStatusRecord {
    pub fn new(state: WorkState, at: impl Into<String>) -> Self {
        Self {
            state,
            note: None,
            risk: None,
            at: at.into(),
            sha: None,
            by: None,
            blocked_on: None,
            verify_after_merge: None,
            verify_in: None,
            checks: Checks::default(),
            checks_mirror: None,
            extra: BTreeMap::new(),
        }
    }

    /// Whether the state discharges every check: verified or dropped.
    pub const fn is_terminal(&self) -> bool {
        matches!(self.state, WorkState::Verified | WorkState::Dropped)
    }

    /// Reconcile the checks with the fields older builds read; see
    /// [`normalize_checks`]. Returns the checks an older build discharged.
    pub fn normalize(&mut self) -> Vec<(VerifyStage, Check)> {
        normalize_checks(
            self.is_terminal(),
            &mut self.checks,
            &mut self.verify_after_merge,
            &mut self.verify_in,
            &mut self.checks_mirror,
        )
    }

    /// Every check on the record as [`Self::normalize`] would leave them,
    /// whatever the state. A record built by hand with only the legacy
    /// fields set reads the same as one that was parsed.
    pub fn all_checks(&self) -> Checks {
        let mut view = self.clone();
        view.normalize();
        view.checks
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
        let result = edit(&mut self.checks);
        mirror_checks(
            &self.checks,
            &mut self.verify_after_merge,
            &mut self.verify_in,
            &mut self.checks_mirror,
        );
        result
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

    /// The first check on the record, staging before production: its stage
    /// and steps. The single check older builds know.
    pub fn verification(&self) -> Option<(VerifyStage, &str)> {
        let (first, _) = self.all_checks().iter().next()?;
        let legacy = self
            .verify_after_merge
            .as_deref()
            .filter(|steps| !steps.trim().is_empty())
            .filter(|_| VerifyStage::or_default(self.verify_in) == first);
        Some((
            first,
            legacy.or_else(|| Some(self.checks.get(first)?.steps.as_str()))?,
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DerivedState {
    Working,
    Asking,
    Polling,
    Unknown,
    Waiting,
    Abandoned,
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveWorkState {
    pub state: WorkState,
    pub derived: bool,
    pub blocked: bool,
}

pub fn resolve_work_state(input: &str) -> Option<WorkState> {
    let query = input.trim().to_lowercase();
    if query.is_empty() {
        return None;
    }
    let alias = match query.as_str() {
        "nh" | "human" => Some(WorkState::NeedsHuman),
        "nt" | "testing" => Some(WorkState::NeedsTesting),
        _ => None,
    };
    if alias.is_some() {
        return alias;
    }
    let mut matches = WORK_STATES
        .into_iter()
        .filter(|state| state.as_str().starts_with(&query));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

impl WorkState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Working => "working",
            Self::Review => "review",
            Self::NeedsTesting => "needs-testing",
            Self::NeedsHuman => "needs-human",
            Self::Ready => "ready",
            Self::Verified => "verified",
            Self::Dropped => "dropped",
        }
    }
}
impl WorkRisk {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

pub fn same_work_claim(a: &WorkStatusRecord, b: &WorkStatusRecord) -> bool {
    a.state == b.state
        && a.note == b.note
        && a.risk == b.risk
        && a.sha == b.sha
        && a.blocked_on == b.blocked_on
        && a.verify_after_merge == b.verify_after_merge
        && a.verify_in == b.verify_in
        && a.all_checks() == b.all_checks()
}

pub fn work_state_rank(state: Option<WorkState>) -> u8 {
    match state {
        Some(WorkState::Ready) => 0,
        Some(WorkState::NeedsHuman) => 1,
        Some(WorkState::NeedsTesting) => 2,
        Some(WorkState::Review) => 3,
        Some(WorkState::Working) => 4,
        Some(WorkState::Todo) => 7,
        Some(WorkState::Verified | WorkState::Dropped) => 9,
        None => NO_STATUS_RANK,
    }
}

pub fn is_gated(record: Option<&WorkStatusRecord>) -> bool {
    record.is_some_and(|record| {
        record.blocked_on.as_deref().is_some_and(|s| !s.is_empty())
            && matches!(record.state, WorkState::Ready | WorkState::Todo)
    })
}

pub fn work_record_rank(record: Option<&WorkStatusRecord>) -> u8 {
    let Some(record) = record else {
        return NO_STATUS_RANK;
    };
    if !is_gated(Some(record)) {
        return work_state_rank(Some(record.state));
    }
    if record.state == WorkState::Todo {
        GATED_TODO_RANK
    } else {
        BLOCKED_RANK
    }
}

/// A landed branch with an undischarged check in any stage. This holds from
/// the base landing whatever the stage: it is what keeps the worktree from
/// cleanup.
pub fn owes_post_merge_verification(record: Option<&WorkStatusRecord>, landed: bool) -> bool {
    landed && record.is_some_and(|record| !record.open_checks().is_empty())
}

/// How one owed check stands against a landing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckStanding {
    pub stage: VerifyStage,
    /// Its environment has the change and no not-before holds it back: it
    /// can be run now.
    pub due: bool,
    /// Landed in Git on a probed stage that is not running the change yet.
    pub awaiting_deploy: bool,
    /// Put off to a time that has not come.
    pub deferred: bool,
    /// Due for [`VERIFY_OVERDUE_DAYS`] by the Git landing alone.
    pub overdue: bool,
}

/// Where a record's owed check in `stage` stands. A check is due once the
/// branch landed, the landing reached the check's stage (deployed there,
/// when the stage has a probe) and its not-before has passed. A production
/// check on a branch only in staging is owed but not yet due.
pub fn check_standing(
    record: &WorkStatusRecord,
    stage: VerifyStage,
    check: &Check,
    landing: Landing,
    now_ms: i64,
) -> CheckStanding {
    let deferred = check.deferred(now_ms);
    CheckStanding {
        stage,
        due: landing.base && landing.reached(stage) && !deferred,
        awaiting_deploy: landing.base && landing.awaiting_deploy(stage),
        deferred,
        overdue: check_overdue(
            record,
            check,
            stage,
            landing,
            now_ms,
            VERIFY_OVERDUE_DAYS as f64,
        ),
    }
}

/// Every check the record still owes with where it stands, staging first.
pub fn check_standings(
    record: Option<&WorkStatusRecord>,
    landing: Landing,
    now_ms: i64,
) -> Vec<(Check, CheckStanding)> {
    let Some(record) = record else {
        return Vec::new();
    };
    record
        .open_checks()
        .into_iter()
        .map(|(stage, check)| {
            let standing = check_standing(record, stage, &check, landing, now_ms);
            (check, standing)
        })
        .collect()
}

/// The stages whose owed check is due at `now_ms`, staging first.
pub fn checks_due(
    record: Option<&WorkStatusRecord>,
    landing: Landing,
    now_ms: i64,
) -> Vec<VerifyStage> {
    check_standings(record, landing, now_ms)
        .into_iter()
        .filter_map(|(_, standing)| standing.due.then_some(standing.stage))
        .collect()
}

/// The checks `wt status verified` closes: every check that is due, or,
/// when none is, the earliest stage's (its agent may have confirmed the
/// deploy itself). Empty when nothing is owed.
pub fn checks_to_close(
    record: Option<&WorkStatusRecord>,
    landing: Landing,
    now_ms: i64,
) -> Vec<VerifyStage> {
    let due = checks_due(record, landing, now_ms);
    if !due.is_empty() {
        return due;
    }
    record
        .and_then(|record| record.open_checks().into_iter().next())
        .map(|(stage, _)| stage)
        .into_iter()
        .collect()
}

/// Whether any owed check is due at `now_ms`.
pub fn verification_due_at(
    record: Option<&WorkStatusRecord>,
    landing: Landing,
    now_ms: i64,
) -> bool {
    !checks_due(record, landing, now_ms).is_empty()
}

/// The state a row shows: needs-human while its session asks, needs-testing
/// while any owed check is due, else the asserted state.
pub fn effective_work_state_at(
    record: Option<&WorkStatusRecord>,
    session_state: Option<DerivedState>,
    landing: Landing,
    now_ms: i64,
) -> Option<EffectiveWorkState> {
    if session_state == Some(DerivedState::Asking) {
        return Some(EffectiveWorkState {
            state: WorkState::NeedsHuman,
            derived: true,
            blocked: false,
        });
    }
    if verification_due_at(record, landing, now_ms) {
        return Some(EffectiveWorkState {
            state: WorkState::NeedsTesting,
            derived: true,
            blocked: false,
        });
    }
    record.map(|record| EffectiveWorkState {
        state: record.state,
        derived: false,
        blocked: is_gated(Some(record)),
    })
}

/// The stages whose owed check has been due for `days`, staging first.
pub fn checks_overdue_with_days(
    record: Option<&WorkStatusRecord>,
    landing: Landing,
    now_ms: i64,
    days: f64,
) -> Vec<VerifyStage> {
    let Some(record) = record else {
        return Vec::new();
    };
    record
        .open_checks()
        .into_iter()
        .filter(|(stage, check)| check_overdue(record, check, *stage, landing, now_ms, days))
        .map(|(stage, _)| stage)
        .collect()
}

/// Only a due check can be overdue: a production check waiting on a
/// promotion is nobody's lapse yet, and neither is one put off to a later
/// time. Due here means the Git landing: a deploy probe that stopped
/// answering must not hide a check that has been owed for days. The age
/// runs from when the check was first seen due (from the status where that
/// was never recorded), or from the not-before when that is later.
fn check_overdue(
    record: &WorkStatusRecord,
    check: &Check,
    stage: VerifyStage,
    landing: Landing,
    now_ms: i64,
    days: f64,
) -> bool {
    let landing = landing.git_only();
    if !(landing.base && landing.reached(stage)) || check.deferred(now_ms) {
        return false;
    }
    let Some(at) = check.due_since_ms.or_else(|| parse_iso_millis(&record.at)) else {
        return true;
    };
    let since = check
        .not_before_ms
        .map_or(at, |not_before| not_before.max(at));
    (now_ms as f64 - since as f64) >= days * 86_400_000.0
}

pub fn is_work_status_stale(
    record: Option<&WorkStatusRecord>,
    last_commit_ms: Option<i64>,
) -> bool {
    let (Some(record), Some(last_commit_ms)) = (record, last_commit_ms) else {
        return false;
    };
    parse_iso_millis(&record.at).is_some_and(|asserted| last_commit_ms > asserted)
}

pub fn work_age(at: &str, now_ms: i64) -> Option<String> {
    let seconds = now_ms.saturating_sub(parse_iso_millis(at)?) / 1000;
    let seconds = seconds.max(0);
    Some(if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    })
}

/// Tolerant persisted-record parser. Unknown keys are preserved; invalid
/// optional known fields are ignored just as the TypeScript parser does.
pub fn parse_work_status(raw: &Value) -> Option<WorkStatusRecord> {
    let object = raw.as_object()?;
    let state = match object.get("state")?.as_str()? {
        "todo" => WorkState::Todo,
        "working" => WorkState::Working,
        "review" => WorkState::Review,
        "needs-testing" => WorkState::NeedsTesting,
        "needs-human" => WorkState::NeedsHuman,
        "ready" => WorkState::Ready,
        "verified" => WorkState::Verified,
        "dropped" => WorkState::Dropped,
        _ => return None,
    };
    let at = object.get("at")?.as_str()?;
    if at.trim().is_empty() {
        return None;
    }
    let risk = match object.get("risk").and_then(Value::as_str) {
        Some("low") => Some(WorkRisk::Low),
        Some("medium") => Some(WorkRisk::Medium),
        Some("high") => Some(WorkRisk::High),
        _ => None,
    };
    let clean_optional = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(sanitize_work_note)
            .filter(|s| !s.is_empty())
    };
    let sha = object
        .get("sha")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned);
    let by = object
        .get("by")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_owned());
    const KNOWN: &[&str] = &[
        "state",
        "note",
        "risk",
        "at",
        "sha",
        "by",
        "blockedOn",
        "verifyAfterMerge",
        "verifyIn",
        "checks",
        "checksMirror",
    ];
    let extra = object
        .iter()
        .filter(|(key, _)| !KNOWN.contains(&key.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let stage = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .and_then(VerifyStage::parse)
    };
    let mut record = WorkStatusRecord {
        state,
        note: clean_optional("note"),
        risk,
        at: at.into(),
        sha,
        by,
        blocked_on: clean_optional("blockedOn"),
        verify_after_merge: clean_optional("verifyAfterMerge"),
        verify_in: stage("verifyIn"),
        checks: object
            .get("checks")
            .and_then(|checks| serde_json::from_value::<Checks>(checks.clone()).ok())
            .unwrap_or_default()
            .cleaned(sanitize_work_note),
        checks_mirror: stage("checksMirror"),
        extra,
    };
    record.normalize();
    Some(record)
}

/// Remove terminal-control sequences and control bytes, then collapse all
/// whitespace to single spaces.
pub fn sanitize_work_note(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut cleaned = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\x1b' {
            if chars.get(i + 1) == Some(&']') {
                i += 2;
                while i < chars.len() {
                    if chars[i] == '\x07' {
                        i += 1;
                        break;
                    }
                    if chars[i] == '\x1b' && chars.get(i + 1) == Some(&'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            } else if chars.get(i + 1) == Some(&'[') {
                i += 2;
                while i < chars.len()
                    && (chars[i].is_ascii_digit() || matches!(chars[i], ';' | '?'))
                {
                    i += 1;
                }
                while i < chars.len() && (' '..='/').contains(&chars[i]) {
                    i += 1;
                }
                if i < chars.len() && ('@'..='~').contains(&chars[i]) {
                    i += 1;
                }
            } else if matches!(chars.get(i + 1), Some('(' | ')'))
                && chars.get(i + 2).is_some_and(char::is_ascii_alphanumeric)
            {
                i += 3;
            } else {
                i = (i + 2).min(chars.len());
            }
            continue;
        }
        let ch = chars[i];
        if ch.is_control() || ('\u{80}'..='\u{9f}').contains(&ch) {
            cleaned.push(' ');
        } else {
            cleaned.push(ch);
        }
        i += 1;
    }
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sanitization for untrusted text rendered in terminal-facing UI surfaces.
/// Currently shares the work-status note contract: remove ANSI/control
/// sequences and collapse multiline input into one safe line.
pub fn sanitize_terminal_text(input: &str) -> String {
    sanitize_work_note(input)
}

/// [`sanitize_terminal_text`] for text whose line breaks mean something (a
/// prompt): each line is cleaned by itself, and blank lines around the text
/// are dropped.
pub fn sanitize_terminal_lines(input: &str) -> String {
    let lines: Vec<String> = input.lines().map(sanitize_work_note).collect();
    let said = |line: &String| !line.is_empty();
    match (lines.iter().position(said), lines.iter().rposition(said)) {
        (Some(first), Some(last)) => lines[first..=last].join("\n"),
        _ => String::new(),
    }
}

pub fn work_status_suffix(record: &WorkStatusRecord) -> String {
    let risk = record
        .risk
        .map(|risk| format!(" (risk: {})", risk.as_str()))
        .unwrap_or_default();
    let gate = record
        .blocked_on
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!(" [blocked on: {}]", clamp_for_line(s, 120, 60)))
        .unwrap_or_default();
    let verify: String = record
        .all_checks()
        .iter()
        .map(|(stage, check)| {
            format!(
                " [verify in {}: {}]",
                stage.as_str(),
                clamp_for_line(&check.steps, 120, 35)
            )
        })
        .collect();
    let note = record
        .note
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!(" — {}", clamp_for_line(s, 400, 200)))
        .unwrap_or_default();
    format!("{risk}{gate}{verify}{note}")
}

pub fn verify_steps_headline(steps: &str) -> String {
    clamp_for_line(steps, 120, 35)
}

fn clamp_for_line(text: &str, max: usize, sentence_floor: usize) -> String {
    if text.encode_utf16().count() <= max {
        return text.to_owned();
    }
    let mut prefix = String::new();
    for ch in text.chars() {
        if prefix.encode_utf16().count() + ch.len_utf16() > max {
            break;
        }
        prefix.push(ch);
    }
    let chars: Vec<char> = prefix.chars().collect();
    let mut sentence_cut = None;
    for i in 0..chars.len().saturating_sub(1) {
        if matches!(chars[i], '.' | '!' | '?') && chars[i + 1].is_whitespace() {
            let length: usize = chars[..=i].iter().map(|c| c.len_utf16()).sum();
            if length >= sentence_floor {
                sentence_cut = Some(i + 1);
            }
        }
    }
    let cut = if let Some(end) = sentence_cut {
        chars[..end].iter().collect::<String>()
    } else if let Some(last_space) = chars.iter().rposition(|ch| ch.is_whitespace()) {
        chars[..last_space]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_owned()
    } else {
        prefix.trim_end().to_owned()
    };
    let cut = cut.strip_suffix('.').unwrap_or(&cut);
    format!("{cut}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shorthands for the assertions below: the single-answer and clockless
    // readings of the per-stage functions.
    fn verification_due(record: Option<&WorkStatusRecord>, landing: Landing) -> bool {
        verification_due_at(record, landing, i64::MIN)
    }

    fn checks_awaiting_deploy(
        record: Option<&WorkStatusRecord>,
        landing: Landing,
    ) -> Vec<VerifyStage> {
        check_standings(record, landing, i64::MAX)
            .into_iter()
            .filter_map(|(_, standing)| standing.awaiting_deploy.then_some(standing.stage))
            .collect()
    }

    fn verification_awaiting_deploy(
        record: Option<&WorkStatusRecord>,
        landing: Landing,
    ) -> Option<VerifyStage> {
        checks_awaiting_deploy(record, landing).into_iter().next()
    }

    fn effective_work_state(
        record: Option<&WorkStatusRecord>,
        session_state: Option<DerivedState>,
        landing: Landing,
    ) -> Option<EffectiveWorkState> {
        effective_work_state_at(record, session_state, landing, i64::MIN)
    }

    fn verification_overdue(
        record: Option<&WorkStatusRecord>,
        landing: Landing,
        now_ms: i64,
    ) -> bool {
        verification_overdue_with_days(record, landing, now_ms, VERIFY_OVERDUE_DAYS as f64)
    }

    fn verification_overdue_with_days(
        record: Option<&WorkStatusRecord>,
        landing: Landing,
        now_ms: i64,
        days: f64,
    ) -> bool {
        !checks_overdue_with_days(record, landing, now_ms, days).is_empty()
    }
    use serde_json::json;

    const AT: &str = "2026-08-20T12:00:00.000Z";
    fn record(state: WorkState) -> WorkStatusRecord {
        WorkStatusRecord::new(state, AT)
    }

    #[test]
    fn state_prefixes_aliases_and_ambiguity_match_cli_rules() {
        assert_eq!(resolve_work_state(" NH "), Some(WorkState::NeedsHuman));
        assert_eq!(resolve_work_state("testing"), Some(WorkState::NeedsTesting));
        assert_eq!(resolve_work_state("rev"), Some(WorkState::Review));
        assert_eq!(resolve_work_state("r"), None);
        assert_eq!(resolve_work_state("needs-"), None);
        assert_eq!(resolve_work_state(""), None);
    }

    #[test]
    fn gate_ranks_and_effective_state_keep_gate_separate_from_work_state() {
        let mut ready = record(WorkState::Ready);
        ready.blocked_on = Some("mobile release".into());
        assert!(is_gated(Some(&ready)));
        assert_eq!(work_record_rank(Some(&ready)), BLOCKED_RANK);
        assert_eq!(
            effective_work_state(Some(&ready), Some(DerivedState::Asking), Landing::NONE),
            Some(EffectiveWorkState {
                state: WorkState::NeedsHuman,
                derived: true,
                blocked: false
            })
        );
        let mut todo = record(WorkState::Todo);
        todo.blocked_on = Some("secrets land".into());
        assert_eq!(work_record_rank(Some(&todo)), GATED_TODO_RANK);
        for state in [
            WorkState::Working,
            WorkState::Review,
            WorkState::NeedsHuman,
            WorkState::Verified,
            WorkState::Dropped,
        ] {
            assert!(!is_gated(Some(&WorkStatusRecord {
                blocked_on: Some("x".into()),
                ..record(state)
            })));
        }
    }

    #[test]
    fn post_merge_verification_is_dormant_until_landing_then_ages_out() {
        let landed = Landing::new(true, false, false);
        let mut ready = record(WorkState::Ready);
        ready.verify_after_merge = Some("run deployed flow".into());
        assert!(!owes_post_merge_verification(Some(&ready), false));
        assert!(owes_post_merge_verification(Some(&ready), true));
        assert_eq!(
            effective_work_state(Some(&ready), None, landed)
                .unwrap()
                .state,
            WorkState::NeedsTesting
        );
        let now = parse_iso_millis("2026-08-22T12:00:00Z").unwrap();
        assert!(verification_overdue_with_days(
            Some(&ready),
            landed,
            now,
            2.0
        ));
        ready.state = WorkState::Verified;
        assert!(!verification_overdue(Some(&ready), landed, now));
        ready.state = WorkState::Dropped;
        assert!(!owes_post_merge_verification(Some(&ready), true));
        ready.state = WorkState::Ready;
        ready.at = "invalid".into();
        assert!(verification_overdue(Some(&ready), landed, now));
    }

    #[test]
    fn a_production_check_waits_for_promotion_but_still_holds_the_worktree() {
        let staged = Landing::new(true, false, true);
        let promoted = Landing::new(true, true, true);
        let mut ready = record(WorkState::Ready);
        ready.verify_after_merge = Some("check prod".into());
        ready.verify_in = Some(VerifyStage::Production);
        assert!(owes_post_merge_verification(Some(&ready), staged.base));
        assert!(!verification_due(Some(&ready), staged));
        assert_eq!(
            effective_work_state(Some(&ready), None, staged)
                .unwrap()
                .state,
            WorkState::Ready
        );
        let now = parse_iso_millis("2026-09-22T12:00:00Z").unwrap();
        assert!(!verification_overdue(Some(&ready), staged, now));
        assert!(verification_due(Some(&ready), promoted));
        assert!(verification_overdue(Some(&ready), promoted, now));
        // Without a separate production branch the base landing ships it.
        assert!(verification_due(
            Some(&ready),
            Landing::new(true, false, false)
        ));
        // Legacy records (no stage) were due on the base landing.
        ready.verify_in = None;
        assert!(verification_due(Some(&ready), staged));
        assert_eq!(
            parse_work_status(
                &json!({"state":"ready","at":AT,"verifyAfterMerge":"x","verifyIn":"production"})
            )
            .unwrap()
            .verify_in,
            Some(VerifyStage::Production)
        );
        let future: WorkStatusRecord = serde_json::from_value(
            json!({"state":"ready","at":AT,"verifyAfterMerge":"x","verifyIn":"canary"}),
        )
        .unwrap();
        assert_eq!(future.verify_in, None);
    }

    #[test]
    fn a_probed_stage_is_reached_only_once_deployed() {
        use VerifyStage::{Production, Staging};
        let deploys = |staging, production| StageDeploys {
            staging,
            production,
        };
        // (base, in production, separate production branch)
        let staged = Landing::new(true, false, true);
        let promoted = Landing::new(true, true, true);
        let released = Landing::new(true, false, false);
        // No probe: the Git landing decides, as before.
        for landing in [Landing::NONE, staged, promoted, released] {
            for stage in [Staging, Production] {
                assert_eq!(landing.reached(stage), landing.landed(stage));
                assert!(!landing.awaiting_deploy(stage));
            }
        }
        for (landing, stage, proof, reached, awaiting) in [
            (staged, Staging, Some(false), false, true),
            (staged, Staging, Some(true), true, false),
            // A deploy cannot stand in for a landing Git does not show.
            (staged, Production, Some(true), false, false),
            (staged, Production, Some(false), false, false),
            (promoted, Production, Some(false), false, true),
            (promoted, Production, Some(true), true, false),
            (released, Production, Some(false), false, true),
            (released, Production, Some(true), true, false),
            (Landing::NONE, Staging, Some(true), false, false),
        ] {
            let mut facts = StageDeploys::UNPROBED;
            facts.set(stage, proof);
            let landing = landing.with_deploys(facts);
            assert_eq!(landing.reached(stage), reached, "{landing:?} {stage:?}");
            assert_eq!(landing.awaiting_deploy(stage), awaiting);
        }
        // Facts nobody computed are not facts about a stage without a probe.
        let only_staging = |stage| stage == Staging;
        assert_eq!(
            StageDeploys::default().for_probes(only_staging),
            deploys(Some(false), None)
        );
        assert_eq!(
            deploys(Some(true), Some(true)).for_probes(only_staging),
            deploys(Some(true), None)
        );
        assert!(
            !staged
                .with_deploys(StageDeploys::default().for_probes(only_staging))
                .reached(Staging)
        );
        // One stage's probe says nothing about the other.
        let mixed = promoted.with_deploys(deploys(Some(false), None));
        assert!(!mixed.reached(Staging) && mixed.reached(Production));

        let mut ready = record(WorkState::Ready);
        ready.verify_after_merge = Some("check staging".into());
        let waiting = staged.with_deploys(deploys(Some(false), None));
        assert!(owes_post_merge_verification(Some(&ready), waiting.base));
        assert!(!verification_due(Some(&ready), waiting));
        assert_eq!(
            verification_awaiting_deploy(Some(&ready), waiting),
            Some(Staging)
        );
        assert_eq!(
            effective_work_state(Some(&ready), None, waiting)
                .unwrap()
                .state,
            WorkState::Ready
        );
        let live = staged.with_deploys(deploys(Some(true), None));
        assert!(verification_due(Some(&ready), live));
        assert_eq!(verification_awaiting_deploy(Some(&ready), live), None);
        // A probe that never answers cannot hide a check owed for days.
        let now = parse_iso_millis(AT).unwrap() + 3 * 86_400_000;
        assert!(verification_overdue(Some(&ready), waiting, now));
        assert!(!verification_overdue(Some(&ready), Landing::NONE, now));
        ready.state = WorkState::Verified;
        assert_eq!(verification_awaiting_deploy(Some(&ready), waiting), None);
        assert_eq!(Staging.deployed_ref(), "refs/wt/deployed/staging");
    }

    #[test]
    fn parse_sanitizes_known_fields_and_retains_unknown_json() {
        let parsed = parse_work_status(&json!({"state":"ready", "at":AT, "risk":"yolo", "note":"\u{001b}[31mred\u{001b}[0m\ntext", "blockedOn":"\u{001b}]0;title\u{0007}release", "future":{"x":1}})).unwrap();
        assert_eq!(parsed.note.as_deref(), Some("red text"));
        assert_eq!(parsed.blocked_on.as_deref(), Some("release"));
        assert_eq!(parsed.risk, None);
        assert_eq!(serde_json::to_value(parsed).unwrap()["future"]["x"], 1);
        assert!(parse_work_status(&json!({"state":"shipped", "at":AT})).is_none());
        assert!(parse_work_status(&json!({"state":"ready", "at":"  "})).is_none());
        let invalid_optional = parse_work_status(
            &json!({"state":"ready", "at":AT, "blockedOn":7, "verifyAfterMerge":false}),
        )
        .unwrap();
        assert_eq!(invalid_optional.blocked_on, None);
        assert_eq!(invalid_optional.verify_after_merge, None);
    }

    #[test]
    fn claim_equality_ignores_timestamp_author_but_tracks_gate_and_verify() {
        let a = record(WorkState::Ready);
        let mut b = a.clone();
        b.at = "different".into();
        b.by = Some("other".into());
        assert!(same_work_claim(&a, &b));
        b.blocked_on = Some("gate".into());
        assert!(!same_work_claim(&a, &b));
        b = a.clone();
        b.verify_after_merge = Some("steps".into());
        assert!(!same_work_claim(&a, &b));
        b = a.clone();
        b.verify_in = Some(VerifyStage::Production);
        assert!(!same_work_claim(&a, &b));
        b = a.clone();
        b.extra
            .insert("futureField".into(), serde_json::json!("new value"));
        assert!(same_work_claim(&a, &b));
    }

    #[test]
    fn age_staleness_and_suffix_are_bounded() {
        let now = parse_iso_millis("2026-08-20T12:00:30Z").unwrap();
        assert_eq!(work_age(AT, now).as_deref(), Some("30s"));
        let mut rec = record(WorkState::Ready);
        rec.risk = Some(WorkRisk::Low);
        rec.blocked_on = Some("release".into());
        rec.note = Some("note".into());
        assert_eq!(
            work_status_suffix(&rec),
            " (risk: low) [blocked on: release] — note"
        );
        let long_note = WorkStatusRecord {
            note: Some("x".repeat(900)),
            ..rec.clone()
        };
        let suffix = work_status_suffix(&long_note);
        assert!(suffix.ends_with("..."));
        assert!(suffix.encode_utf16().count() < 450);
        let short_sentence = format!("Yes. {}", "word ".repeat(60));
        assert!(verify_steps_headline(&short_sentence).starts_with("Yes. word word"));
        assert_eq!(
            verify_steps_headline(&format!(
                "Only a real callback proves this. {}",
                "x".repeat(300)
            )),
            "Only a real callback proves this..."
        );
        assert!(is_work_status_stale(Some(&rec), Some(now)));
        assert!(!is_work_status_stale(Some(&rec), None));
        assert!(verify_steps_headline(&"A complete sentence. ".repeat(10)).ends_with("..."));
    }

    const STAGING: VerifyStage = VerifyStage::Staging;
    const PRODUCTION: VerifyStage = VerifyStage::Production;

    fn stages(record: &WorkStatusRecord) -> Vec<(VerifyStage, String)> {
        record
            .open_checks()
            .into_iter()
            .map(|(stage, check)| (stage, check.steps))
            .collect()
    }

    /// What a build from before `checks` existed does to a stored record:
    /// it rewrites the fields it knows and carries the rest through.
    fn old_build_writes(stored: &Value, legacy: Value) -> Value {
        let mut next = legacy;
        for key in ["checks", "checksMirror"] {
            if let Some(kept) = stored.get(key) {
                next[key] = kept.clone();
            }
        }
        next
    }

    #[test]
    fn a_record_from_before_checks_imports_its_one_check() {
        let stored = json!({"state": "ready", "at": AT, "verifyAfterMerge": "open the page"});
        for record in [
            parse_work_status(&stored).unwrap(),
            serde_json::from_value::<WorkStatusRecord>(stored.clone()).unwrap(),
        ] {
            assert_eq!(stages(&record), [(STAGING, "open the page".to_owned())]);
            assert_eq!(record.checks_mirror, Some(STAGING));
            // The stage was absent and stays absent: it reads as staging.
            assert_eq!(record.verify_in, None);
            let written = serde_json::to_value(&record).unwrap();
            assert_eq!(
                written["checks"],
                json!({"staging": {"steps": "open the page"}})
            );
            assert_eq!(written["checksMirror"], "staging");
            assert_eq!(written["verifyAfterMerge"], "open the page");
        }
        let production = json!({
            "state": "ready", "at": AT, "verifyAfterMerge": "watch the logs", "verifyIn": "production",
        });
        let record = parse_work_status(&production).unwrap();
        assert_eq!(stages(&record), [(PRODUCTION, "watch the logs".to_owned())]);
        assert_eq!(record.verification(), Some((PRODUCTION, "watch the logs")));
    }

    #[test]
    fn checks_are_always_written_and_normalizing_twice_changes_nothing() {
        let record = record(WorkState::Ready);
        assert_eq!(serde_json::to_value(&record).unwrap()["checks"], json!({}));
        let mut both = record
            .with_check(PRODUCTION, "watch the logs")
            .with_check(STAGING, "open the page");
        assert_eq!(both.verification(), Some((STAGING, "open the page")));
        assert_eq!(both.verify_in, Some(STAGING));
        assert_eq!(both.checks_mirror, Some(STAGING));
        let once = both.clone();
        assert!(both.normalize().is_empty());
        assert_eq!(both, once);
        let read: WorkStatusRecord =
            serde_json::from_value(serde_json::to_value(&both).unwrap()).unwrap();
        assert_eq!(read, both);
        assert_eq!(
            parse_work_status(&serde_json::to_value(&both).unwrap()),
            Some(both)
        );
    }

    #[test]
    fn the_mirror_follows_the_first_open_check() {
        let mut record = record(WorkState::Ready)
            .with_check(STAGING, "open the page")
            .with_check(PRODUCTION, "watch the logs");
        assert_eq!(record.drop_check(STAGING).unwrap().steps, "open the page");
        assert_eq!(record.verify_after_merge.as_deref(), Some("watch the logs"));
        assert_eq!(record.verify_in, Some(PRODUCTION));
        assert_eq!(record.checks_mirror, Some(PRODUCTION));
        assert_eq!(record.clear_checks().len(), 1);
        assert_eq!(
            (
                record.verify_after_merge.clone(),
                record.verify_in,
                record.checks_mirror
            ),
            (None, None, None)
        );
        assert_eq!(record.verification(), None);
    }

    #[test]
    fn an_older_build_discharging_its_check_drops_only_that_stage() {
        let record = record(WorkState::Ready)
            .with_check(STAGING, "open the page")
            .with_check(PRODUCTION, "watch the logs");
        let stored = serde_json::to_value(&record).unwrap();
        // It asserts a fresh status without the check it knew.
        let written = old_build_writes(&stored, json!({"state": "ready", "at": AT}));
        let mut read = parse_work_status(&written).unwrap();
        assert_eq!(stages(&read), [(PRODUCTION, "watch the logs".to_owned())]);
        assert_eq!(read.checks_mirror, Some(PRODUCTION));
        assert_eq!(read.verify_after_merge.as_deref(), Some("watch the logs"));
        // The typed path reports what was discharged.
        let mut raw = WorkStatusRecord::new(WorkState::Ready, AT);
        raw.checks = record.checks.clone();
        raw.checks_mirror = Some(STAGING);
        let discharged = raw.normalize();
        assert_eq!(discharged.len(), 1);
        assert_eq!(discharged[0].0, STAGING);
        assert_eq!(raw, read);
        assert!(read.normalize().is_empty());
    }

    #[test]
    fn an_older_build_verifying_discharges_every_check() {
        let record = record(WorkState::Ready)
            .with_check(STAGING, "open the page")
            .with_check(PRODUCTION, "watch the logs");
        let stored = serde_json::to_value(&record).unwrap();
        for state in ["verified", "dropped"] {
            let written = old_build_writes(&stored, json!({"state": state, "at": AT}));
            let read = parse_work_status(&written).unwrap();
            assert!(read.checks.is_empty(), "{state}");
            assert_eq!(read.checks_mirror, None);
            assert_eq!(serde_json::to_value(&read).unwrap()["checks"], json!({}));
        }
    }

    #[test]
    fn an_older_build_setting_a_check_again_sets_that_stage() {
        let mut record = record(WorkState::Ready)
            .with_check(STAGING, "open the page")
            .with_check(PRODUCTION, "watch the logs");
        assert!(record.defer_check(PRODUCTION, Some(5)));
        let stored = serde_json::to_value(&record).unwrap();
        // The same steps carried forward change nothing, a not-before included.
        let carried = old_build_writes(
            &stored,
            json!({"state": "review", "at": AT, "verifyAfterMerge": "open the page"}),
        );
        let read = parse_work_status(&carried).unwrap();
        assert_eq!(read.checks, record.checks);
        // New steps for production replace that stage's check alone; the
        // fields it reads go back to the first open check.
        let reset = old_build_writes(
            &stored,
            json!({
                "state": "ready", "at": AT,
                "verifyAfterMerge": "tail the worker", "verifyIn": "production",
            }),
        );
        let read = parse_work_status(&reset).unwrap();
        assert_eq!(
            stages(&read),
            [
                (STAGING, "open the page".to_owned()),
                (PRODUCTION, "tail the worker".to_owned()),
            ]
        );
        assert_eq!(read.open_check(PRODUCTION).unwrap().not_before_ms, None);
        assert_eq!(read.verify_after_merge.as_deref(), Some("open the page"));
        assert_eq!(read.verify_in, Some(STAGING));
        // A record it builds from nothing keeps what it does not know only
        // through the unknown keys it preserved; with none, the import is all.
        let fresh =
            json!({"state": "ready", "at": AT, "verifyAfterMerge": "x", "verifyIn": "production"});
        assert_eq!(
            stages(&parse_work_status(&fresh).unwrap()),
            [(PRODUCTION, "x".to_owned())]
        );
    }

    #[test]
    fn a_hand_built_record_with_only_the_legacy_fields_reads_like_a_parsed_one() {
        let mut ready = record(WorkState::Ready);
        ready.verify_after_merge = Some("open the page".into());
        ready.verify_in = Some(PRODUCTION);
        assert_eq!(stages(&ready), [(PRODUCTION, "open the page".to_owned())]);
        assert_eq!(ready.verification(), Some((PRODUCTION, "open the page")));
        let mut parsed = ready.clone();
        parsed.normalize();
        assert!(same_work_claim(&ready, &parsed));
        assert!(!same_work_claim(&ready, &record(WorkState::Ready)));
    }

    #[test]
    fn malformed_and_unknown_checks_do_not_fail_the_record() {
        let stored = json!({
            "state": "ready", "at": AT,
            "checks": {
                "staging": {"steps": "open\u{1b}[31m the page", "notBeforeMs": 7, "later": true},
                "production": "not a check",
                "preview": {"steps": "from a newer build"},
            },
            "checksMirror": "preview",
        });
        let record = parse_work_status(&stored).unwrap();
        let staging = record.open_check(STAGING).unwrap();
        assert_eq!(staging.steps, "open the page");
        assert_eq!(staging.not_before_ms, Some(7));
        assert_eq!(staging.extra["later"], true);
        assert_eq!(record.open_check(PRODUCTION), None);
        let written = serde_json::to_value(&record).unwrap();
        assert_eq!(
            written["checks"]["preview"],
            json!({"steps": "from a newer build"})
        );
        assert!(parse_work_status(&json!({"state": "ready", "at": AT, "checks": 3})).is_some());
        assert!(
            serde_json::from_value::<WorkStatusRecord>(
                json!({"state": "ready", "at": AT, "checks": [1]})
            )
            .is_ok()
        );
    }

    #[test]
    fn each_stage_comes_due_by_itself_and_a_fresh_assertion_carries_both() {
        let ready = record(WorkState::Ready)
            .with_check(STAGING, "open the page")
            .with_check(PRODUCTION, "watch the logs");
        let now = parse_iso_millis(AT).unwrap() + 1_000;
        let staged = Landing::new(true, false, true);
        let promoted = Landing::new(true, true, true);
        assert_eq!(checks_due(Some(&ready), Landing::NONE, now), []);
        assert_eq!(checks_due(Some(&ready), staged, now), [STAGING]);
        assert_eq!(
            checks_due(Some(&ready), promoted, now),
            [STAGING, PRODUCTION]
        );
        assert_eq!(
            effective_work_state_at(Some(&ready), None, staged, now)
                .unwrap()
                .state,
            WorkState::NeedsTesting
        );
        // Verified closes what is due; with nothing due, the earliest stage.
        assert_eq!(checks_to_close(Some(&ready), staged, now), [STAGING]);
        assert_eq!(
            checks_to_close(Some(&ready), promoted, now),
            [STAGING, PRODUCTION]
        );
        assert_eq!(checks_to_close(Some(&ready), Landing::NONE, now), [STAGING]);
        assert_eq!(
            checks_to_close(Some(&record(WorkState::Ready)), promoted, now),
            []
        );
        let mut after_staging = ready.clone();
        after_staging.drop_check(STAGING);
        assert_eq!(checks_due(Some(&after_staging), staged, now), []);
        assert!(owes_post_merge_verification(Some(&after_staging), true));
        assert_eq!(
            checks_to_close(Some(&after_staging), staged, now),
            [PRODUCTION]
        );
        // A fresh assertion sets one stage and keeps the other.
        let mut fresh = record(WorkState::Review).with_check(STAGING, "new steps");
        fresh.carry_checks(&ready);
        assert_eq!(
            stages(&fresh),
            [
                (STAGING, "new steps".to_owned()),
                (PRODUCTION, "watch the logs".to_owned())
            ]
        );
        // Deploy probes are per stage.
        let waiting = promoted.with_deploys(StageDeploys {
            staging: Some(true),
            production: Some(false),
        });
        assert_eq!(checks_due(Some(&ready), waiting, now), [STAGING]);
        assert_eq!(checks_awaiting_deploy(Some(&ready), waiting), [PRODUCTION]);
    }

    #[test]
    fn a_check_seen_due_is_overdue_from_then_however_late_the_status_is() {
        let at = parse_iso_millis(AT).unwrap();
        let two_days = 2 * 86_400_000;
        let landed = Landing::new(true, false, true);
        // First seen due at AT; the status was asserted again a day later.
        let reasserted = parse_work_status(&serde_json::json!({
            "state": "ready",
            "at": "2026-08-21T12:00:00.000Z",
            "checks": {"staging": {"steps": "open the page", "dueSinceMs": at}},
        }))
        .unwrap();
        let check = reasserted.open_check(STAGING).unwrap();
        assert_eq!(check.due_at_ms(), Some(at));
        assert!(!verification_overdue(
            Some(&reasserted),
            landed,
            at + two_days - 1
        ));
        assert!(verification_overdue(
            Some(&reasserted),
            landed,
            at + two_days
        ));
        // The list says since when it has been due.
        let items = crate::owed_items(Some(&reasserted), &landed, &[], &[], at + 1);
        assert_eq!(items[0].due_at_ms, Some(at));
        // The dates survive a write by a build that does not know them:
        // the check as that build declared it, which keeps unknown keys.
        #[derive(Serialize, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct OldCheck {
            steps: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            not_before_ms: Option<i64>,
            #[serde(flatten)]
            extra: BTreeMap<String, Value>,
        }
        let mut dated = check.clone();
        dated.since_ms = Some(at - 5);
        dated.due_pr = Some(7);
        let old: OldCheck = serde_json::from_value(serde_json::to_value(&dated).unwrap()).unwrap();
        assert_eq!(old.steps, "open the page");
        assert_eq!(old.not_before_ms, None);
        let rewritten: Check = serde_json::from_value(serde_json::to_value(&old).unwrap()).unwrap();
        assert_eq!(rewritten, dated);
        assert_eq!(
            (rewritten.due_since_ms, rewritten.since_ms, rewritten.due_pr),
            (Some(at), Some(at - 5), Some(7))
        );
    }

    #[test]
    fn a_check_put_off_is_owed_but_not_due_or_overdue_until_its_time() {
        let mut ready = record(WorkState::Ready).with_check(STAGING, "open the page");
        let at = parse_iso_millis(AT).unwrap();
        let later = at + 3 * 3_600_000;
        assert!(ready.defer_check(STAGING, Some(later)));
        assert!(!ready.defer_check(PRODUCTION, Some(later)));
        let landed = Landing::new(true, false, true);
        assert!(owes_post_merge_verification(Some(&ready), true));
        assert!(!verification_due_at(Some(&ready), landed, later - 1));
        assert!(verification_due_at(Some(&ready), landed, later));
        // Without a clock a check that was put off is not due.
        assert!(!verification_due(Some(&ready), landed));
        assert_eq!(
            effective_work_state(Some(&ready), None, landed)
                .unwrap()
                .state,
            WorkState::Ready
        );
        // The overdue age runs from the not-before.
        let two_days = 2 * 86_400_000;
        assert!(!verification_overdue(Some(&ready), landed, at + two_days));
        assert!(verification_overdue(Some(&ready), landed, later + two_days));
        // Setting the same steps keeps it; new steps are a new check.
        ready.set_check(STAGING, "open the page");
        assert_eq!(
            ready.open_check(STAGING).unwrap().not_before_ms,
            Some(later)
        );
        ready.set_check(STAGING, "open the other page");
        assert_eq!(ready.open_check(STAGING).unwrap().not_before_ms, None);
    }

    #[test]
    fn the_suffix_names_every_stage() {
        let ready = record(WorkState::Ready)
            .with_check(STAGING, "open the page")
            .with_check(PRODUCTION, "watch the logs");
        assert_eq!(
            work_status_suffix(&ready),
            " [verify in staging: open the page] [verify in production: watch the logs]"
        );
    }
}
