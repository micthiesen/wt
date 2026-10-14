//! Credential-free account-switch intent. Generations survive offline workers;
//! only a completed worker login and daemon refresh acknowledge them.
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use wt_platform::lock::FileLock;

const STATE: &str = "wt-managed-auth.json";

#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    desired: u64,
    #[serde(default)]
    acknowledged: u64,
}

pub fn controller_home(home: &Path) -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"))
}

fn read(directory: &Path) -> Result<State> {
    match std::fs::read(directory.join(STATE)) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).context("read managed Codex account-switch state")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error).context("read managed Codex account-switch state"),
    }
}

fn write(directory: &Path, state: &State) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&serde_json::to_vec(state)?)?;
    file.as_file().sync_all()?;
    file.persist(directory.join(STATE))?;
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

pub fn controller_generation(codex_home: &Path) -> Result<Option<u64>> {
    Ok(nonzero(read(codex_home)?.desired))
}

fn nonzero(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}

pub async fn request_generation(codex_home: &Path, cancel: &CancellationToken) -> Result<u64> {
    let _lock = FileLock::acquire(
        codex_home,
        "wt-managed-auth",
        "request Codex account switch",
        cancel,
    )
    .await?;
    let mut state = read(codex_home)?;
    let now = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())?;
    state.desired = now.max(
        state
            .desired
            .checked_add(1)
            .context("account-switch generation exhausted")?,
    );
    write(codex_home, &state)?;
    Ok(state.desired)
}

pub fn worker_desired(root: &Path) -> Result<Option<u64>> {
    controller_generation(root)
}

pub fn worker_pending(root: &Path) -> Result<bool> {
    let state = read(root)?;
    Ok(state.desired > state.acknowledged)
}

pub async fn write_worker_desired(
    root: &Path,
    desired: Option<u64>,
    cancel: &CancellationToken,
) -> Result<()> {
    let Some(desired) = desired else {
        return Ok(());
    };
    let _lock = FileLock::acquire(
        root,
        "wt-managed-auth",
        "receive Codex account switch",
        cancel,
    )
    .await?;
    let mut state = read(root)?;
    if desired > state.desired {
        state.desired = desired;
        write(root, &state)?;
    }
    Ok(())
}

/// Call only after native login, login status and daemon restart succeeded.
/// A login begun for an older request cannot satisfy a newer account switch.
pub async fn acknowledge(
    root: &Path,
    expected: Option<u64>,
    cancel: &CancellationToken,
) -> Result<()> {
    let _lock = FileLock::acquire(
        root,
        "wt-managed-auth",
        "complete Codex account switch",
        cancel,
    )
    .await?;
    let mut state = read(root)?;
    if nonzero(state.desired) != expected {
        bail!("a newer Codex account switch arrived during sign-in; sign in again to complete it");
    }
    state.acknowledged = state.desired;
    write(root, &state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn offline_intent_survives_restart_and_only_matching_login_acknowledges() {
        let controller = tempfile::tempdir().unwrap();
        let worker = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let first = request_generation(controller.path(), &cancel)
            .await
            .unwrap();
        assert_eq!(
            controller_generation(controller.path()).unwrap(),
            Some(first)
        );
        write_worker_desired(worker.path(), Some(first), &cancel)
            .await
            .unwrap();
        assert!(worker_pending(worker.path()).unwrap());
        let next = request_generation(controller.path(), &cancel)
            .await
            .unwrap();
        assert!(next > first);
        write_worker_desired(worker.path(), Some(next), &cancel)
            .await
            .unwrap();
        write_worker_desired(worker.path(), Some(first), &cancel)
            .await
            .unwrap();
        assert!(
            acknowledge(worker.path(), Some(first), &cancel)
                .await
                .is_err()
        );
        assert!(worker_pending(worker.path()).unwrap());
        acknowledge(worker.path(), Some(next), &cancel)
            .await
            .unwrap();
        assert!(!worker_pending(worker.path()).unwrap());
    }

    #[tokio::test]
    async fn missing_generation_preserves_legacy_login_and_never_clears_pending() {
        let worker = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        assert!(!worker_pending(worker.path()).unwrap());
        acknowledge(worker.path(), None, &cancel).await.unwrap();
        write_worker_desired(worker.path(), Some(4), &cancel)
            .await
            .unwrap();
        write_worker_desired(worker.path(), None, &cancel)
            .await
            .unwrap();
        assert!(worker_pending(worker.path()).unwrap());
        assert_eq!(worker_desired(worker.path()).unwrap(), Some(4));
    }

    #[test]
    fn malformed_state_is_not_a_successful_login() {
        let worker = tempfile::tempdir().unwrap();
        std::fs::write(worker.path().join(STATE), b"{").unwrap();
        assert!(worker_pending(worker.path()).is_err());
    }
}
