use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoSlug {
    pub owner: String,
    pub name: String,
}

impl RepoSlug {
    pub fn parse(value: &str) -> Option<Self> {
        let (owner, name) = value.split_once('/')?;
        let valid = |s: &str| {
            !s.is_empty()
                && s.len() <= 100
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        };
        (valid(owner) && valid(name) && !name.contains('/')).then(|| Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    pub fn as_str(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrChecks {
    Pass,
    Fail,
    Pending,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrReview {
    Approved,
    ChangesRequested,
    Pending,
    Unrequested,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewBotStatus {
    pub state: String,
    pub unresolved: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestedReviewer {
    pub login: String,
    pub is_author: bool,
    pub is_commenter: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrComment {
    pub author: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AutoMergeMethod {
    Squash,
    Merge,
    Rebase,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoMerge {
    pub enabled_at: String,
    pub merge_method: AutoMergeMethod,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeQueueState {
    AwaitingChecks,
    Locked,
    Mergeable,
    Queued,
    Unmergeable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeQueueEntry {
    pub head_ref_name: String,
    pub position: u32,
    pub state: MergeQueueState,
    pub enqueued_at: String,
    pub estimated_time_to_merge: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub number: u64,
    pub url: String,
    pub head_ref_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_ref_oid: Option<String>,
    pub base_ref_name: String,
    pub merge_commit_oid: Option<String>,
    pub title: String,
    pub is_draft: bool,
    pub state: String,
    pub mergeable: Option<String>,
    pub merge_state_status: Option<String>,
    pub checks: PrChecks,
    pub failed_checks: Vec<String>,
    pub review: PrReview,
    pub review_requests: u32,
    pub requested_reviewers: Vec<String>,
    pub suggested_reviewers: Vec<SuggestedReviewer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_bot: Option<ReviewBotStatus>,
    pub auto_merge: Option<AutoMerge>,
    pub comments: Vec<PrComment>,
    pub unresolved_threads: u32,
    pub unresolved_threads_total: u32,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubData {
    pub prs: BTreeMap<String, PullRequest>,
    pub merge_queue: BTreeMap<String, MergeQueueEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRequestPr {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub repo_name_with_owner: String,
    pub head_ref_name: Option<String>,
    /// Immutable commit identity for safe checkout of a review request.
    #[serde(default)]
    pub head_ref_oid: Option<String>,
    pub author: Option<String>,
    pub is_draft: bool,
    pub checks: PrChecks,
    pub review_decision: Option<String>,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
    pub comment_count: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LivePrInfo {
    pub number: u64,
    pub base_ref_name: String,
    pub state: String,
    pub is_draft: bool,
    pub title: String,
    pub id: String,
    pub head_ref_oid: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhActionResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

impl GhActionResult {
    pub const fn success() -> Self {
        Self {
            ok: true,
            error: None,
            retryable: None,
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            retryable: None,
        }
    }
    pub fn failed(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            retryable: Some(retryable),
        }
    }
    pub fn is_ok(&self) -> bool {
        self.ok
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contributor {
    pub login: String,
    pub contributions: u64,
}

#[derive(Debug, Error)]
pub enum GithubError {
    #[error("gh CLI is unavailable")]
    MissingCli,
    #[error("cannot resolve GitHub repository: {0}")]
    Repository(String),
    #[error("GitHub command failed: {0}")]
    Command(String),
    #[error("GitHub returned invalid data: {0}")]
    Protocol(String),
    #[error("GitHub rate limit: {0}")]
    RateLimit(String),
    #[error("transient GitHub failure: {message}")]
    Transient { message: String },
}

impl GithubError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Transient { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrMergeTarget {
    pub id: String,
    pub number: u64,
    pub base_ref_name: String,
    pub head_ref_oid: String,
}
