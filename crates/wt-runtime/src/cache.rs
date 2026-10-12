//! Disposable per-source snapshot cache. A cache file only ever produces
//! `Provenance::Restored` data; nothing read here can be confirmed.
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

const FORMAT: u32 = 1;
pub const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CACHE_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Where cache files live and which configuration they belong to. The scope
/// string must change whenever restored data could mean something different:
/// another repository, configuration or build.
#[derive(Clone, Debug)]
pub struct CacheStore {
    inner: Arc<StoreInner>,
}

#[derive(Debug)]
struct StoreInner {
    directory: PathBuf,
    scope: String,
}

impl CacheStore {
    pub fn new(directory: PathBuf, scope: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(StoreInner {
                directory,
                scope: scope.into(),
            }),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.inner.directory
    }

    pub fn scope(&self) -> &str {
        &self.inner.scope
    }

    fn path(&self, name: &str) -> PathBuf {
        self.inner.directory.join(format!("{name}.json"))
    }
}

#[derive(Serialize)]
struct EnvelopeOut<'a, T> {
    format: u32,
    name: &'a str,
    scope: &'a str,
    saved_at_ms: u64,
    data: &'a T,
}

#[derive(Deserialize)]
struct EnvelopeIn<T> {
    format: u32,
    name: String,
    scope: String,
    saved_at_ms: u64,
    data: T,
}

/// Why a cache file was not used. Every variant deletes the file.
#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    Missing,
    TooLarge,
    Corrupt(String),
    Mismatched,
    Expired,
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// A typed cache slot. Constructing one requires a serializable type, which
/// is how a source opts into persistence.
pub struct DiskCache<T> {
    slot: Arc<dyn CacheSlot<T>>,
}

impl<T> Clone for DiskCache<T> {
    fn clone(&self) -> Self {
        Self {
            slot: self.slot.clone(),
        }
    }
}

impl<T: Serialize + DeserializeOwned + Send + Sync + 'static> DiskCache<T> {
    pub fn new(store: &CacheStore) -> Self {
        Self {
            slot: Arc::new(SerdeSlot::<T> {
                store: store.clone(),
                _type: PhantomData,
            }),
        }
    }
}

impl<T> DiskCache<T> {
    pub(crate) fn slot(&self) -> Arc<dyn CacheSlot<T>> {
        self.slot.clone()
    }

    #[cfg(test)]
    fn read(&self, name: &str) -> Result<(T, u64), Rejected> {
        self.slot.read(name)
    }

    #[cfg(test)]
    fn write(&self, name: &str, data: &T) -> std::io::Result<()> {
        self.slot.write(name, data)
    }
}

struct SerdeSlot<T> {
    store: CacheStore,
    _type: PhantomData<fn() -> T>,
}

/// Erased read/write so the generic source code needs no serde bounds.
pub(crate) trait CacheSlot<T>: Send + Sync {
    fn read(&self, name: &str) -> Result<(T, u64), Rejected>;
    fn write(&self, name: &str, data: &T) -> std::io::Result<()>;
}

impl<T: Serialize + DeserializeOwned + Send + Sync + 'static> CacheSlot<T> for SerdeSlot<T> {
    fn read(&self, name: &str) -> Result<(T, u64), Rejected> {
        let path = self.store.path(name);
        let result = read_envelope(&path, name, self.store.scope(), now_ms());
        if let Err(reason) = &result
            && *reason != Rejected::Missing
        {
            tracing::warn!(source = name, ?reason, path = %path.display(), "discarding source cache");
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(source = name, %error, "cannot delete discarded source cache");
            }
        }
        result
    }

    fn write(&self, name: &str, data: &T) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(&EnvelopeOut {
            format: FORMAT,
            name,
            scope: self.store.scope(),
            saved_at_ms: now_ms(),
            data,
        })
        .map_err(std::io::Error::other)?;
        if bytes.len() > MAX_CACHE_BYTES {
            return Err(std::io::Error::other("source cache exceeds size limit"));
        }
        let directory = self.store.directory();
        std::fs::create_dir_all(directory)?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        file.write_all(&bytes)?;
        file.persist(self.store.path(name))
            .map_err(|error| error.error)?;
        Ok(())
    }
}

