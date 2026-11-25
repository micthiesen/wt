//! The record that wt armed a pull request's merge: what later lets it tell
//! "GitHub dropped the merge" from "nobody asked for one".

use serde::{Deserialize, Serialize};

/// How the merge was armed. The two are separate GitHub operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMechanism {
    /// Submitted to the base branch's merge queue.
    Queue,
    /// Classic auto-merge.
    AutoMerge,
}

/// One arming of one pull request. Arming again is a new record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeArm {
    pub pr: u64,
    /// The head the arm was sent for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub armed_at_ms: i64,
    pub mechanism: MergeMechanism,
}
