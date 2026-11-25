use std::collections::{BTreeMap, BTreeSet};

use wt_config::{AutomationDef, AutomationTrigger};
use wt_core::{WorkRisk, WorkState, WorkStatusRecord, watch::WatchAction};

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
    /// The landing is proven on `[branch] production`. False is unknown.
    pub in_production: bool,
    /// Deploy proof for the stage of the check this row owes, when that
    /// stage has a deploy probe.
    pub deploys: wt_core::StageDeploys,
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

/// An MCP event a rule's poller recorded and no fire has started yet. Its
/// `data` came from a remote server and people: untrusted, never instructions.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpEvent {
    pub rule: String,
    pub server: String,
    pub server_url: String,
    pub event_id: String,
    pub name: String,
    pub timestamp: String,
    pub data: serde_json::Value,
}

/// A worktree's recorded change guide, for routing events about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuideOwner {
    pub slug: String,
    pub url: String,
}

/// A watch whose conditions held and whose owner is still the worktree it
/// was set on (or has outlived it by design), with the actions it still owes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetWatch {
    pub id: String,
    pub slug: String,
    pub by: String,
    /// The conditions that held, in words.
    pub held: String,
    /// What is still owed, in delivery order, each with its text.
    pub actions: Vec<(WatchAction, String)>,
    /// The gate this watch already cleared.
    pub cleared_gate: Option<String>,
}

/// One action of a met watch, as its fire carries it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WatchFire {
    pub id: String,
    #[serde(with = "watch_action")]
    pub action: WatchAction,
    /// Who set the watch.
    pub by: String,
    /// The conditions that held, in words.
    pub held: String,
    /// The action's own text: the gate to clear, what to tell the person,
    /// or the note for the session.
    pub text: String,
    /// The gate this watch cleared before waking the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_gate: Option<String>,
}

/// An action a newer build added fails to read, so the deferred fire that
/// carries it is skipped instead of run as something else.
mod watch_action {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    use wt_core::watch::WatchAction;

    pub fn serialize<S: Serializer>(
        action: &WatchAction,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(action.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<WatchAction, D::Error> {
        let name = String::deserialize(deserializer)?;
        WatchAction::ALL
            .into_iter()
            .find(|action| action.as_str() == name)
            .ok_or_else(|| D::Error::custom(format!("unknown watch action {name:?}")))
    }
}

#[derive(Clone, Debug, Default)]
pub struct AutomationEvalContext {
    pub github_fresh: bool,
    pub now_ms: i64,
    /// Local date `YYYY-MM-DD`, calculated by the app in its configured zone.
    pub local_day: String,
    pub base_branch: String,
    /// `[branch] production` names a branch other than base.
    pub separate_production: bool,
    pub reviewers_enabled: bool,
    pub actions: BTreeMap<String, ActionTraits>,
    pub pauses: PauseSnapshot,
    /// Watched tips by name: a `branch.advanced` rule's branch, or a
    /// `stage.deployed` rule's deployed ref (`VerifyStage::deployed_ref`).
    pub branch_tips: BTreeMap<String, BranchTip>,
    /// Frozen template values for post-merge external fires.
    pub row_vars: BTreeMap<String, BTreeMap<String, String>>,
    /// Pending events of `mcp.event` rules, oldest first.
    pub mcp_events: Vec<McpEvent>,
    /// Live worktrees' recorded guides (`wt guide <url>`).
    pub guides: Vec<GuideOwner>,
    /// Met watches with actions still owed.
    pub watches: Vec<MetWatch>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BranchRange {
    pub branch: String,
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FrozenPr {
    pub state: String,
    pub is_draft: bool,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// The event an `mcp.event` fire delivers. A fire with no owning
    /// worktree has `slug == FLEET_SLUG` and goes to the manager.
    pub mcp_event: Option<McpEvent>,
    /// The watch action a `watch.met` fire delivers. Absent from fires
    /// persisted before watches existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<WatchFire>,
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
        AutomationTrigger::StatusVerified => Some(WorkState::Verified),
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
    if let Some((stage, steps)) = work.verification() {
        suffix.push_str(&format!(" [verify in {}: {steps}]", stage.as_str()));
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
