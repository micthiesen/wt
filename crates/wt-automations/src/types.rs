use std::collections::{BTreeMap, BTreeSet};

use wt_config::{AutomationDef, AutomationTrigger};
use wt_core::{WorkRisk, WorkState, WorkStatusRecord};

/// Inputs are prepared by the application from current Git, GitHub and store
/// snapshots. No source or persistence is touched by the evaluator.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutomationRow {
    pub slug: String,
    pub branch: String,
    pub archived: bool,
    pub busy: bool,
    pub created_at: Option<String>,
    pub local_merged: Option<bool>,
    pub gone: Option<bool>,
    /// The lifecycle's complete clean-safety predicate, including the
    /// non-vacuous owned-commit guard.
    pub clean_candidate: bool,
    pub pr: Option<AutomationPr>,
    pub conflict: Option<AutomationConflict>,
    pub work: Option<WorkStatusRecord>,
    pub github_issue: Option<u64>,
    pub issue_id: Option<String>,
    pub stack: Option<AutomationStack>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutomationPr {
    pub number: u64,
    pub state: String,
    pub head_sha: Option<String>,
    pub is_draft: bool,
    pub checks_failed: bool,
    pub failed_checks: Vec<String>,
    pub review_bot_unresolved: u32,
    pub changes_requested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutomationConflict {
    pub base: String,
    pub conflicted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutomationStack {
    pub id: String,
    pub parent: Option<StackParent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackParent {
    pub branch: String,
    pub slug: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PauseSnapshot {
    pub global: bool,
    pub slugs: BTreeSet<String>,
    pub stack_ids: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActionAudience {
    #[default]
    None,
    Session,
    Manager,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionTraits {
    pub shell: bool,
    pub external: bool,
    pub audience: ActionAudience,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchTip {
    pub now: String,
    pub seen: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AutomationEvalContext {
    pub github_fresh: bool,
    pub now_ms: i64,
    /// Local date `YYYY-MM-DD`, calculated by the app in its configured zone.
    pub local_day: String,
    pub base_branch: String,
    pub reviewers_enabled: bool,
    pub actions: BTreeMap<String, ActionTraits>,
    pub pauses: PauseSnapshot,
    pub branch_tips: BTreeMap<String, BranchTip>,
    /// Frozen template values for post-merge external fires.
    pub row_vars: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchRange {
    pub branch: String,
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenPr {
    pub state: String,
    pub is_draft: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutomationFire {
    pub rule: AutomationDef,
    pub slug: String,
    pub quiesce_slugs: Vec<String>,
    pub fire_keys: Vec<String>,
    pub stack_id: Option<String>,
    pub close_issue: Option<u64>,
    pub delete_branch: Option<String>,
    pub delete_branch_pr: Option<u64>,
    pub branch_range: Option<BranchRange>,
    pub frozen_vars: Option<BTreeMap<String, String>>,
    pub frozen_pr: Option<FrozenPr>,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutomationIntent {
    pub fire: AutomationFire,
    pub queued_at_ms: i64,
    pub settle_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchDisposition {
    /// No external durable effect was attempted; safe to release the key.
    NotStarted,
    /// A matching action run proves ownership/delivery after restart.
    Delivered,
    /// A durable send may have happened, so it must never be blindly replayed.
    Ambiguous(String),
}

pub fn status_trigger_state(trigger: AutomationTrigger) -> Option<WorkState> {
    match trigger {
        AutomationTrigger::StatusNeedsHuman => Some(WorkState::NeedsHuman),
        AutomationTrigger::StatusNeedsTesting => Some(WorkState::NeedsTesting),
        AutomationTrigger::StatusReady => Some(WorkState::Ready),
        _ => None,
    }
}

pub fn status_suffix(work: &WorkStatusRecord) -> String {
    let mut suffix = String::new();
    if let Some(risk) = work.risk {
        let risk = match risk {
            WorkRisk::Low => "low",
            WorkRisk::Medium => "medium",
            WorkRisk::High => "high",
        };
        suffix.push_str(&format!(" (risk: {risk})"));
    }
    if let Some(blocked) = work.blocked_on.as_deref().filter(|text| !text.is_empty()) {
        suffix.push_str(&format!(" [blocked on: {blocked}]"));
    }
    if let Some(steps) = work
        .verify_after_merge
        .as_deref()
        .filter(|text| !text.is_empty())
    {
        suffix.push_str(&format!(" [verify after merge: {steps}]"));
    }
    if let Some(note) = work.note.as_deref().filter(|text| !text.is_empty()) {
        suffix.push_str(&format!(" — {note}"));
    }
    suffix
}

pub fn status_is_gated(work: &WorkStatusRecord) -> bool {
    work.blocked_on
        .as_ref()
        .is_some_and(|blocked| !blocked.is_empty())
        && matches!(work.state, WorkState::Ready | WorkState::Todo)
}
