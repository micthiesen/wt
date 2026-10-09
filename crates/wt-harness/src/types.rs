use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::status::DerivedState;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessExtras {
    pub managed_name: Option<String>,
    pub derived_state: Option<DerivedState>,
    #[serde(default)]
    pub queued: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_since: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_ended_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_summary: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSession {
    pub display_name: String,
    pub session_id: String,
    pub tmux_session_name: String,
    pub last_active_ms: Option<i64>,
    pub is_live: bool,
    #[serde(default)]
    pub extras: HarnessExtras,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessSpawnRequest {
    pub worktree_path: PathBuf,
    pub slug: String,
    pub managed_name: Option<String>,
    pub resume_session_id: Option<String>,
    pub display_label: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryRequest {
    pub slug: String,
    pub worktree_path: PathBuf,
    pub live_session_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SessionSummarySource {
    #[serde(rename = "ai-title")]
    AiTitle,
    #[serde(rename = "away_summary")]
    AwaySummary,
    #[serde(rename = "last-prompt")]
    LastPrompt,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub text: String,
    pub source: SessionSummarySource,
}
