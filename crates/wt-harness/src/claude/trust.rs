use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Map, Value};
use thiserror::Error;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum TrustError {
    #[error("Claude trust file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Claude trust file {path}: invalid JSON: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

fn lock(path: &Path) -> Result<fs::File, TrustError> {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".wt-lock");
    let lock_path = PathBuf::from(lock_path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|source| TrustError::Io {
            path: parent.into(),
            source,
        })?;
    }
    let f = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| TrustError::Io {
            path: lock_path.clone(),
            source,
        })?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor refers to the open lock file and flock stores no pointer.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(TrustError::Io {
                path: lock_path,
                source: std::io::Error::last_os_error(),
            });
        }
    }
    Ok(f)
}

fn read_projects(path: &Path) -> Result<(Value, Map<String, Value>), TrustError> {
    let raw = fs::read(path).map_err(|source| TrustError::Io {
        path: path.to_owned(),
        source,
    })?;
    let data: Value = serde_json::from_slice(&raw).map_err(|source| TrustError::Json {
        path: path.to_owned(),
        source,
    })?;
    let projects = data
        .get("projects")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    Ok((data, projects))
}

fn write_atomic(path: &Path, value: &Value) -> Result<(), TrustError> {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("wt-{}-{seq}.tmp", std::process::id()));
    let mut f = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .map_err(|source| TrustError::Io {
            path: tmp.clone(),
            source,
        })?;
    serde_json::to_writer_pretty(&mut f, value).map_err(|source| TrustError::Json {
        path: tmp.clone(),
        source,
    })?;
    f.write_all(b"\n").map_err(|source| TrustError::Io {
        path: tmp.clone(),
        source,
    })?;
    f.sync_all().map_err(|source| TrustError::Io {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| TrustError::Io {
        path: path.to_owned(),
        source,
    })
}

/// Mark only requested paths trusted, preserving every unrelated Claude JSON
/// field. Callers choose the settings file explicitly; no real-user path is
/// consulted by this helper.
pub fn ensure_trusted_paths(path: &Path, wanted: &[PathBuf]) -> Result<Vec<PathBuf>, TrustError> {
    let _guard = lock(path)?;
    if !path.exists() {
        return Ok(wanted.to_vec());
    }
    let (mut data, mut projects) = read_projects(path)?;
    for project_path in wanted {
        let key = project_path.to_string_lossy().into_owned();
        let mut project = projects
            .get(&key)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        project.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
        projects.insert(key, Value::Object(project));
    }
    let Some(root) = data.as_object_mut() else {
        return Ok(wanted.to_vec());
    };
    root.insert("projects".into(), Value::Object(projects));
    write_atomic(path, &data)?;
    let (_, after) = read_projects(path)?;
    let untrusted = wanted
        .iter()
        .filter(|p| {
            after
                .get(p.to_string_lossy().as_ref())
                .and_then(|p| p.get("hasTrustDialogAccepted"))
                != Some(&Value::Bool(true))
        })
        .cloned()
        .collect();
    Ok(untrusted)
}

/// Retry read-modify-write-verify if Claude flushes a stale full-file snapshot
/// immediately after our atomic replacement. This mirrors the short bounded
/// convergence window in the Bun implementation without blocking a Tokio
/// executor thread during backoff.
pub async fn ensure_trusted_paths_retry(
    path: &Path,
    wanted: &[PathBuf],
) -> Result<Vec<PathBuf>, TrustError> {
    for (attempt, delay) in [0, 60, 200].into_iter().enumerate() {
        if delay != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        let remaining = ensure_trusted_paths(path, wanted)?;
        if remaining.is_empty() {
            return Ok(Vec::new());
        }
        if attempt == 2 {
            return Ok(remaining);
        }
    }
    unreachable!("fixed trust retry loop always returns")
}

pub fn sibling_paths_to_repair(
    current: &Path,
    known: impl IntoIterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    let base = current.parent();
    let known: BTreeSet<_> = known.into_iter().collect();
    std::iter::once(current.to_owned())
        .chain(
            known
                .into_iter()
                .filter(|p| p.parent() == base && p != current),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn trust_write_is_atomic_and_preserves_extensions() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("claude.json");
        fs::write(
            &path,
            r#"{"future":7,"projects":{"/tmp/wt":{"other":true}}}"#,
        )
        .unwrap();
        assert!(
            ensure_trusted_paths(&path, &[PathBuf::from("/tmp/wt")])
                .unwrap()
                .is_empty()
        );
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["future"], 7);
        assert_eq!(saved["projects"]["/tmp/wt"]["other"], true);
        assert_eq!(saved["projects"]["/tmp/wt"]["hasTrustDialogAccepted"], true);
    }
}
