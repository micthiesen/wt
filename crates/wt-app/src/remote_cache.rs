//! Disposable, config-scoped presentation snapshots. A cached board never
//! establishes connectivity or authorizes a command.
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wt_config::RemoteConfig;
use wt_runtime::{SourceHandle, SourceState, TaskScope};

use crate::{
    context::AppContext,
    host_protocol::{HOST_PROTOCOL, HostSnapshot, MAX_FRAME_BYTES},
};

const WRITE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
struct Cached {
    protocol: u32,
    endpoint: String,
    snapshot: HostSnapshot,
}

fn path(context: &AppContext, endpoint: &RemoteConfig) -> PathBuf {
    let digest = Sha256::digest(endpoint.key().as_bytes());
    context
        .config
        .paths
        .cache_root
        .join("native-hosts")
        .join(format!("{digest:x}.json"))
}

pub async fn load(context: &AppContext, endpoint: &RemoteConfig) -> Option<HostSnapshot> {
    let path = path(context, endpoint);
    let key = endpoint.key();
    tokio::task::spawn_blocking(move || read(&path, &key))
        .await
        .ok()
        .and_then(Result::ok)
}

fn read(path: &Path, key: &str) -> Result<HostSnapshot> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FRAME_BYTES {
        bail!("cached host snapshot exceeds size limit");
    }
    let cached: Cached = serde_json::from_slice(&bytes)?;
    if cached.protocol != HOST_PROTOCOL || cached.endpoint != key {
        bail!("cached host snapshot identity changed");
    }
    cached.snapshot.validate()?;
    Ok(cached.snapshot)
}

fn write(path: &Path, key: String, snapshot: Arc<HostSnapshot>) -> Result<()> {
    let cached = Cached {
        protocol: HOST_PROTOCOL,
        endpoint: key,
        snapshot: (*snapshot).clone(),
    };
    let bytes = serde_json::to_vec(&cached)?;
    if bytes.len() > MAX_FRAME_BYTES {
        bail!("cached host snapshot exceeds size limit");
    }
    let parent = path.parent().context("host cache has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&bytes)?;
    file.persist(path).context("replace host snapshot cache")?;
    Ok(())
}

pub fn start_writer(
    scope: &TaskScope,
    context: &AppContext,
    endpoint: &RemoteConfig,
    source: SourceHandle<HostSnapshot>,
) {
    let path = path(context, endpoint);
    let key = endpoint.key();
    let cancel = scope.token();
    scope.spawn(async move {
        let mut updates = source.subscribe();
        let mut pending = None;
        let mut last_write = tokio::time::Instant::now() - WRITE_INTERVAL;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                changed = updates.changed() => {
                    if changed.is_err() { break; }
                    let snapshot = updates.borrow_and_update().clone();
                    if snapshot.state == SourceState::Ready
                        && let Some(data) = snapshot.data
                        && data.board.is_some()
                    { pending = Some(data); }
                },
                _ = tokio::time::sleep_until(last_write + WRITE_INTERVAL), if pending.is_some() => {
                    let Some(snapshot) = pending.take() else { continue; };
                    let path = path.clone();
                    let key = key.clone();
                    // Always join the bounded blocking write before scope shutdown.
                    match tokio::task::spawn_blocking(move || write(&path, key, snapshot)).await {
                        Ok(Ok(())) => {},
                        Ok(Err(error)) => tracing::warn!(%error, "could not save disposable host cache"),
                        Err(error) => tracing::warn!(%error, "host cache writer panicked"),
                    }
                    last_write = tokio::time::Instant::now();
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_protocol::HostState;
    #[test]
    fn cache_identity_and_structure_are_checked_before_display() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("host.json");
        let snapshot = HostSnapshot {
            board: Some(wt_tui::Board::default()),
            state: HostState::Ready,
            layout: Default::default(),
        };
        write(
            &path,
            "host [config: one]".into(),
            Arc::new(snapshot.clone()),
        )
        .unwrap();
        assert_eq!(read(&path, "host [config: one]").unwrap(), snapshot);
        assert!(read(&path, "host [config: two]").is_err());
        let mut invalid = snapshot;
        invalid.layout.insert(
            "absent".into(),
            crate::host_protocol::RowLayout {
                base_branch: None,
                work: None,
            },
        );
        write(&path, "host".into(), Arc::new(invalid)).unwrap();
        assert!(read(&path, "host").is_err());
        std::fs::write(&path, b"{truncated").unwrap();
        assert!(read(&path, "host").is_err());
    }
}
