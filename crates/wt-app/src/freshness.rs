//! Filesystem-driven invalidation with a low-frequency reconciliation backstop.
//! Watch Git metadata and checkout roots, not generated dependency trees.
//! Edit timestamps therefore cover filesystem events at those watched roots;
//! this does not recursively walk worktree contents.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use wt_config::Config;
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel};
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
) -> SourceHandle<BTreeMap<String, i64>> {
    let (edits, mut edit_publisher) = source_channel();
    let edit_roots = Arc::new(RwLock::new(BTreeMap::<PathBuf, String>::new()));
    let pending_edits = Arc::new(Mutex::new(BTreeMap::<String, i64>::new()));
    let edit_changed = Arc::new(tokio::sync::Notify::new());
    let cancellation = scope.token();
    scope.spawn(async move {
        if let Some(board) = source.snapshot().data
            && let Ok(mut roots) = edit_roots.write()
        {
            *roots = board
                .rows
                .iter()
                .map(|row| (PathBuf::from(&row.path), row.key.clone()))
                .collect();
        }
        let observed_git = git.clone();
        let observed_metadata = metadata.clone();
        let state_db = config.paths.state_db.clone();
        let checkout_roots = [config.paths.main_clone.clone(), config.paths.worktree_root.clone()];
        let callback_roots = edit_roots.clone();
        let callback_pending = pending_edits.clone();
        let callback_changed = edit_changed.clone();
        let mut watcher = tokio::task::spawn_blocking(move || {
            notify::recommended_watcher(move |event: notify::Result<Event>| match event {
                Ok(event) if relevant(&event) => {
                    let (git, metadata) = invalidations(&event.paths, &state_db, &checkout_roots);
                    if git { observed_git.refresh(); }
                    if metadata { observed_metadata.refresh(); }
                    if edit_event(&event) {
                        let roots = callback_roots.read().ok();
                        let mut changed = false;
                        if let Some(roots) = roots.as_deref() {
                            let timestamp = now_ms();
                            let slugs: BTreeSet<_> = event.paths.iter().take(256)
                                .filter(|path| !is_state_db_path(path, &state_db))
                                .filter_map(|path| edited_slug(path, roots))
                                .collect();
                            if !slugs.is_empty()
                                && let Ok(mut pending) = callback_pending.lock()
                            {
                                for slug in slugs {
                                    let value = pending.entry(slug).or_default();
                                    *value = (*value).max(timestamp);
                                    changed = true;
                                }
                            }
                        }
                        if changed {
                            callback_changed.notify_one();
                        }
                    }
                }
                Ok(_) => {},
                Err(error) => {
                    tracing::warn!(%error, "filesystem notification lost; refreshing source");
                    observed_git.refresh();
                    observed_metadata.refresh();
                }
            })
        });
        let watcher = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                // `spawn_blocking` cannot be aborted. Drain its result so a
                // watcher created concurrently with shutdown is dropped here
                // instead of being detached from the source lifetime.
                if let Ok(Ok(watcher)) = watcher.await {
                    drop(watcher);
                }
                return;
            }
            watcher = &mut watcher => watcher,
        };
        let mut watcher = match watcher {
            Ok(Ok(watcher)) => Some(watcher),
            error => {
                tracing::warn!(?error, "filesystem watcher unavailable; using refresh backstop");
                None
            }
        };
        let mut registered = BTreeMap::new();
        let mut updates = source.subscribe();
        updates.mark_changed();
        let mut keys = Vec::new();
        let mut latest_edits = BTreeMap::<String, i64>::new();
        let mut edits_published = false;
        // File invalidation belongs to the host even when no automation
        // subscribes to the optional edit-timestamp stream.
        let mut edits_open = true;
        let mut reconcile = true;
        let mut backstop = tokio::time::interval_at(tokio::time::Instant::now() + BACKSTOP, BACKSTOP);
        backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if reconcile && let Some(active) = watcher.take() {
                let desired = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        drop(active);
                        break;
                    }
                    desired = desired_paths(&config, &repository, &cancellation) => desired,
                };
                match desired {
                    Ok(paths) => {
                        let previous_registered = registered.clone();
                        let update_cancel = cancellation.clone();
                        let mut update = tokio::task::spawn_blocking(move || {
                            update_watches(active, registered, paths, &update_cancel)
                        });
                        let update = tokio::select! {
                            biased;
                            _ = cancellation.cancelled() => {
                                // The blocking watcher update owns `active`.
                                // Join it and drop the returned watcher before
                                // this source task exits.
                                if let Ok((watcher, _, _)) = update.await {
                                    drop(watcher);
                                }
                                break;
                            }
                            update = &mut update => update,
                        };
                        match update {
                            Ok((active, paths, cancelled)) => {
                                if cancelled {
                                    drop(active);
                                    break;
                                }
                                let watch_set_changed = paths != previous_registered;
                                watcher = Some(active);
                                registered = paths;
                                if watch_set_changed {
                                    // Inventory can finish before its paths are
                                    // watched. A single follow-up scan closes that
                                    // gap without delaying the first board render.
                                    git.refresh();
                                }
                                if !edits_published {
                                    edits_published = true;
                                    edit_publisher.publish(SourceSnapshot {
                                        data: Some(Arc::new(latest_edits.clone())),
                                        state: SourceState::Ready,
                                        updated_at: Some(tokio::time::Instant::now()),
                                        revision: 0,
                                    });
                                }
                            }
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
                _ = cancellation.cancelled() => {
                    break;
                },
                changed = updates.changed() => {
                    if changed.is_err() { break; }
                    let snapshot = updates.borrow_and_update().clone();
                    if snapshot.state == SourceState::Ready && let Some(board) = snapshot.data {
                        let next: Vec<_> = board.rows.iter().map(|row| (row.key.clone(), row.path.clone())).collect();
                        if let Ok(mut roots) = edit_roots.write() {
                            *roots = board.rows.iter()
                                .map(|row| (PathBuf::from(&row.path), row.key.clone()))
                                .collect();
                        }
                        let live_keys: BTreeSet<_> = board.rows.iter().map(|row| row.key.as_str()).collect();
                        latest_edits.retain(|slug, _| live_keys.contains(slug.as_str()));
                        reconcile = next != keys;
                        keys = next;
                    }
                }
                request = edit_publisher.requested(), if edits_open => {
                    if request.is_none() { edits_open = false; }
                    else { edit_changed.notify_one(); }
                }
                _ = edit_changed.notified() => {
                    if let Ok(mut pending) = pending_edits.lock() {
                        for (slug, timestamp) in std::mem::take(&mut *pending) {
                            latest_edits.entry(slug).and_modify(|current| *current = (*current).max(timestamp)).or_insert(timestamp);
                        }
                    }
                    edit_publisher.publish(SourceSnapshot {
                        data: Some(Arc::new(latest_edits.clone())),
                        state: SourceState::Ready,
                        updated_at: Some(tokio::time::Instant::now()),
                        revision: 0,
                    });
                }
                _ = backstop.tick() => { git.refresh(); metadata.refresh(); reconcile = true; }
            }
        }
    });
    edits
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

