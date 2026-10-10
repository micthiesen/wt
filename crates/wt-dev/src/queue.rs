use std::{
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use wt_platform::lock::{FileLock, LockError};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevWaiter {
    pub slug: String,
    pub pid: u32,
    pub since: u64,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueReport {
    pub waiters: Vec<DevWaiter>,
}

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("invalid waiter slug {0:?}")]
    InvalidSlug(String),
    #[error("dev queue I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("dev queue data at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("dev queue wait cancelled")]
    Cancelled,
    #[error("timed out cleaning up dev queue waiter")]
    CleanupTimeout,
}

pub(crate) fn queue_dir(dev_dir: &Path) -> PathBuf {
    dev_dir.join("waiting")
}

pub(crate) async fn join(
    dev_dir: &Path,
    lock_dir: &Path,
    slug: &str,
    cancellation: &CancellationToken,
) -> Result<DevWaiter, QueueError> {
    validate_slug(slug)?;
    let _lock = FileLock::acquire(lock_dir, "dev-queue", "join dev queue", cancellation).await?;
    // Repeated admission by the same live process is a retry, not a new place
    // in line. Preserve both its FIFO timestamp and any manager promotion.
    let path = queue_dir(dev_dir).join(format!("{slug}.json"));
    let waiter = match tokio::fs::read(&path).await {
        Ok(bytes) => match serde_json::from_slice::<DevWaiter>(&bytes) {
            Ok(waiter) if waiter.slug == slug && pid_alive(waiter.pid) => waiter,
            _ => DevWaiter {
                slug: slug.to_owned(),
                pid: std::process::id(),
                since: now_ms(),
                priority: 0,
            },
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => DevWaiter {
            slug: slug.to_owned(),
            pid: std::process::id(),
            since: now_ms(),
            priority: 0,
        },
        Err(source) => return Err(QueueError::Io { path, source }),
    };
    write_waiter(dev_dir, &waiter).await?;
    Ok(waiter)
}

pub(crate) async fn leave(dev_dir: &Path, lock_dir: &Path, slug: &str) -> Result<(), QueueError> {
    validate_slug(slug)?;
    // Cleanup is required after a successful queue admission, even though the
    // operation token may already be cancelled. Bound the shielded cleanup so
    // a wedged lock owner cannot hold shutdown indefinitely.
    let cleanup = CancellationToken::new();
    let _lock = tokio::time::timeout(
        Duration::from_secs(2),
        FileLock::acquire(lock_dir, "dev-queue", "leave dev queue", &cleanup),
    )
    .await
    .map_err(|_| QueueError::CleanupTimeout)??;
    let path = queue_dir(dev_dir).join(format!("{slug}.json"));
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(QueueError::Io { path, source }),
    }
}

pub(crate) async fn list(
    dev_dir: &Path,
    lock_dir: &Path,
    cancellation: &CancellationToken,
) -> Result<QueueReport, QueueError> {
    let _lock = FileLock::acquire(lock_dir, "dev-queue", "read dev queue", cancellation).await?;
    let dir = queue_dir(dev_dir);
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(QueueReport::default());
        }
        Err(source) => return Err(QueueError::Io { path: dir, source }),
    };
    let mut waiters = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|source| QueueError::Io {
            path: dir.clone(),
            source,
        })?
    {
        if cancellation.is_cancelled() {
            return Err(QueueError::Cancelled);
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if validate_slug(slug).is_err() {
            remove_malformed(&path).await;
            continue;
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => return Err(QueueError::Io { path, source }),
        };
        let mut waiter: DevWaiter = match serde_json::from_slice(&bytes) {
            Ok(waiter) => waiter,
            Err(source) => {
                remove_malformed(&path).await;
                let _ = source;
                continue;
            }
        };
        if waiter.slug != slug || waiter.pid == 0 || !pid_alive(waiter.pid) {
            remove_malformed(&path).await;
            continue;
        }
        // Old readers/writers may have omitted priority; deserialize defaults to FIFO.
        waiter.priority = waiter.priority.clamp(0, 1);
        waiters.push(waiter);
    }
    waiters.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.since.cmp(&b.since))
            .then_with(|| a.slug.cmp(&b.slug))
    });
    Ok(QueueReport { waiters })
}

