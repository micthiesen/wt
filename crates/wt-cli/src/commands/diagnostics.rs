use crate::context::AppContext;
use anyhow::Result;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{fs::OpenOptions, io, path::Path};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CheckStatus {
    Ok,
    Info,
    Warn,
    Err,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OperationLock {
    pub op: Option<String>,
    pub phase: Option<String>,
    pub pid: Option<u32>,
    pub host: Option<String>,
    pub started_at: Option<String>,
    pub phase_started: Option<String>,
}

/// Read operation-lock metadata without creating lock files or acquiring a
/// lock directory. A stale file is not reported as an active operation.
pub(crate) async fn operation_lock(ctx: &AppContext, slug: &str) -> Result<Option<OperationLock>> {
    let path = ctx.config.paths.lock_dir.join(format!("{slug}.lock"));
    Ok(tokio::task::spawn_blocking(move || inspect_operation_lock(&path)).await??)
}

fn inspect_operation_lock(path: &Path) -> io::Result<Option<OperationLock>> {
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    {
        // SAFETY: flock only operates on the open descriptor and does not
        // mutate file contents. A successful probe proves the file is stale.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            // SAFETY: this descriptor owns the lock just acquired above.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            return Ok(None);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error);
        }
    }
    #[cfg(not(unix))]
    return Ok(Some(OperationLock {
        op: None,
        phase: None,
        pid: None,
        host: None,
        started_at: None,
        phase_started: None,
    }));

    use std::io::{Read, Seek, SeekFrom};
    let mut contents = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut contents)?;
    Ok(Some(serde_json::from_str(&contents).unwrap_or(
        OperationLock {
            op: None,
            phase: None,
            pid: None,
            host: None,
            started_at: None,
            phase_started: None,
        },
    )))
}

pub(crate) fn worst(checks: &[Check]) -> CheckStatus {
    checks
        .iter()
        .map(|check| check.status)
        .max_by_key(|status| match status {
            CheckStatus::Ok | CheckStatus::Info => 0,
            CheckStatus::Warn => 1,
            CheckStatus::Err => 2,
        })
        .unwrap_or(CheckStatus::Ok)
}

pub(crate) fn display_status(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Ok => "ok",
        CheckStatus::Info => "info",
        CheckStatus::Warn => "warn",
        CheckStatus::Err => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lock_probe_distinguishes_live_and_stale_files_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let lock = wt_platform::lock::FileLock::try_acquire(directory.path(), "worktree", "test")
            .await
            .unwrap()
            .unwrap();
        let path = directory.path().join("worktree.lock");
        let before = std::fs::read(&path).unwrap();
        assert!(inspect_operation_lock(&path).unwrap().is_some());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(lock);
        assert!(inspect_operation_lock(&path).unwrap().is_none());
    }
}
