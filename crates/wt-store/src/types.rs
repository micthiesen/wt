use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    #[serde(flatten)]
    pub extra: Map<String, Value>,
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
    }
}

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

/// A repository snapshot found in an arbitrary database during state repair.
#[derive(Clone, Debug, PartialEq)]
pub struct ForeignRepositoryRow {
    pub repo_id: String,
    pub repo_path: String,
    pub data: String,
    pub updated_at: i64,
    pub archived: BTreeSet<String>,
}
