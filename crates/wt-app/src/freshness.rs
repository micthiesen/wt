//! Filesystem-driven invalidation with a low-frequency reconciliation backstop.
//! Watch Git metadata and checkout roots, not generated dependency trees.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use wt_config::Config;
use wt_runtime::{SourceHandle, SourceState, TaskScope};
use wt_tui::Board;
use wt_vcs::{GitRepository, WorktreeSnapshot};

const BACKSTOP: Duration = Duration::from_secs(60);

pub fn start(
    scope: &TaskScope,
    config: Arc<Config>,
    repository: Arc<GitRepository>,
    source: SourceHandle<Board>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<crate::local_source::Metadata>,
) {
    let cancellation = scope.token();
    scope.spawn(async move {
        let observed_git = git.clone();
        let observed_metadata = metadata.clone();
        let state_db = config.paths.state_db.clone();
        let checkout_roots = [config.paths.main_clone.clone(), config.paths.worktree_root.clone()];
        let watcher = tokio::task::spawn_blocking(move || {
            notify::recommended_watcher(move |event: notify::Result<Event>| match event {
                Ok(event) if relevant(&event) => {
                    let (git, metadata) = invalidations(&event.paths, &state_db, &checkout_roots);
                    if git { observed_git.refresh(); }
                    if metadata { observed_metadata.refresh(); }
                }
                Ok(_) => {},
                Err(error) => {
                    tracing::warn!(%error, "filesystem notification lost; refreshing source");
                    observed_git.refresh();
                    observed_metadata.refresh();
                }
            })
        }).await;
        let mut watcher = match watcher {
            Ok(Ok(watcher)) => Some(watcher),
            error => {
                tracing::warn!(?error, "filesystem watcher unavailable; using refresh backstop");
                None
            }
        };
        let mut registered = BTreeMap::new();
        let mut updates = source.subscribe();
        let mut keys = Vec::new();
        let mut reconcile = true;
        let mut backstop = tokio::time::interval_at(tokio::time::Instant::now() + BACKSTOP, BACKSTOP);
        backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if reconcile && let Some(active) = watcher.take() {
                match desired_paths(&config, &repository, &cancellation).await {
                    Ok(paths) => {
                        let update = tokio::task::spawn_blocking(move || update_watches(active, registered, paths)).await;
                        match update {
                            Ok((active, paths)) => { watcher = Some(active); registered = paths; }
                            Err(error) => {
                                tracing::error!(%error, "filesystem watch worker stopped; using refresh backstop");
                                registered = BTreeMap::new();
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "cannot reconcile filesystem watches");
                        watcher = Some(active);
                    }
                }
                reconcile = false;
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                changed = updates.changed() => {
                    if changed.is_err() { break; }
                    let snapshot = updates.borrow_and_update().clone();
                    if snapshot.state == SourceState::Ready && let Some(board) = snapshot.data {
                        let next: Vec<_> = board.rows.iter().map(|row| (row.key.clone(), row.path.clone())).collect();
                        reconcile = next != keys;
                        keys = next;
                    }
                }
                _ = backstop.tick() => { git.refresh(); metadata.refresh(); reconcile = true; }
            }
        }
    });
}

async fn desired_paths(
    config: &Config,
    repository: &GitRepository,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<BTreeMap<PathBuf, bool>> {
    let mut paths = BTreeMap::new();
    paths.insert(config.paths.worktree_root.clone(), false);
    paths.insert(config.paths.main_clone.clone(), false);
    if let Some(parent) = config.paths.state_db.parent() {
        paths.insert(parent.to_owned(), false);
    }
    for worktree in repository
        .inventory(cancellation)
        .await
        .context("watch inventory")?
    {
        paths.insert(PathBuf::from(worktree.target.path), false);
        for directory in [worktree.git_dir, worktree.common_dir]
            .into_iter()
            .flatten()
        {
            paths.insert(directory.clone(), false);
            for child in ["refs", "worktrees"] {
                let path = directory.join(child);
                if tokio::fs::try_exists(&path).await? {
                    paths.insert(path, true);
                }
            }
        }
    }
    Ok(paths)
}

fn update_watches(
    mut watcher: RecommendedWatcher,
    mut registered: BTreeMap<PathBuf, bool>,
    desired: BTreeMap<PathBuf, bool>,
) -> (RecommendedWatcher, BTreeMap<PathBuf, bool>) {
    registered.retain(|path, recursive| {
        if desired.get(path) == Some(recursive) {
            return true;
        }
        if let Err(error) = watcher.unwatch(path) {
            tracing::debug!(%error, path = %path.display(), "unwatch removed path");
        }
        false
    });
    for (path, recursive) in desired {
        if registered.contains_key(&path) {
            continue;
        }
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        match watcher.watch(&path, mode) {
            Ok(()) => {
                registered.insert(path, recursive);
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "cannot watch path; refresh backstop remains active")
            }
        }
    }
    (watcher, registered)
}

fn relevant(event: &Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event.paths.is_empty()
        || event.paths.iter().any(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            !name.ends_with(".lock") && !matches!(name, "FETCH_HEAD" | "ORIG_HEAD")
        })
}

/// SQLite writes and WAL/checkpoint churn only invalidate presentation state.
/// Other files in the shared state directory belong to other sources/repos.
fn invalidations(
    paths: &[PathBuf],
    state_db: &std::path::Path,
    checkout_roots: &[PathBuf],
) -> (bool, bool) {
    if paths.is_empty() {
        return (true, true);
    }
    let mut git = false;
    let mut metadata = false;
    for path in paths {
        let sqlite = path == state_db
            || ["-wal", "-shm", "-journal"].iter().any(|suffix| {
                let mut name = state_db.as_os_str().to_os_string();
                name.push(suffix);
                path.as_os_str() == name
            });
        if sqlite {
            metadata = true;
        } else if path.parent() != state_db.parent()
            || checkout_roots.iter().any(|root| path.starts_with(root))
        {
            git = true;
        }
    }
    (git, metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_events_do_not_rescan_git_and_unrelated_shared_files_do_not_refresh() {
        let db = std::path::Path::new("/state/wt.sqlite");
        for path in [
            "/state/wt.sqlite",
            "/state/wt.sqlite-wal",
            "/state/wt.sqlite-shm",
        ] {
            assert_eq!(invalidations(&[path.into()], db, &[]), (false, true));
        }
        assert_eq!(
            invalidations(&["/state/another.sqlite-wal".into()], db, &[]),
            (false, false)
        );
        assert_eq!(
            invalidations(&["/repo/.git/index".into()], db, &[]),
            (true, false)
        );
        assert_eq!(
            invalidations(
                &["/state/wt.sqlite-wal".into(), "/repo/file".into()],
                db,
                &[]
            ),
            (true, true)
        );
        assert_eq!(invalidations(&[], db, &[]), (true, true));
        assert_eq!(
            invalidations(
                &["/repo/file".into()],
                std::path::Path::new("/repo/state.sqlite"),
                &["/repo".into()]
            ),
            (true, false)
        );
    }
}
