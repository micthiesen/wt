//! Process-local parsed-file cache. Every reuse requires current filesystem
//! identity, including subsecond change time, rather than a polling TTL.
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

const CAPACITY: usize = 8192;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl Fingerprint {
    fn new(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            },
        }
    }
}

pub(super) struct FileCache<T>(Mutex<HashMap<PathBuf, (Fingerprint, T)>>);

impl<T: Clone> FileCache<T> {
    pub(super) fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }

    pub(super) fn read<E: From<io::Error>>(
        &self,
        path: &Path,
        mut read: impl FnMut(&fs::Metadata) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut entries = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        for _ in 0..3 {
            let metadata = match fs::metadata(path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    entries.remove(path);
                    return Err(error.into());
                }
            };
            let fingerprint = Fingerprint::new(&metadata);
            if let Some((prior, value)) = entries.get(path)
                && prior == &fingerprint
            {
                return Ok(value.clone());
            }
            entries.remove(path);
            let result = read(&metadata);
            // Neither return nor cache a value whose bytes may belong to a
            // different revision than its metadata. Even a parse error may be
            // caused by a concurrent append, so revalidate before resolving it.
            if !fs::metadata(path).is_ok_and(|after| Fingerprint::new(&after) == fingerprint) {
                continue;
            }
            let value = result?;
            if entries.len() >= CAPACITY {
                entries.clear();
            }
            entries.insert(path.to_owned(), (fingerprint, value.clone()));
            return Ok(value);
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!(
                "file changed during three read attempts: {}",
                path.display()
            ),
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn caches_exclusions_and_invalidates_rewrites_replacements_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout");
        fs::write(&path, "old").unwrap();
        let cache = FileCache::new();
        let reads = Cell::new(0);
        let read = || {
            cache.read::<io::Error>(&path, |_| {
                reads.set(reads.get() + 1);
                Ok((fs::read_to_string(&path)? == "new").then_some("matched"))
            })
        };
        assert_eq!(read().unwrap(), None);
        assert_eq!(read().unwrap(), None);
        assert_eq!(reads.get(), 1);
        fs::write(&path, "new").unwrap();
        assert_eq!(read().unwrap(), Some("matched"));
        let replacement = dir.path().join("replacement");
        fs::write(&replacement, "old").unwrap();
        fs::rename(replacement, &path).unwrap();
        assert_eq!(read().unwrap(), None);
        fs::remove_file(&path).unwrap();
        assert!(read().is_err());
        fs::write(&path, "new").unwrap();
        assert_eq!(read().unwrap(), Some("matched"));
        assert_eq!(reads.get(), 4);
    }

    #[test]
    fn retries_replacement_during_read_without_returning_mixed_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout");
        fs::write(&path, "old").unwrap();
        let cache = FileCache::new();
        let mut reads = 0;
        assert_eq!(
            cache
                .read::<io::Error>(&path, |metadata| {
                    reads += 1;
                    if reads == 1 {
                        let replacement = dir.path().join("replacement");
                        fs::write(&replacement, "changed")?;
                        fs::rename(replacement, &path)?;
                    }
                    Ok((metadata.len(), fs::read_to_string(&path)?))
                })
                .unwrap(),
            (7, "changed".to_owned())
        );
        assert_eq!(reads, 2);
        assert_eq!(
            cache
                .read::<io::Error>(&path, |_| panic!("stable revision should be cached"))
                .unwrap(),
            (7, "changed".to_owned())
        );
    }

    #[test]
    fn rejects_continuously_changing_files_after_bounded_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout");
        fs::write(&path, "old").unwrap();
        let cache = FileCache::new();
        let mut reads = 0;
        let error = cache
            .read::<io::Error>(&path, |metadata| {
                reads += 1;
                fs::write(&path, "x".repeat(metadata.len() as usize + 1))?;
                Ok(metadata.len())
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(reads, 3);
        assert_eq!(
            cache
                .read::<io::Error>(&path, |metadata| Ok(metadata.len()))
                .unwrap(),
            6
        );
    }
}
