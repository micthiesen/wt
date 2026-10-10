use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use wt_config::EffectTag;
use wt_core::WorktreeRef;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionRunKind {
    Claude,
    Shell,
    Harness,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionRunStatus {
    Running,
    Succeeded,
    Failed,
    Killed,
    Ambiguous,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueStatusExpectation {
    pub issue_id: String,
    pub status: String,
}

/// Optional refinement data for an argument-prompt value. Kept in durable
/// action metadata so a worker can update picker history after the TUI exits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionArgHistory {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_extract: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionMeta {
    pub version: u32,
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_ref: Option<WorktreeRef>,
    pub run_id: String,
    /// Worktree identity used by the caller. Older TypeScript records omit it;
    /// those remain addressable by their slug under the per-slug lock.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub action_key: String,
    pub kind: ActionRunKind,
    pub action_id: String,
    pub action_name: String,
    pub prompt: String,
    pub affects: Vec<EffectTag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_status: Option<IssueStatusExpectation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg_history: Option<ActionArgHistory>,
    /// `None` means a legacy/pending run. `Some(true)` means refinement was
    /// attempted successfully or no distinct label was available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_refined: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_fire_keys: Vec<String>,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub status: ActionRunStatus,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRun {
    pub meta: ActionMeta,
    pub run_dir: PathBuf,
    pub command: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRunView {
    pub meta: ActionMeta,
    pub run_dir: PathBuf,
    pub lines: Vec<ActionOutputLine>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionOutputLine {
    pub stream: ActionStream,
    pub line: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionStream {
    Stdout,
    Stderr,
}
