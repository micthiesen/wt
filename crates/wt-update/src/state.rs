use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{Channel, VersionId, validate_component};

pub const STATE_FORMAT: u32 = 1;
const HISTORY_LIMIT: usize = 64;

/// Machine-local, forward-compatible updater state. Unknown fields survive
/// read/modify/write so newer launchers do not lose state written by a future
/// version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallState {
    pub format_version: u32,
    pub channel: Channel,
    pub current: Option<VersionId>,
    pub last_good: Option<VersionId>,
    pub pending_boot: Option<PendingBoot>,
    pub declined_build_id: Option<String>,
    pub last_check_unix: Option<u64>,
    pub history: Vec<StateHistoryEntry>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingBoot {
    pub candidate: VersionId,
    pub fallback: Option<VersionId>,
    pub attempt_token: String,
    pub started_unix: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateHistoryEntry {
    pub at_unix: u64,
    pub operation: String,
    pub from: Option<VersionId>,
    pub to: Option<VersionId>,
    pub detail: Option<String>,
}

impl InstallState {
    pub fn new(channel: Channel) -> Self {
        Self {
            format_version: STATE_FORMAT,
            channel,
            current: None,
            last_good: None,
            pending_boot: None,
            declined_build_id: None,
            last_check_unix: None,
            history: Vec::new(),
            extra: BTreeMap::new(),
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, StateError> {
        let raw: Value = serde_json::from_slice(bytes)
            .map_err(|error| StateError::Invalid(error.to_string()))?;
        let format_version = raw
            .get("formatVersion")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| StateError::Invalid("missing or invalid formatVersion".into()))?;
        if format_version != STATE_FORMAT {
            return Err(StateError::UnsupportedVersion(format_version));
        }
        let state: Self = serde_json::from_slice(bytes)
            .map_err(|error| StateError::Invalid(error.to_string()))?;
        if state.history.len() > HISTORY_LIMIT {
            return Err(StateError::Invalid(format!(
                "history has {} entries; maximum is {HISTORY_LIMIT}",
                state.history.len()
            )));
        }
        let pending_versions = state
            .pending_boot
            .iter()
            .flat_map(|pending| std::iter::once(&pending.candidate).chain(pending.fallback.iter()));
        for version in state
            .current
            .iter()
            .chain(state.last_good.iter())
            .chain(pending_versions)
        {
            if !version.is_valid() {
                return Err(StateError::Invalid("unsafe version identity".into()));
            }
        }
        if state
            .declined_build_id
            .as_deref()
            .is_some_and(|value| !validate_component(value))
        {
            return Err(StateError::Invalid("unsafe declined build id".into()));
        }
        if let Some(pending) = &state.pending_boot
            && !validate_component(&pending.attempt_token)
        {
            return Err(StateError::Invalid("unsafe boot attempt token".into()));
        }
        Ok(state)
    }

    pub fn to_json(&self) -> Result<Vec<u8>, StateError> {
        serde_json::to_vec_pretty(self).map_err(|error| StateError::Invalid(error.to_string()))
    }

    pub fn push_history(&mut self, entry: StateHistoryEntry) {
        self.history.push(entry);
        if self.history.len() > HISTORY_LIMIT {
            let discard = self.history.len() - HISTORY_LIMIT;
            self.history.drain(..discard);
        }
    }
}

/// Decision for a manifest already selected from the configured release channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateOffer {
    AlreadyInstalled,
    Declined,
    Available(VersionId),
}

pub fn update_offer(state: &InstallState, latest: VersionId) -> UpdateOffer {
    if state.current.as_ref() == Some(&latest) {
        UpdateOffer::AlreadyInstalled
    } else if state.declined_build_id.as_deref() == Some(latest.build_id()) {
        UpdateOffer::Declined
    } else {
        UpdateOffer::Available(latest)
    }
}

/// Startup checks run at most once per day. A future timestamp is treated as
/// due so clock rollback cannot suppress updates until wall time catches up.
pub fn update_check_due(last_check_unix: Option<u64>, now_unix: u64) -> bool {
    const DAY: u64 = 24 * 60 * 60;
    last_check_unix.is_none_or(|last| now_unix < last || now_unix.saturating_sub(last) >= DAY)
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("update state is invalid: {0}")]
    Invalid(String),
    #[error("update state format {0} is newer than this launcher supports")]
    UnsupportedVersion(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip_keeps_unknown_fields() {
        let bytes = br#"{"formatVersion":1,"channel":"preview","current":null,"lastGood":null,"pendingBoot":null,"declinedBuildId":"preview-a1b2","lastCheckUnix":7,"history":[],"futurePolicy":{"x":1}}"#;
        let mut state = InstallState::parse(bytes).unwrap();
        state.last_check_unix = Some(8);
        let reparsed = InstallState::parse(&state.to_json().unwrap()).unwrap();
        assert_eq!(
            reparsed.extra.get("futurePolicy"),
            Some(&serde_json::json!({"x":1}))
        );
        assert_eq!(reparsed.last_check_unix, Some(8));
    }

    #[test]
    fn newer_or_malformed_state_fails_closed() {
        assert!(matches!(
            InstallState::parse(br#"{"formatVersion":2}"#),
            Err(StateError::UnsupportedVersion(2))
        ));
        assert!(InstallState::parse(br#"{"formatVersion":1,"declinedBuildId":"../bad"}"#).is_err());
    }

    #[test]
    fn declined_build_is_suppressed_until_the_build_changes() {
        let mut state = InstallState::new(Channel::Stable);
        state.declined_build_id = Some("deadbeef".into());
        let declined = VersionId::new("v1", "deadbeef", "x86_64-unknown-linux-gnu").unwrap();
        let next = VersionId::new("v1", "cafebabe", "x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(update_offer(&state, declined), UpdateOffer::Declined);
        assert_eq!(
            update_offer(&state, next.clone()),
            UpdateOffer::Available(next)
        );
        assert!(update_check_due(Some(500), 499));
        assert!(!update_check_due(Some(100), 100 + 86_399));
        assert!(update_check_due(Some(100), 100 + 86_400));
    }
}