fn edit_event(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

fn edited_slug(path: &Path, roots: &BTreeMap<PathBuf, String>) -> Option<String> {
    let (root, slug) = roots
        .iter()
        .filter(|(root, _)| path.starts_with(root))
        .max_by_key(|(root, _)| root.components().count())?;
    let relative = path.strip_prefix(root).ok()?;
    if generated_path(relative)
        || relative
            .components()
            .any(|component| matches!(component, Component::Normal(name) if name == ".git"))
    {
        return None;
    }
    Some(slug.clone())
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
    let inventory = repository
        .inventory(cancellation)
        .await
        .context("watch inventory")?;
    let common_dirs: BTreeSet<_> = inventory
        .iter()
        .filter_map(|worktree| worktree.common_dir.clone())
        .collect();
    for common_dir in &common_dirs {
        paths.insert(common_dir.clone(), false);
        for child in ["refs", "worktrees"] {
            let path = common_dir.join(child);
            if tokio::fs::try_exists(&path).await? {
                paths.insert(path, true);
            }
        }
    }
    for worktree in &inventory {
        paths.insert(PathBuf::from(&worktree.target.path), false);
        if let Some(directory) = &worktree.git_dir
            && !common_dirs
                .iter()
                .any(|common| directory.starts_with(common.join("worktrees")))
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
    cancellation: &tokio_util::sync::CancellationToken,
) -> (RecommendedWatcher, BTreeMap<PathBuf, bool>, bool) {
    if registered == desired || cancellation.is_cancelled() {
        return (watcher, registered, cancellation.is_cancelled());
    }
    // macOS restarts its FSEvents stream for each individual watch/unwatch.
    // Apply the whole inventory in one transaction so fleet size does not
    // multiply stream teardown and leave long registration gaps.
    let started = std::time::Instant::now();
    let mut changes = watcher.paths_mut();
    let obsolete: Vec<_> = registered
        .iter()
        .filter_map(|(path, recursive)| {
            (desired.get(path) != Some(recursive)).then_some(path.clone())
        })
        .collect();
    for path in obsolete {
        if cancellation.is_cancelled() {
            drop(changes);
            return (watcher, registered, true);
        }
        if let Err(error) = changes.remove(&path) {
            tracing::debug!(%error, path = %path.display(), "unwatch removed path");
        }
        registered.remove(&path);
    }
    for (path, recursive) in desired {
        if cancellation.is_cancelled() {
            drop(changes);
            return (watcher, registered, true);
        }
        if registered.contains_key(&path) {
            continue;
        }
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        match changes.add(&path, mode) {
            Ok(()) => {
                registered.insert(path, recursive);
            }
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "cannot watch path; refresh backstop remains active")
            }
        }
    }
    if let Err(error) = changes.commit() {
        tracing::warn!(%error, "cannot apply filesystem watches; reconciliation will retry");
        registered.clear();
    }
    tracing::debug!(
        paths = registered.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "filesystem watches registered"
    );
    (watcher, registered, false)
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
            !name.ends_with(".lock")
                && !matches!(name, "FETCH_HEAD" | "ORIG_HEAD")
                && !generated_path(path)
        })
}

