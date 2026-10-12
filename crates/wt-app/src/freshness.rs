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
use wt_core::Domain;
use wt_runtime::{
    Persistence, RefreshPolicy, SourceHandle, SourceSpec, SourceState, Stamp, TaskScope,
    leaf_channel,
};
use wt_tui::Board;
use wt_vcs::{GitRepository, WorktreeSnapshot};

const BACKSTOP: Duration = Duration::from_secs(60);
const METADATA_BACKSTOP: Duration = Duration::from_secs(1);
const MAX_METADATA_ENTRIES: usize = 8192;

pub fn start(
    scope: &TaskScope,
    config: Arc<Config>,
    repository: Arc<GitRepository>,
    source: SourceHandle<Board>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<crate::local_source::Metadata>,
) -> SourceHandle<BTreeMap<String, i64>> {
    start_with_notifications(scope, config, repository, source, git, metadata, true)
}

fn start_with_notifications(
    scope: &TaskScope,
    config: Arc<Config>,
    repository: Arc<GitRepository>,
    source: SourceHandle<Board>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<crate::local_source::Metadata>,
    notifications: bool,
) -> SourceHandle<BTreeMap<String, i64>> {
    let (edits, mut edit_publisher) = leaf_channel(
        scope,
        SourceSpec {
            name: "edit-times",
            domain: Domain::Git,
            persistence: Persistence::Never("live filesystem events"),
            policy: RefreshPolicy::default(),
        },
    );
    let edits = edits.view();
    let edit_roots = Arc::new(RwLock::new(BTreeMap::<PathBuf, String>::new()));
    let pending_edits = Arc::new(Mutex::new(BTreeMap::<String, i64>::new()));
    let edit_changed = Arc::new(tokio::sync::Notify::new());
    let cancellation = scope.token();
    scope.spawn(async move {
        if let Some(board) = source.snapshot().data().cloned()
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
        let state_db = canonical_watch_path(&config.paths.state_db).await;
        let checkout_roots = [
            canonical_watch_path(&config.paths.main_clone).await,
            canonical_watch_path(&config.paths.worktree_root).await,
        ];
        let probe_state_db = state_db.clone();
        let probe_checkout_roots = checkout_roots.clone();
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
        let mut metadata_backstop = tokio::time::interval_at(
            tokio::time::Instant::now() + METADATA_BACKSTOP,
            METADATA_BACKSTOP,
        );
        metadata_backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut fingerprint = None;
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
                            if notifications {
                                update_watches(active, registered, paths, &update_cancel)
                            } else {
                                (active, paths, false)
                            }
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
                                    edit_publisher.publish_live(latest_edits.clone(), Stamp::now());
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
                    if *snapshot.state() == SourceState::Ready && let Some(board) = snapshot.data() {
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
                    edit_publisher.publish_live(latest_edits.clone(), Stamp::now());
                }
                _ = metadata_backstop.tick() => {
                    let paths = registered.clone();
                    let probe_cancel = cancellation.clone();
                    let next = tokio::task::spawn_blocking(move || watch_fingerprint(&paths, &probe_cancel)).await;
                    if let Ok(Some(next)) = next {
                        // Refresh on the first sample too: an edit may occur
                        // between watcher registration and this baseline.
                        let changed = changed_paths(fingerprint.as_ref(), &next);
                        if !changed.is_empty() {
                            let (git_changed, metadata_changed) = invalidations(&changed, &probe_state_db, &probe_checkout_roots);
                            if git_changed { git.refresh(); }
                            if metadata_changed { metadata.refresh(); }
                        }
                        fingerprint = Some(next);
                    }
                }
                _ = backstop.tick() => { git.refresh(); metadata.refresh(); reconcile = true; }
            }
        }
    });
    edits
}

/// A bounded notification-loss backstop, not a recursive checkout scan. The
/// watch contract includes immediate checkout children and recursive Git
/// metadata only. Symlinks and generated dependency trees are never followed.
type WatchFingerprint = BTreeMap<PathBuf, (u64, Option<SystemTime>, bool)>;

