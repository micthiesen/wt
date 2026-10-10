use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use wt_naming::{AiSummary, NamingCacheKey};
use wt_platform::lock::FileLock;

const CACHE_VERSION: u32 = 1;
const MAX_CACHE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 20_000;
const CACHE_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
static TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Default)]
pub(crate) struct NamingCache {
    path: PathBuf,
    entries: std::sync::Arc<Mutex<BTreeMap<String, CacheEntry>>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheEntry {
    summary: CachedSummary,
    updated_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedSummary {
    title: Option<String>,
    description: String,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: BTreeMap<String, CacheEntry>,
}

impl NamingCache {
    pub(crate) fn new(cache_root: &Path) -> Self {
        Self {
            path: cache_root.join("naming-v1.json"),
            entries: Default::default(),
        }
    }

    pub(crate) async fn load(&self) {
        let bytes = match read_bounded(&self.path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                tracing::warn!(path = %self.path.display(), error = %error, "ignoring unreadable naming cache");
                return;
            }
        };
        let parsed = serde_json::from_slice::<CacheFile>(&bytes);
        let Ok(cache) = parsed else {
            tracing::warn!(path = %self.path.display(), "ignoring invalid naming cache");
            return;
        };
        if cache.version != CACHE_VERSION {
            return;
        }
        let now = now_ms();
        let entries = cache
            .entries
            .into_iter()
            .filter(|(_, entry)| now.saturating_sub(entry.updated_at_ms) <= CACHE_MAX_AGE_MS)
            .take(MAX_CACHE_ENTRIES)
            .collect();
        *self.entries.lock().await = entries;
    }

    pub(crate) async fn get(&self, key: &NamingCacheKey) -> Option<AiSummary> {
        let cache_key = key.as_str();
        self.entries
            .lock()
            .await
            .get(&cache_key)
            .filter(|entry| now_ms().saturating_sub(entry.updated_at_ms) <= CACHE_MAX_AGE_MS)
            .map(|entry| entry.summary.clone().into())
    }

    pub(crate) async fn put(
        &self,
        key: &NamingCacheKey,
        summary: AiSummary,
        lock_dir: &Path,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let mut current = self.entries.lock().await;
        let _lock = FileLock::acquire(
            lock_dir,
            "naming-cache",
            "update naming cache",
            cancellation,
        )
        .await
        .context("lock naming cache")?;
        let mut entries = read_cache_file(&self.path).await.unwrap_or_default();
        let now = now_ms();
        entries.retain(|_, entry| now.saturating_sub(entry.updated_at_ms) <= CACHE_MAX_AGE_MS);
        entries.insert(
            key.as_str(),
            CacheEntry {
                summary: CachedSummary::from(summary.clone()),
                updated_at_ms: now,
            },
        );
        if entries.len() > MAX_CACHE_ENTRIES {
            let mut ordered: Vec<_> = entries.into_iter().collect();
            ordered.sort_by_key(|(_, entry)| entry.updated_at_ms);
            entries = ordered.into_iter().rev().take(MAX_CACHE_ENTRIES).collect();
        }
        write_cache_file(&self.path, &entries)
            .await
            .context("write naming cache")?;
        *current = entries;
        Ok(())
    }

    pub(crate) async fn clear(
        &self,
        lock_dir: &Path,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let mut current = self.entries.lock().await;
        let _lock = FileLock::acquire(
            lock_dir,
            "naming-cache",
            "clear derived naming cache",
            cancellation,
        )
        .await
        .context("lock naming cache")?;
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove derived naming cache"),
        }
        current.clear();
        Ok(())
    }
}

impl From<CachedSummary> for AiSummary {
    fn from(value: CachedSummary) -> Self {
        Self {
            title: value.title,
            description: value.description,
        }
    }
}

impl From<AiSummary> for CachedSummary {
    fn from(value: AiSummary) -> Self {
        Self {
            title: value.title,
            description: value.description,
        }
    }
}

