use std::{
    cmp::Ordering,
    collections::HashMap,
    fs,
    io::Write,
    path::Path,
    sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
};

use serde_json::{Map, Value};
use thiserror::Error;

use super::{identity::claude_session_id, transcript::SessionTail};
use crate::{DerivedState, SessionSummary};

const NAME_FILE: &str = "claude-sessions.json";
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeSessionPickerEntry {
    pub name: Option<String>,
    pub session_id: String,
    pub is_live: bool,
    pub state: DerivedState,
    pub last_entry_ms: Option<i64>,
    pub queued: u32,
    pub summary: Option<SessionSummary>,
    pub session_summary: Option<String>,
}

#[derive(Debug, Error)]
pub enum NamesError {
    #[error("Claude session names: {0}")]
    Io(#[from] std::io::Error),
    #[error("Claude session names JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Returns a user-facing validation error, matching Claude's picker contract.
pub fn validate_session_name(input: &str) -> Option<&'static str> {
    let name = input.trim();
    if name.is_empty() {
        return Some("name can't be empty");
    }
    if name.len() > 32 {
        return Some("name too long (max 32)");
    }
    if name.eq_ignore_ascii_case("primary") {
        return Some("`primary` is reserved");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Some("use letters, digits, _ or - only");
    }
    None
}

fn lock_file(path: &Path) -> Result<fs::File, NamesError> {
    fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: flock operates on the valid open file descriptor and does not
        // retain the pointer. The guard unlocks when the file is dropped.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(lock)
}

fn read_shape(path: &Path) -> Result<Value, NamesError> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let raw = fs::read(path)?;
    let value: Value = serde_json::from_slice(&raw)?;
    Ok(if value.is_object() {
        value
    } else {
        Value::Object(Map::new())
    })
}

fn atomic_write(path: &Path, value: &Value) -> Result<(), NamesError> {
    fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
    let seq = TEMP_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), seq));
    let mut f = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)?;
    serde_json::to_writer_pretty(&mut f, value)?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn slug_names(root: &Value, slug: &str) -> Vec<String> {
    root.get(slug)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| validate_session_name(name).is_none())
        .map(str::to_owned)
        .fold(Vec::new(), |mut names, name| {
            if !names.contains(&name) {
                names.push(name);
            }
            names
        })
}

pub fn list_claude_names(cache_dir: &Path, slug: &str) -> Result<Vec<String>, NamesError> {
    let path = cache_dir.join(NAME_FILE);
    Ok(slug_names(&read_shape(&path)?, slug))
}

pub fn add_claude_name(cache_dir: &Path, slug: &str, name: &str) -> Result<(), NamesError> {
    if validate_session_name(name).is_some() {
        return Ok(());
    }
    let path = cache_dir.join(NAME_FILE);
    let _lock = lock_file(&path)?;
    let mut value = read_shape(&path)?;
    let names = slug_names(&value, slug);
    if names.iter().any(|n| n == name) {
        return Ok(());
    }
    let mut next = names;
    next.push(name.to_owned());
    value[slug] = serde_json::to_value(next)?;
    atomic_write(&path, &value)
}

pub fn remove_claude_name(cache_dir: &Path, slug: &str, name: &str) -> Result<(), NamesError> {
    let path = cache_dir.join(NAME_FILE);
    let _lock = lock_file(&path)?;
    let mut value = read_shape(&path)?;
    let next: Vec<_> = slug_names(&value, slug)
        .into_iter()
        .filter(|n| n != name)
        .collect();
    if next.is_empty() {
        value.as_object_mut().expect("object").remove(slug);
    } else {
        value[slug] = serde_json::to_value(next)?;
    }
    atomic_write(&path, &value)
}

