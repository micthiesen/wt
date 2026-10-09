use serde::{Deserialize, Serialize};

use crate::{LastEntryKind, RegistryStatus, SessionTail};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DerivedState {
    Working,
    Asking,
    Polling,
    Unknown,
    Waiting,
    Abandoned,
    Idle,
}

pub fn registry_status_to_state(status: RegistryStatus) -> DerivedState {
    match status {
        RegistryStatus::Busy => DerivedState::Working,
        RegistryStatus::Shell => DerivedState::Polling,
        RegistryStatus::Waiting => DerivedState::Asking,
        RegistryStatus::Unknown => DerivedState::Unknown,
        RegistryStatus::Idle => DerivedState::Waiting,
    }
}

pub fn derive_session_state(
    tail: &SessionTail,
    tmux_live: bool,
    registry_status: Option<RegistryStatus>,
) -> DerivedState {
    if let Some(status) = registry_status {
        return registry_status_to_state(status);
    }
    let mid_turn = matches!(
        tail.last_entry_kind,
        Some(LastEntryKind::ToolUse | LastEntryKind::ToolResult | LastEntryKind::Paused)
    );
    match (tmux_live, mid_turn) {
        (true, true) => DerivedState::Working,
        (true, false) => DerivedState::Waiting,
        (false, true) => DerivedState::Abandoned,
        (false, false) => DerivedState::Idle,
    }
}
