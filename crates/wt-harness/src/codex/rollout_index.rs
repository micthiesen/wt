//! Cache directory membership, never session ownership. Missing IDs can be
//! queried repeatedly without enumerating or statting every historical file.
//! Directory identity/mtime/ctime witnesses expose new files and date folders;
//! selected files still pass the ordinary header and slot-ownership checks.
use super::file_cache::FileCache;
use std::{
    collections::HashSet,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Entries {
    directories: Vec<PathBuf>,
    rollouts: Vec<PathBuf>,
}

pub(super) struct RolloutIndex {
    directories: FileCache<Arc<Entries>>,
    #[cfg(test)]
    reads: std::sync::atomic::AtomicUsize,
}

impl RolloutIndex {
    pub(super) fn new() -> Self {
        Self {
            directories: FileCache::new(),
            #[cfg(test)]
            reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(super) fn matching(
        &self,
        root: &Path,
        id: &str,
        cancel: Option<&CancellationToken>,
    ) -> io::Result<Vec<PathBuf>> {
        let mut stack = vec![root.to_owned()];
        let mut matches = Vec::new();
        let mut visited = HashSet::new();
        while let Some(directory) = stack.pop() {
            check_cancelled(cancel)?;
            if let Ok(identity) = fs::canonicalize(&directory)
                && !visited.insert(identity)
            {
                continue;
            }
            let entries = self.directories.read::<io::Error>(&directory, |_| {
                #[cfg(test)]
                self.reads
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut entries = Entries::default();
                for entry in fs::read_dir(&directory)? {
                    check_cancelled(cancel)?;
                    let entry = entry?;
                    // dirent carries file type on supported filesystems. The
                    // old path.is_dir() issued a stat for every history file.
                    let kind = entry.file_type()?;
                    let path = entry.path();
                    if kind.is_dir() || kind.is_symlink() {
                        entries.directories.push(path.clone());
                    }
                    if !kind.is_dir() && super::is_rollout_path(&path) {
                        entries.rollouts.push(path);
                    }
                }
                Ok(Arc::new(entries))
            });
            let entries = match entries {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
                // A removed/inaccessible directory supplies no paths. Its
                // membership is retried on the next call, never negative-TTL'd.
                Err(_) => continue,
            };
            stack.extend(entries.directories.iter().cloned());
            matches.extend(
                entries
                    .rollouts
                    .iter()
                    .filter(|path| {
                        path.file_name()
                            .is_some_and(|name| name.to_string_lossy().contains(id))
                            // A symlink may point at a directory named like a
                            // rollout. Stat only filename matches, never the
                            // unrelated history files we are avoiding.
                            && fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
                    })
                    .cloned(),
            );
        }
        Ok(matches)
    }
}

fn check_cancelled(cancel: Option<&CancellationToken>) -> io::Result<()> {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "rollout lookup cancelled",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[cfg(unix)]
    #[test]
    fn symlink_targets_can_appear_without_parent_changes_and_cycles_terminate() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let target = external.path().join("day");
        symlink(&target, root.path().join("rollout-thread.jsonl")).unwrap();
        let index = RolloutIndex::new();
        assert!(
            index
                .matching(root.path(), "thread", None)
                .unwrap()
                .is_empty()
        );
        fs::create_dir(&target).unwrap();
        fs::write(target.join("rollout-thread.jsonl"), "").unwrap();
        symlink(root.path(), target.join("cycle")).unwrap();
        assert_eq!(
            index.matching(root.path(), "thread", None).unwrap().len(),
            1
        );
        // Same path, entirely new directory identity and contents.
        fs::rename(&target, external.path().join("old-day")).unwrap();
        fs::create_dir(&target).unwrap();
        assert!(
            index
                .matching(root.path(), "thread", None)
                .unwrap()
                .is_empty()
        );
        fs::write(target.join("rollout-thread.jsonl"), "").unwrap();
        assert_eq!(
            index.matching(root.path(), "thread", None).unwrap().len(),
            1
        );
    }

    #[test]
    fn repeated_missing_ids_reuse_membership_and_new_files_and_directories_are_visible() {
        let root = tempfile::tempdir().unwrap();
        let day = root.path().join("2026/10/13");
        fs::create_dir_all(&day).unwrap();
        for n in 0..200 {
            fs::write(day.join(format!("rollout-unrelated-{n}.jsonl")), "").unwrap();
        }
        let index = RolloutIndex::new();
        assert!(
            index
                .matching(root.path(), "missing", None)
                .unwrap()
                .is_empty()
        );
        let reads = index.reads.load(Ordering::Relaxed);
        for _ in 0..20 {
            assert!(
                index
                    .matching(root.path(), "missing", None)
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(index.reads.load(Ordering::Relaxed), reads);
        let path = day.join("rollout-missing.jsonl");
        fs::write(&path, "").unwrap();
        assert_eq!(
            index.matching(root.path(), "missing", None).unwrap(),
            std::slice::from_ref(&path)
        );
        assert_eq!(index.reads.load(Ordering::Relaxed), reads + 1);
        fs::remove_file(&path).unwrap();
        let next = root.path().join("2027/01/01");
        fs::create_dir_all(&next).unwrap();
        let moved = next.join("rollout-missing.jsonl");
        fs::write(&moved, "").unwrap();
        assert_eq!(
            index.matching(root.path(), "missing", None).unwrap(),
            [moved]
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            index
                .matching(root.path(), "missing", Some(&cancel))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
    }
}