fn generated_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => matches!(
            name.to_str(),
            Some(
                "node_modules"
                    | "target"
                    | ".next"
                    | ".turbo"
                    | "coverage"
                    | ".cache"
                    | "fsmonitor--daemon"
            )
        ),
        _ => false,
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
        if is_state_db_path(path, state_db) {
            metadata = true;
        } else if path.parent() != state_db.parent()
            || checkout_roots.iter().any(|root| path.starts_with(root))
        {
            git = true;
        }
    }
    (git, metadata)
}

fn is_state_db_path(path: &Path, state_db: &Path) -> bool {
    path == state_db
        || ["-wal", "-shm", "-journal"].iter().any(|suffix| {
            let mut name = state_db.as_os_str().to_os_string();
            name.push(suffix);
            path.as_os_str() == name
        })
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

    #[test]
    fn edit_times_match_nested_worktree_files_but_skip_generated_and_git_paths() {
        let roots = BTreeMap::from([
            (PathBuf::from("/repo/wt/one"), "one".to_owned()),
            (PathBuf::from("/repo/wt/one/nested"), "nested".to_owned()),
        ]);
        assert_eq!(
            edited_slug(Path::new("/repo/wt/one/src/main.rs"), &roots).as_deref(),
            Some("one")
        );
        assert_eq!(
            edited_slug(Path::new("/repo/wt/one/nested/src/lib.rs"), &roots).as_deref(),
            Some("nested")
        );
        for path in [
            "/repo/wt/one/.git/index",
            "/repo/wt/one/node_modules/pkg/index.js",
            "/repo/wt/one/target/debug/wt",
            "/repo/wt/one/.next/cache/data",
        ] {
            assert_eq!(edited_slug(Path::new(path), &roots), None, "{path}");
        }
        let database = Path::new("/repo/wt/one/.cache/wt.sqlite");
        assert!(is_state_db_path(database, database));
        assert!(is_state_db_path(
            Path::new("/repo/wt/one/.cache/wt.sqlite-wal"),
            database
        ));
        assert!(generated_path(Path::new(
            ".git/worktrees/one/fsmonitor--daemon/cookies/next"
        )));
    }

    #[tokio::test]
    async fn dropping_optional_edit_stream_does_not_stop_git_watch_invalidation() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let sources = crate::local_source::start(&scope, &fixture.ctx);
        let worktree = tokio::fs::canonicalize(fixture.ctx.config.paths.worktree_root.join("one"))
            .await
            .unwrap();
        let mut git_updates = sources.git.subscribe();
        let mut edit_updates = sources.edits.subscribe();
        sources.git.refresh();
        sources.metadata.refresh();
        tokio::time::timeout(
            Duration::from_secs(5),
            git_updates.wait_for(|snapshot| {
                snapshot.state == SourceState::Ready && snapshot.data.is_some()
            }),
        )
        .await
        .expect("initial Git inventory should load")
        .expect("Git source should stay open");
        tokio::time::timeout(
            Duration::from_secs(5),
            edit_updates.wait_for(|snapshot| {
                snapshot.state == SourceState::Ready && snapshot.data.is_some()
            }),
        )
        .await
        .expect("freshness watcher registration should complete")
        .expect("edit source should stay open");
        let initial_revision = git_updates.borrow().revision;

        // This receiver is optional: with automations disabled nobody observes
        // edit timestamps. The Git and metadata invalidation watcher must live on.
        drop(sources.edits);
        let tracked_file = worktree.join("tracked.txt");
        tokio::fs::write(&tracked_file, "first external edit\n")
            .await
            .unwrap();
        let first_revision = tokio::time::timeout(
            Duration::from_secs(5),
            git_updates.wait_for(|snapshot| {
                snapshot.revision > initial_revision
                    && snapshot.data.as_ref().is_some_and(|rows| {
                        rows.iter().any(|row| {
                            row.worktree.target.path == worktree
                                && row.status.as_ref().is_some_and(|status| status.dirty)
                        })
                    })
            }),
        )
        .await
        .expect("first filesystem event should refresh Git after edit stream closes")
        .expect("Git source should stay open")
        .revision;

        tokio::fs::write(&tracked_file, "second external edit\n")
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            git_updates.wait_for(|snapshot| {
                snapshot.revision > first_revision
                    && snapshot.data.as_ref().is_some_and(|rows| {
                        rows.iter().any(|row| {
                            row.worktree.target.path == worktree
                                && row.status.as_ref().is_some_and(|status| status.dirty)
                        })
                    })
            }),
        )
        .await
        .expect("the filesystem watcher should remain alive for later edits")
        .expect("Git source should stay open");

        scope.shutdown(Duration::from_secs(2)).await.unwrap();
        fixture.close().await.unwrap();
    }
}