pub(crate) async fn set_priority(
    dev_dir: &Path,
    lock_dir: &Path,
    slug: &str,
    priority: i32,
    cancellation: &CancellationToken,
) -> Result<Option<DevWaiter>, QueueError> {
    validate_slug(slug)?;
    if !(0..=1).contains(&priority) {
        return Err(QueueError::InvalidSlug(format!("priority {priority}")));
    }
    let _lock = FileLock::acquire(
        lock_dir,
        "dev-queue",
        "set dev queue priority",
        cancellation,
    )
    .await?;
    let path = queue_dir(dev_dir).join(format!("{slug}.json"));
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(QueueError::Io { path, source }),
    };
    let mut waiter: DevWaiter = match serde_json::from_slice(&bytes) {
        Ok(waiter) => waiter,
        Err(_) => {
            remove_malformed(&path).await;
            return Ok(None);
        }
    };
    if waiter.slug != slug || !pid_alive(waiter.pid) {
        remove_malformed(&path).await;
        return Ok(None);
    }
    waiter.priority = priority;
    write_waiter(dev_dir, &waiter).await?;
    Ok(Some(waiter))
}

async fn write_waiter(dev_dir: &Path, waiter: &DevWaiter) -> Result<(), QueueError> {
    let dir = queue_dir(dev_dir);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|source| QueueError::Io {
            path: dir.clone(),
            source,
        })?;
    let path = dir.join(format!("{}.json", waiter.slug));
    let temp = dir.join(format!(".{}.{}.tmp", waiter.slug, std::process::id()));
    let encoded = serde_json::to_vec(waiter).map_err(|source| QueueError::Json {
        path: path.clone(),
        source,
    })?;
    let mut file = tokio::fs::File::create(&temp)
        .await
        .map_err(|source| QueueError::Io {
            path: temp.clone(),
            source,
        })?;
    file.write_all(&encoded)
        .await
        .map_err(|source| QueueError::Io {
            path: temp.clone(),
            source,
        })?;
    file.sync_all().await.map_err(|source| QueueError::Io {
        path: temp.clone(),
        source,
    })?;
    drop(file);
    tokio::fs::rename(&temp, &path)
        .await
        .map_err(|source| QueueError::Io {
            path: path.clone(),
            source,
        })?;
    #[cfg(unix)]
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|source| QueueError::Io { path, source })?;
    Ok(())
}

async fn remove_malformed(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal zero only probes process existence.
        if unsafe { libc::kill(pid as i32, 0) } == 0 {
            return true;
        }
        io::Error::last_os_error().kind() == io::ErrorKind::PermissionDenied
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn validate_slug(slug: &str) -> Result<(), QueueError> {
    if slug.is_empty()
        || slug == "."
        || slug == ".."
        || slug.contains('/')
        || slug.contains('\\')
        || slug.chars().any(char::is_control)
    {
        return Err(QueueError::InvalidSlug(slug.to_owned()));
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn scratch() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "wt-dev-queue-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn queue_reaps_dead_pid_and_keeps_live_waiter_priority_order() {
        let root = scratch();
        let dev = root.join("dev");
        let locks = root.join("locks");
        let dead = DevWaiter {
            slug: "dead".into(),
            pid: i32::MAX as u32,
            since: 1,
            priority: 1,
        };
        write_waiter(&dev, &dead).await.unwrap();
        write_waiter(
            &dev,
            &DevWaiter {
                slug: "later".into(),
                pid: std::process::id(),
                since: 20,
                priority: 0,
            },
        )
        .await
        .unwrap();
        write_waiter(
            &dev,
            &DevWaiter {
                slug: "first".into(),
                pid: std::process::id(),
                since: 30,
                priority: 1,
            },
        )
        .await
        .unwrap();

        let cancellation = CancellationToken::new();
        let report = list(&dev, &locks, &cancellation).await.unwrap();
        assert_eq!(
            report
                .waiters
                .iter()
                .map(|w| w.slug.as_str())
                .collect::<Vec<_>>(),
            ["first", "later"]
        );
        assert!(!queue_dir(&dev).join("dead.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn waiter_promotion_changes_only_an_existing_waiter() {
        let root = scratch();
        let dev = root.join("dev");
        let locks = root.join("locks");
        write_waiter(
            &dev,
            &DevWaiter {
                slug: "one".into(),
                pid: std::process::id(),
                since: 10,
                priority: 0,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            set_priority(&dev, &locks, "missing", 1, &CancellationToken::new())
                .await
                .unwrap(),
            None
        );
        let promoted = set_priority(&dev, &locks, "one", 1, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(promoted.priority, 1);
        assert_eq!(
            list(&dev, &locks, &CancellationToken::new())
                .await
                .unwrap()
                .waiters[0]
                .slug,
            "one"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn repeated_admission_keeps_fifo_and_promotion() {
        let root = scratch();
        let dev = root.join("dev");
        let locks = root.join("locks");
        let original = DevWaiter {
            slug: "one".into(),
            pid: std::process::id(),
            since: 10,
            priority: 1,
        };
        write_waiter(&dev, &original).await.unwrap();
        let joined = join(&dev, &locks, "one", &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(joined.since, 10);
        assert_eq!(joined.priority, 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