async fn read_cache_file(path: &Path) -> Result<BTreeMap<String, CacheEntry>> {
    let bytes = match read_bounded(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    let file: CacheFile = serde_json::from_slice(&bytes).context("parse cache")?;
    if file.version != CACHE_VERSION {
        return Ok(BTreeMap::new());
    }
    Ok(file.entries)
}

/// Bound memory before reading. The metadata check rejects already-oversized
/// files cheaply; `take` also caps a file that grows after metadata was read.
async fn read_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let file = tokio::fs::File::open(path).await?;
    if file.metadata().await?.len() > MAX_CACHE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("cache file exceeds {MAX_CACHE_BYTES} bytes"),
        ));
    }
    let mut bytes = Vec::with_capacity(MAX_CACHE_BYTES.min(64 * 1024) as usize);
    file.take(MAX_CACHE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("cache file exceeds {MAX_CACHE_BYTES} bytes"),
        ));
    }
    Ok(bytes)
}

async fn write_cache_file(path: &Path, entries: &BTreeMap<String, CacheEntry>) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec(&CacheFile {
        version: CACHE_VERSION,
        entries: entries.clone(),
    })?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        anyhow::bail!("serialized naming cache exceeds {MAX_CACHE_BYTES} bytes");
    }
    let parent = path.parent().context("naming cache has no parent")?;
    let temp = parent.join(format!(
        "naming-v1.{}.{}.tmp",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .await?;
    use tokio::io::AsyncWriteExt;
    if let Err(error) = async {
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        tokio::fs::rename(&temp, path).await?;
        Ok::<_, std::io::Error>(())
    }
    .await
    {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error.into());
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

pub(crate) fn title_revision(state: &serde_json::Value, slug: &str) -> u64 {
    state["slugs"][slug]["manualTitleRevision"]
        .as_u64()
        .unwrap_or(0)
}

pub(crate) fn manual_title(state: &serde_json::Value, slug: &str) -> Option<String> {
    state["slugs"][slug]["manualTitle"]
        .as_str()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cache_is_shared_by_content_key_and_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let cache = NamingCache::new(dir.path());
        cache.load().await;
        let key = NamingCacheKey::Diff("same-content".into());
        let summary = AiSummary {
            title: Some("Add shared name".into()),
            description: "Description".into(),
        };
        cache
            .put(&key, summary.clone(), dir.path(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(cache.get(&key).await, Some(summary.clone()));
        let reloaded = NamingCache::new(dir.path());
        reloaded.load().await;
        assert_eq!(reloaded.get(&key).await, Some(summary));
        assert!(
            tokio::fs::read(dir.path().join("naming-v1.json"))
                .await
                .unwrap()
                .len()
                < MAX_CACHE_BYTES as usize
        );
    }

    #[tokio::test]
    async fn clearing_derived_cache_removes_memory_and_disk_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = NamingCache::new(dir.path());
        let key = NamingCacheKey::Diff("clear-me".into());
        cache
            .put(
                &key,
                AiSummary {
                    title: Some("Derived title".into()),
                    description: "Derived description".into(),
                },
                dir.path(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        cache
            .clear(dir.path(), &CancellationToken::new())
            .await
            .unwrap();

        assert!(cache.get(&key).await.is_none());
        assert!(
            tokio::fs::metadata(dir.path().join("naming-v1.json"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn oversized_cache_is_rejected_before_reading_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("naming-v1.json");
        let file = tokio::fs::File::create(&path).await.unwrap();
        file.set_len(MAX_CACHE_BYTES + 1).await.unwrap();

        let cache = NamingCache::new(dir.path());
        cache.load().await;
        assert!(
            cache
                .get(&NamingCacheKey::Diff("anything".into()))
                .await
                .is_none()
        );
        assert!(matches!(
            read_cache_file(&path).await,
            Err(error) if error.to_string().contains("exceeds")
        ));
    }

    #[test]
    fn title_revision_and_empty_manual_title_are_distinct() {
        let state =
            serde_json::json!({"slugs":{"one":{"manualTitle":"  ","manualTitleRevision":4}}});
        assert_eq!(title_revision(&state, "one"), 4);
        assert_eq!(manual_title(&state, "one"), None);
    }
}