fn changed_paths(previous: Option<&WatchFingerprint>, next: &WatchFingerprint) -> Vec<PathBuf> {
    let Some(previous) = previous else {
        return next.keys().cloned().collect();
    };
    previous
        .keys()
        .chain(next.keys())
        .filter(|path| previous.get(*path) != next.get(*path))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn watch_fingerprint(
    paths: &BTreeMap<PathBuf, bool>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Option<WatchFingerprint> {
    let started = std::time::Instant::now();
    let mut result = BTreeMap::new();
    let mut pending: Vec<_> = paths
        .iter()
        .map(|(path, recursive)| (path.clone(), *recursive, true))
        .collect();
    while let Some((path, recursive, descend)) = pending.pop() {
        if cancellation.is_cancelled() || started.elapsed() > Duration::from_millis(200) {
            return None;
        }
        if generated_path(&path) {
            continue;
        }
        if result.len() >= MAX_METADATA_ENTRIES {
            return None;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        result.insert(
            path.clone(),
            (metadata.len(), metadata.modified().ok(), metadata.is_dir()),
        );
        if descend && metadata.is_dir() {
            let entries = std::fs::read_dir(&path).ok()?;
            for entry in entries {
                if cancellation.is_cancelled() || started.elapsed() > Duration::from_millis(200) {
                    return None;
                }
                let path = entry.ok()?.path();
                if !generated_path(&path) {
                    if pending.len() + result.len() >= MAX_METADATA_ENTRIES {
                        return None;
                    }
                    pending.push((path, recursive, recursive));
                }
            }
        }
    }
    Some(result)
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
    // FSEvents reports the spelling supplied to its stream, while notify's
    // event filter compares canonical paths. Mixed /var and /private/var
    // registrations can therefore silently discard all notifications.
    let mut canonical = BTreeMap::new();
    for (path, recursive) in paths {
        let path = canonical_watch_path(&path).await;
        canonical
            .entry(path)
            .and_modify(|value| *value |= recursive)
            .or_insert(recursive);
    }
    Ok(canonical)
}

async fn canonical_watch_path(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_owned())
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
        } else if Some(path.as_path()) == state_db.parent() {
            // Creating/removing a WAL changes its parent directory's mtime.
            // The metadata probe samples that directory too; it is not a Git
            // edit unless the shared state directory is inside a checkout.
            metadata = true;
            git |= checkout_roots.iter().any(|root| path.starts_with(root));
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
        assert_eq!(invalidations(&["/state".into()], db, &[]), (false, true));
        assert_eq!(
            invalidations(&["/state".into()], db, &["/state".into()]),
            (true, true)
        );
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
    fn metadata_probe_is_bounded_to_watch_contract_and_skips_generated_trees() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/deep")).unwrap();
        std::fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(root.path().join(".git/refs/heads")).unwrap();
        std::fs::write(root.path().join("tracked.txt"), "a").unwrap();
        std::fs::write(root.path().join("src/deep/code.rs"), "a").unwrap();
        std::fs::write(root.path().join("node_modules/pkg/data"), "a").unwrap();
        std::fs::write(root.path().join(".git/refs/heads/main"), "a").unwrap();
        let paths = BTreeMap::from([
            (root.path().to_owned(), false),
            (root.path().join(".git/refs"), true),
        ]);
        let cancel = tokio_util::sync::CancellationToken::new();
        let first = watch_fingerprint(&paths, &cancel).unwrap();
        assert!(first.contains_key(&root.path().join("tracked.txt")));
        assert!(first.contains_key(&root.path().join(".git/refs/heads/main")));
        assert!(!first.contains_key(&root.path().join("src/deep")));
        assert!(!first.keys().any(|path| generated_path(path)));
        assert!(
            changed_paths(Some(&first), &watch_fingerprint(&paths, &cancel).unwrap()).is_empty()
        );
        std::fs::write(root.path().join("tracked.txt"), "changed").unwrap();
        let next = watch_fingerprint(&paths, &cancel).unwrap();
        assert_eq!(
            changed_paths(Some(&first), &next),
            vec![root.path().join("tracked.txt")]
        );
        cancel.cancel();
        assert!(watch_fingerprint(&paths, &cancel).is_none());
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
        edit_stream_regression(true).await;
    }

    #[tokio::test]
    async fn metadata_backstop_observes_edits_without_native_notifications() {
        edit_stream_regression(false).await;
    }

    async fn edit_stream_regression(notifications: bool) {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let paths = desired_paths(
            &fixture.ctx.config,
            &fixture.ctx.repository,
            &fixture.ctx.cancellation,
        )
        .await
        .unwrap();
        for path in paths.keys() {
            assert_eq!(
                path,
                &tokio::fs::canonicalize(path).await.unwrap(),
                "watch registration must use the same canonical spelling as notify's event filter"
            );
        }
        let scope = TaskScope::new();
        let git = wt_runtime::start_source(
            &scope,
            SourceSpec {
                name: "test-git",
                domain: Domain::Git,
                persistence: Persistence::Never("isolated test"),
                policy: RefreshPolicy::default(),
            },
            {
                let repository = fixture.ctx.repository.clone();
                move |cancel| {
                    let repository = repository.clone();
                    async move { repository.inventory_status(&cancel).await }
                }
            },
        );
        let (metadata, _metadata_publisher) = leaf_channel(
            &scope,
            SourceSpec {
                name: "test-metadata",
                domain: Domain::Metadata,
                persistence: Persistence::Never("isolated test"),
                policy: RefreshPolicy::default(),
            },
        );
        let (board, _board_publisher) = wt_runtime::derived_channel(&scope, "test-board");
        let edits = start_with_notifications(
            &scope,
            fixture.ctx.config.clone(),
            fixture.ctx.repository.clone(),
            board,
            git.view(),
            metadata.view(),
            notifications,
        );
        let worktree = tokio::fs::canonicalize(fixture.ctx.config.paths.worktree_root.join("one"))
            .await
            .unwrap();
        let mut git_updates = git.subscribe();
        let mut edit_updates = edits.subscribe();
        git.refresh();
        tokio::time::timeout(
            Duration::from_secs(5),
            git_updates.wait_for(|snapshot| {
                *snapshot.state() == SourceState::Ready && snapshot.data().is_some()
            }),
        )
        .await
        .expect("initial Git inventory should load")
        .expect("Git source should stay open");
        tokio::time::timeout(
            Duration::from_secs(5),
            edit_updates.wait_for(|snapshot| {
                *snapshot.state() == SourceState::Ready && snapshot.data().is_some()
            }),
        )
        .await
        .expect("freshness watcher registration should complete")
        .expect("edit source should stay open");
        let initial_revision = git_updates.borrow().revision();

        // This receiver is optional: with automations disabled nobody observes
        // edit timestamps. The Git and metadata invalidation watcher must live on.
        drop(edits);
        let tracked_file = worktree.join("tracked.txt");
        tokio::fs::write(&tracked_file, "first external edit\n")
            .await
            .unwrap();
        let first_revision = tokio::time::timeout(
            Duration::from_secs(5),
            git_updates.wait_for(|snapshot| {
                snapshot.revision() > initial_revision
                    && snapshot.data().is_some_and(|rows| {
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
        .revision();

        // Require a different observable state, not merely another revision
        // from an in-flight follow-up scan of the first edit.
        tokio::fs::write(&tracked_file, "base\n").await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            git_updates.wait_for(|snapshot| {
                snapshot.revision() > first_revision
                    && snapshot.data().is_some_and(|rows| {
                        rows.iter().any(|row| {
                            row.worktree.target.path == worktree
                                && row.status.as_ref().is_some_and(|status| !status.dirty)
                        })
                    })
            }),
        )
        .await
        .expect("the filesystem watcher should remain alive for later edits")
        .expect("Git source should stay open");

        if !notifications {
            // A later edit must be detected after the first fingerprint, too.
            let clean_revision = git_updates.borrow().revision();
            tokio::fs::write(&tracked_file, "another edit\n")
                .await
                .unwrap();
            tokio::time::timeout(
                Duration::from_secs(5),
                git_updates.wait_for(|snapshot| {
                    snapshot.revision() > clean_revision
                        && snapshot.data().is_some_and(|rows| {
                            rows.iter().any(|row| {
                                row.worktree.target.path == worktree
                                    && row.status.as_ref().is_some_and(|status| status.dirty)
                            })
                        })
                }),
            )
            .await
            .expect("later metadata samples must detect edits without native events")
            .expect("Git source should stay open");
        }
        scope.shutdown(Duration::from_secs(2)).await.unwrap();
        fixture.close().await.unwrap();
    }
}
