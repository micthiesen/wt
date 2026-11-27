//! Removing installed versions nothing can still run.
//!
//! Every install leaves its version directory behind, and a version stays
//! runnable for as long as something names it: the install state, a running
//! process, or a session whose `PATH` was pinned to it when it started.

use std::{collections::BTreeSet, fs, io, path::PathBuf, time::SystemTime};

use crate::{InstallPaths, InstallState, StateStore, StoreError};

/// Versions kept beyond the ones the install state names and the ones in
/// use, newest first: rollback targets.
pub const KEEP_NEWEST_VERSIONS: usize = 3;

/// The version directories under `paths` that may be removed: every one
/// that `state` does not name as current, last good or part of a pending
/// boot, that is not in `in_use` (version keys a running process or a
/// session is pinned to), and that is not among the `keep_newest` most
/// recently installed of the rest.
pub fn prunable_versions(
    paths: &InstallPaths,
    state: &InstallState,
    in_use: &BTreeSet<String>,
    keep_newest: usize,
) -> io::Result<Vec<PathBuf>> {
    let named: BTreeSet<String> = [
        state.current.as_ref(),
        state.last_good.as_ref(),
        state.pending_boot.as_ref().map(|boot| &boot.candidate),
        state
            .pending_boot
            .as_ref()
            .and_then(|boot| boot.fallback.as_ref()),
    ]
    .into_iter()
    .flatten()
    .map(crate::VersionId::key)
    .collect();
    let entries = match fs::read_dir(paths.root().join("versions")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut others: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry?;
        let key = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_dir() || named.contains(&key) || in_use.contains(&key) {
            continue;
        }
        others.push((entry.metadata()?.modified()?, entry.path()));
    }
    others.sort_by(|a, b| b.cmp(a));
    Ok(others
        .into_iter()
        .skip(keep_newest)
        .map(|(_, path)| path)
        .collect())
}

/// Remove what [`prunable_versions`] allows, under the install state's lock
/// so no install or rollback is choosing a version meanwhile, and return the
/// directories removed. A busy lock removes nothing.
pub fn prune_versions(
    paths: &InstallPaths,
    in_use: &BTreeSet<String>,
    keep_newest: usize,
) -> Result<Vec<PathBuf>, StoreError> {
    let store = StateStore::new(paths.clone());
    let _lock = match store.lock() {
        Ok(lock) => lock,
        Err(StoreError::Busy) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let state = store.load()?;
    let io_error = |source| StoreError::Io {
        path: paths.root().join("versions"),
        source,
    };
    let mut removed = Vec::new();
    for path in prunable_versions(paths, &state, in_use, keep_newest).map_err(io_error)? {
        fs::remove_dir_all(&path).map_err(|source| StoreError::Io {
            path: path.clone(),
            source,
        })?;
        removed.push(path);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Channel, VersionId};
    use std::time::Duration;

    fn version(release: &str) -> VersionId {
        VersionId::new(release, "build", "target").unwrap()
    }

    #[test]
    fn only_versions_nothing_names_or_runs_are_removed_past_the_newest_three() {
        let dir = tempfile::tempdir().unwrap();
        let paths = InstallPaths::new(dir.path()).unwrap();
        let releases = ["v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8", "v9"];
        let base = SystemTime::now() - Duration::from_secs(3600);
        for (age, release) in releases.iter().enumerate() {
            let path = paths.version_dir(&version(release));
            fs::create_dir_all(path.join("bin")).unwrap();
            fs::write(path.join("bin/wt"), release).unwrap();
            fs::File::open(&path)
                .unwrap()
                .set_modified(base + Duration::from_secs(age as u64 * 60))
                .unwrap();
        }
        let mut state = InstallState::new(Channel::default());
        state.current = Some(version("v9"));
        state.last_good = Some(version("v8"));
        state.pending_boot = Some(crate::PendingBoot {
            candidate: version("v9"),
            fallback: Some(version("v1")),
            attempt_token: "token".into(),
            started_unix: 0,
        });
        StateStore::new(paths.clone()).save(&state).unwrap();
        // A session started long ago still has v2 first on its PATH.
        let in_use = BTreeSet::from([version("v2").key()]);

        let removed = prune_versions(&paths, &in_use, KEEP_NEWEST_VERSIONS).unwrap();
        let mut removed: Vec<String> = removed
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        removed.sort();
        assert_eq!(removed, [version("v3").key(), version("v4").key()]);
        let mut kept: Vec<String> = fs::read_dir(dir.path().join("versions"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        kept.sort();
        assert_eq!(
            kept,
            ["v1", "v2", "v5", "v6", "v7", "v8", "v9"].map(|release| version(release).key())
        );

        // While an install holds the state lock, nothing is removed.
        let store = StateStore::new(paths.clone());
        let lock = store.lock().unwrap();
        assert!(
            prune_versions(&paths, &BTreeSet::new(), 0)
                .unwrap()
                .is_empty()
        );
        drop(lock);
    }
}
