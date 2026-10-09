//! Filesystem-driven invalidation with a low-frequency reconciliation backstop.
//! Watch Git metadata and checkout roots, not generated dependency trees.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use wt_config::Config;
use wt_runtime::{SourceHandle, SourceState, TaskScope};
use wt_tui::Board;
use wt_vcs::GitRepository;

const BACKSTOP: Duration = Duration::from_secs(60);

pub fn start(
    scope: &TaskScope,
    config: Arc<Config>,
    repository: Arc<GitRepository>,
    source: SourceHandle<Board>,
) {
    let cancellation = scope.token();
    scope.spawn(async move {
        let observed = source.clone();
        let watcher = tokio::task::spawn_blocking(move || {
            notify::recommended_watcher(move |event: notify::Result<Event>| match event {
                Ok(event) if relevant(&event) => { observed.refresh(); }
                Ok(_) => {},
                Err(error) => {
                    tracing::warn!(%error, "filesystem notification lost; refreshing source");
                    observed.refresh();
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
                _ = backstop.tick() => { source.refresh(); reconcile = true; }
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