/// Drop persisted named-session slots for worktrees no longer in the
/// caller's authoritative live inventory, preserving unknown root fields.
pub fn reap_claude_names(
    cache_dir: &Path,
    live_slugs: &std::collections::HashSet<String>,
) -> Result<(), NamesError> {
    let path = cache_dir.join(NAME_FILE);
    let _lock = lock_file(&path)?;
    let mut value = read_shape(&path)?;
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    let before = object.len();
    object.retain(|slug, _| live_slugs.contains(slug));
    if object.len() != before {
        atomic_write(&path, &value)?;
    }
    Ok(())
}

pub fn next_auto_name(cache_dir: &Path, slug: &str) -> Result<String, NamesError> {
    let taken: std::collections::HashSet<_> =
        list_claude_names(cache_dir, slug)?.into_iter().collect();
    for n in 2u64.. {
        if !taken.contains(&n.to_string()) {
            return Ok(n.to_string());
        }
    }
    unreachable!("integer iterator is unbounded")
}

pub fn build_claude_session_entries(
    slug: &str,
    worktree_path: &Path,
    persisted_names: &[String],
    live_names: &[Option<String>],
    tails: &HashMap<Option<String>, SessionTail>,
    registry_status: &HashMap<String, super::RegistryStatus>,
    summaries: &HashMap<String, Option<SessionSummary>>,
) -> Vec<ClaudeSessionPickerEntry> {
    let mut names = Vec::new();
    for name in persisted_names.iter().chain(live_names.iter().flatten()) {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    let all: Vec<Option<String>> = std::iter::once(None)
        .chain(names.into_iter().map(Some))
        .collect();
    let live: std::collections::HashSet<_> = live_names.iter().cloned().collect();
    let mut out: Vec<_> = all
        .into_iter()
        .map(|name| {
            let id = claude_session_id(worktree_path, name.as_deref());
            let tail = tails
                .get(&name)
                .cloned()
                .unwrap_or_else(|| SessionTail::empty(name.clone()));
            let status = registry_status.get(&id).copied();
            let is_live = live.contains(&name);
            ClaudeSessionPickerEntry {
                name,
                session_id: id.clone(),
                is_live,
                state: crate::derive_session_state(&tail, is_live, status),
                last_entry_ms: tail.last_entry_ms,
                queued: tail.queued,
                summary: summaries.get(&id).cloned().flatten(),
                session_summary: tail.session_summary,
            }
        })
        .collect();
    out.retain(|e| {
        e.is_live || e.state != DerivedState::Idle || e.last_entry_ms.is_some() || e.name.is_none()
    });
    out.sort_by(|a, b| {
        state_rank(a.state)
            .cmp(&state_rank(b.state))
            .then_with(|| match (a.name.is_none(), b.name.is_none()) {
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => Ordering::Equal,
            })
            .then_with(|| {
                b.last_entry_ms
                    .unwrap_or(0)
                    .cmp(&a.last_entry_ms.unwrap_or(0))
            })
            .then_with(|| {
                a.name
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.name.as_deref().unwrap_or(""))
            })
    });
    let _ = slug; // retained in API for parity and future per-slug validation.
    out
}

fn state_rank(state: DerivedState) -> u8 {
    match state {
        DerivedState::Asking => 0,
        DerivedState::Working => 1,
        DerivedState::Polling => 2,
        DerivedState::Unknown => 3,
        DerivedState::Waiting => 4,
        DerivedState::Abandoned => 5,
        DerivedState::Idle => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn names_mutate_atomically_without_discarding_future_fields() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join(NAME_FILE);
        fs::write(&path, r#"{"demo":["review"],"future":{"x":1}}"#).unwrap();
        add_claude_name(tmp.path(), "demo", "docs").unwrap();
        remove_claude_name(tmp.path(), "demo", "review").unwrap();
        let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["demo"], serde_json::json!(["docs"]));
        assert_eq!(saved["future"]["x"], 1);
    }

    #[test]
    fn session_names_are_validated_with_primary_reserved() {
        assert_eq!(validate_session_name(" review "), None);
        assert_eq!(
            validate_session_name("primary"),
            Some("`primary` is reserved")
        );
        assert!(validate_session_name("has space").is_some());
    }
}