fn read_envelope<T: DeserializeOwned>(
    path: &Path,
    name: &str,
    scope: &str,
    now_ms: u64,
) -> Result<(T, u64), Rejected> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Rejected::Missing);
        }
        Err(error) => return Err(Rejected::Corrupt(error.to_string())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CACHE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| Rejected::Corrupt(error.to_string()))?;
    if bytes.len() > MAX_CACHE_BYTES {
        return Err(Rejected::TooLarge);
    }
    // Check identity before decoding the payload: a different build may have
    // a different shape that serde would fill with defaults.
    #[derive(Deserialize)]
    struct Header {
        format: u32,
        name: String,
        scope: String,
        saved_at_ms: u64,
    }
    let header: Header =
        serde_json::from_slice(&bytes).map_err(|error| Rejected::Corrupt(error.to_string()))?;
    if header.format != FORMAT || header.name != name || header.scope != scope {
        return Err(Rejected::Mismatched);
    }
    if header.saved_at_ms > now_ms
        || Duration::from_millis(now_ms - header.saved_at_ms) > MAX_CACHE_AGE
    {
        return Err(Rejected::Expired);
    }
    let envelope: EnvelopeIn<T> =
        serde_json::from_slice(&bytes).map_err(|error| Rejected::Corrupt(error.to_string()))?;
    debug_assert!(envelope.format == FORMAT && envelope.name == name && envelope.scope == scope);
    Ok((envelope.data, envelope.saved_at_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(directory: &Path, scope: &str) -> DiskCache<Vec<String>> {
        DiskCache::new(&CacheStore::new(directory.to_path_buf(), scope))
    }

    #[test]
    fn round_trip_and_every_rejection_deletes_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let cache = store(directory.path(), "scope-a");
        assert_eq!(cache.read("git").unwrap_err(), Rejected::Missing);
        cache.write("git", &vec!["row".into()]).unwrap();
        let (data, saved) = cache.read("git").unwrap();
        assert_eq!(data, vec!["row".to_owned()]);
        assert!(saved <= now_ms());

        // Another configuration or build cannot read it, and discards it.
        assert_eq!(
            store(directory.path(), "scope-b").read("git").unwrap_err(),
            Rejected::Mismatched
        );
        assert!(!directory.path().join("git.json").exists());

        // Corrupt data.
        std::fs::write(directory.path().join("git.json"), b"{not json").unwrap();
        assert!(matches!(cache.read("git"), Err(Rejected::Corrupt(_))));
        assert!(!directory.path().join("git.json").exists());

        // A changed payload shape under the same identity is corrupt, not
        // silently defaulted.
        std::fs::write(
            directory.path().join("git.json"),
            serde_json::to_vec(&serde_json::json!({
                "format": FORMAT, "name": "git", "scope": "scope-a",
                "savedAtMs": 0, "saved_at_ms": now_ms(), "data": {"rows": 3}
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(cache.read("git"), Err(Rejected::Corrupt(_))));

        // Expired.
        std::fs::write(
            directory.path().join("git.json"),
            serde_json::to_vec(&serde_json::json!({
                "format": FORMAT, "name": "git", "scope": "scope-a",
                "saved_at_ms": now_ms() - MAX_CACHE_AGE.as_millis() as u64 - 1000,
                "data": ["old"]
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(cache.read("git").unwrap_err(), Rejected::Expired);
        assert!(!directory.path().join("git.json").exists());

        // Oversized.
        std::fs::write(
            directory.path().join("git.json"),
            vec![b' '; MAX_CACHE_BYTES + 1],
        )
        .unwrap();
        assert_eq!(cache.read("git").unwrap_err(), Rejected::TooLarge);
        assert!(!directory.path().join("git.json").exists());
    }
}
