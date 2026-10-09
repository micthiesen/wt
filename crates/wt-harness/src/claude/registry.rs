use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegistryStatus {
    Busy,
    Shell,
    Idle,
    Waiting,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySession {
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub name: Option<String>,
    pub status: RegistryStatus,
    pub waiting_for: Option<String>,
    pub kind: String,
    pub entrypoint: String,
    pub started_at: i64,
    pub updated_at: i64,
    /// Claude's registry format is undocumented and can grow. Keep every
    /// field not modeled above so consumers can inspect future data.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

pub fn read_registry(sessions_dir: &Path) -> Vec<RegistrySession> {
    let Ok(entries) = fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        if entry.path().extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(pid) = value
            .get("pid")
            .and_then(Value::as_u64)
            .filter(|p| *p > 0 && *p <= u32::MAX as u64)
            .map(|p| p as u32)
        else {
            continue;
        };
        let Some(session_id) = value.get("sessionId").and_then(Value::as_str) else {
            continue;
        };
        let Some(cwd) = value.get("cwd").and_then(Value::as_str) else {
            continue;
        };
        let Some(raw_status) = value.get("status").and_then(Value::as_str) else {
            continue;
        };
        let status = match raw_status {
            "busy" => RegistryStatus::Busy,
            "shell" => RegistryStatus::Shell,
            "idle" => RegistryStatus::Idle,
            "waiting" => RegistryStatus::Waiting,
            _ => RegistryStatus::Unknown,
        };
        let known = [
            "pid",
            "sessionId",
            "cwd",
            "name",
            "status",
            "waitingFor",
            "kind",
            "entrypoint",
            "startedAt",
            "updatedAt",
        ];
        let extra = value
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, _)| !known.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let session = RegistrySession {
            pid,
            session_id: session_id.to_owned(),
            cwd: cwd.to_owned(),
            name: value.get("name").and_then(Value::as_str).map(str::to_owned),
            status,
            waiting_for: value
                .get("waitingFor")
                .and_then(Value::as_str)
                .map(str::to_owned),
            kind: value
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            entrypoint: value
                .get("entrypoint")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            started_at: value.get("startedAt").and_then(Value::as_i64).unwrap_or(0),
            updated_at: value.get("updatedAt").and_then(Value::as_i64).unwrap_or(0),
            extra,
        };
        if session.pid == 0 || !pid_alive(session.pid) {
            continue;
        }
        sessions.push(session);
    }
    sessions
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill with signal zero only probes a positive pid and does not
        // access user memory. EPERM means the process exists but is protected.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

pub fn registry_by_session_id(
    entries: &[RegistrySession],
    preferred_name: &str,
) -> BTreeMap<String, RegistrySession> {
    let mut grouped: BTreeMap<String, Vec<&RegistrySession>> = BTreeMap::new();
    for entry in entries {
        grouped
            .entry(entry.session_id.clone())
            .or_default()
            .push(entry);
    }
    grouped
        .into_iter()
        .filter_map(|(id, candidates)| {
            candidates
                .iter()
                .copied()
                .find(|r| r.name.as_deref() == Some(preferred_name))
                .or_else(|| {
                    candidates
                        .iter()
                        .copied()
                        .find(|r| r.name.as_deref() == Some("primary"))
                })
                .or_else(|| candidates.first().copied())
                .cloned()
                .map(|r| (id, r))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn unknown_registry_status_and_future_fields_are_retained() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path()).unwrap();
        let path = tmp.path().join("current.json");
        fs::write(
            &path,
            serde_json::json!({
                "pid": std::process::id(), "sessionId":"future-id", "cwd":"/tmp/demo",
                "status":"future-state", "futureField":{"x":2}
            })
            .to_string(),
        )
        .unwrap();
        let entries = read_registry(tmp.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, RegistryStatus::Unknown);
        assert_eq!(entries[0].extra["futureField"]["x"], 2);
    }
}
