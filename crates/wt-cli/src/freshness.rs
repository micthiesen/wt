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

use anyhow::Result;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use serde_json::Value;
use wt_app_core::watcher::{self, WtWatcher};
use wt_config::Config;
use wt_core::Domain;
use wt_runtime::{
    Persistence, RefreshPolicy, SourceHandle, SourceSpec, SourceState, Stamp, TaskScope,
    leaf_channel,
};
use wt_tui::Board;
use wt_vcs::{WorktreeRecord, WorktreeSnapshot};

const BACKSTOP: Duration = Duration::from_secs(60);
const METADATA_BACKSTOP: Duration = Duration::from_secs(1);
const MAX_METADATA_ENTRIES: usize = 8192;
/// A deleted watch root makes the poller report it missing on every poll.
/// Those reports re-read the watch list at most this often.
const VANISHED_INTERVAL: Duration = Duration::from_secs(5);

pub fn start(
    scope: &TaskScope,
    config: Arc<Config>,
    source: SourceHandle<Board>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<crate::local_source::Metadata>,
) -> SourceHandle<BTreeMap<String, i64>> {
    start_with_notifications(scope, config, source, git, metadata, true, None)
}

/// Test seam: the watch set the backend actually holds.
type WatchProbe = Arc<Mutex<BTreeMap<PathBuf, bool>>>;

fn start_with_notifications(
    scope: &TaskScope,
    config: Arc<Config>,
    source: SourceHandle<Board>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<crate::local_source::Metadata>,
    notifications: bool,
    probe: Option<WatchProbe>,
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
    // Native notifications can arrive after the metadata backstop already
    // observed and invalidated an edit. Share that witness across both lanes.
    let observed_files = Arc::new(RwLock::new(None::<Arc<WatchFingerprint>>));
    // A watched checkout disappeared; re-read the watch list.
    let rewatch = Arc::new(tokio::sync::Notify::new());
    let vanished_gate = Arc::new(RateGate::new(VANISHED_INTERVAL));
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
        let callback_observed_files = observed_files.clone();
        let callback_rewatch = rewatch.clone();
        let callback_vanished = vanished_gate.clone();
        let mut watcher = tokio::task::spawn_blocking(move || {
            watcher::new(move |event: notify::Result<Event>| match event {
                Ok(event) if relevant(&event) => {
                    let paths = if event.need_rescan() {
                        event.paths.clone()
                    } else {
                        unobserved_paths(&event.paths, &callback_observed_files)
                    };
                    let (git, metadata) = if !event.paths.is_empty() && paths.is_empty() {
                        (false, false)
                    } else {
                        invalidations(&paths, &state_db, &checkout_roots)
                    };
                    tracing::debug!(?event, ?paths, git, metadata, "filesystem notification invalidation");
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
                Err(error) if vanished(&error) => {
                    // Expected while a checkout is removed. The poller repeats
                    // this every interval until the path is unwatched, so one
                    // refresh and watch-list re-read covers the whole burst.
                    if callback_vanished.allow() {
                        tracing::debug!(%error, paths = ?error.paths, "watched path vanished; re-reading watch list");
                        observed_git.refresh();
                        observed_metadata.refresh();
                        callback_rewatch.notify_one();
                    }
                }
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
                    wt_app_core::watcher_cleanup::drop_watcher(watcher).await;
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
        // The watch list derives from the Git source's inventory, minus
        // checkouts with an accepted removal. No separate inventory scan.
        let mut git_updates = git.subscribe();
        let mut metadata_updates = metadata.subscribe();
        git_updates.mark_changed();
        metadata_updates.mark_changed();
        let mut inputs = Vec::<WorktreeRecord>::new();
        // Some listed checkout was missing at the last reconcile; any later
        // inventory may be the one that shows it again.
        let mut inputs_missing = false;
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
                        wt_app_core::watcher_cleanup::drop_watcher(active).await;
                        break;
                    }
                    desired = desired_paths(&config, &inputs) => desired,
                };
                match desired {
                    Ok((paths, missing)) => {
                        inputs_missing = missing;
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
                                    wt_app_core::watcher_cleanup::drop_watcher(watcher).await;
                                }
                                break;
                            }
                            update = &mut update => update,
                        };
                        match update {
                            Ok((active, paths, cancelled)) => {
                                if cancelled {
                                    wt_app_core::watcher_cleanup::drop_watcher(active).await;
                                    break;
                                }
                                let watch_set_changed = paths != previous_registered;
                                watcher = Some(active);
                                registered = paths;
                                if let Some(probe) = &probe && let Ok(mut probe) = probe.lock() {
                                    *probe = registered.clone();
                                }
                                if watch_set_changed {
                                    tracing::debug!("watch registration invalidates Git");
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
                        if let Ok(mut roots) = edit_roots.write() {
                            *roots = board.rows.iter()
                                .map(|row| (PathBuf::from(&row.path), row.key.clone()))
                                .collect();
                        }
                        let live_keys: BTreeSet<_> = board.rows.iter().map(|row| row.key.as_str()).collect();
                        latest_edits.retain(|slug, _| live_keys.contains(slug.as_str()));
                    }
                }
                changed = git_updates.changed() => {
                    if changed.is_err() { break; }
                    let snapshot = git_updates.borrow_and_update().clone();
                    let next = watch_inputs(&snapshot, &metadata.snapshot());
                    reconcile |= next != inputs || inputs_missing;
                    inputs = next;
                }
                changed = metadata_updates.changed() => {
                    if changed.is_err() { break; }
                    let snapshot = metadata_updates.borrow_and_update().clone();
                    let next = watch_inputs(&git.snapshot(), &snapshot);
                    reconcile |= next != inputs;
                    inputs = next;
                }
                _ = rewatch.notified() => { reconcile = true; }
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
                        let next = Arc::new(next);
                        // Refresh on the first sample too: an edit may occur
                        // between watcher registration and this baseline.
                        let changed = changed_paths(fingerprint.as_deref(), &next);
                        if !changed.is_empty() {
                            let (git_changed, metadata_changed) = invalidations(&changed, &probe_state_db, &probe_checkout_roots);
                            tracing::debug!(?changed, git_changed, metadata_changed, "filesystem metadata invalidation");
                            if git_changed { git.refresh(); }
                            if metadata_changed { metadata.refresh(); }
                        }
                        if let Ok(mut observed) = observed_files.write() {
                            *observed = Some(next.clone());
                        }
                        fingerprint = Some(next);
                    }
                }
                _ = backstop.tick() => { git.refresh(); metadata.refresh(); reconcile = true; }
            }
        }
        wt_app_core::watcher_cleanup::drop_watcher(watcher).await;
    });
    edits
}

/// A bounded notification-loss backstop, not a recursive checkout scan. The
/// watch contract includes immediate checkout children and recursive Git
/// metadata only. Symlinks and generated dependency trees are never followed.
type WatchFingerprint = BTreeMap<PathBuf, FileSignature>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileSignature {
    len: u64,
    modified: Option<SystemTime>,
    is_file: bool,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl FileSignature {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            is_file: metadata.is_file(),
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

fn unobserved_paths(
    paths: &[PathBuf],
    observed: &RwLock<Option<Arc<WatchFingerprint>>>,
) -> Vec<PathBuf> {
    let Ok(observed) = observed.read() else {
        return paths.to_vec();
    };
    let Some(observed) = observed.as_ref() else {
        return paths.to_vec();
    };
    paths
        .iter()
        .filter(|path| {
            let Some(prior) = observed.get(*path) else {
                return true;
            };
            // Directory metadata cannot prove its descendants unchanged. Missing,
            // renamed, symlink and unknown paths retain conservative invalidation.
            !prior.is_file
                || !std::fs::symlink_metadata(path)
                    .is_ok_and(|current| FileSignature::from_metadata(&current) == *prior)
        })
        .cloned()
        .collect()
}

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
        result.insert(path.clone(), FileSignature::from_metadata(&metadata));
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

/// Checkouts to watch: the Git source's inventory without checkouts that an
/// accepted removal owns. Their files are about to disappear; watching them
/// only produces missing-path errors.
fn watch_inputs(
    git: &wt_runtime::SourceSnapshot<Vec<WorktreeSnapshot>>,
    metadata: &wt_runtime::SourceSnapshot<crate::local_source::Metadata>,
) -> Vec<WorktreeRecord> {
    let state = metadata.data().map(|metadata| &metadata.0);
    git.data()
        .into_iter()
        .flat_map(|rows| rows.iter())
        .map(|row| &row.worktree)
        .filter(|record| !state.is_some_and(|state| removing(state, record)))
        .cloned()
        .collect()
}

fn removing(state: &Value, record: &WorktreeRecord) -> bool {
    state["removals"][wt_core::worktree_target_key(&record.target)]["path"].as_str()
        == Some(record.target.path.as_str())
}

/// The second value reports a listed checkout that no longer exists.
async fn desired_paths(
    config: &Config,
    inventory: &[WorktreeRecord],
) -> Result<(BTreeMap<PathBuf, bool>, bool)> {
    let mut paths = BTreeMap::new();
    let mut missing = false;
    paths.insert(config.paths.worktree_root.clone(), false);
    paths.insert(config.paths.main_clone.clone(), false);
    if let Some(parent) = config.paths.state_db.parent() {
        paths.insert(parent.to_owned(), false);
    }
    let common_dirs: BTreeSet<_> = inventory
        .iter()
        .filter_map(|worktree| worktree.common_dir.clone())
        .collect();
    for common_dir in &common_dirs {
        paths.insert(common_dir.clone(), false);
        for child in ["refs", "worktrees"] {
            let path = common_dir.join(child);
            if path_may_exist(&path).await {
                paths.insert(path, true);
            }
        }
    }
    for worktree in inventory {
        let checkout = PathBuf::from(&worktree.target.path);
        // A deleted checkout cannot be watched; requesting it would only
        // repeat the backend's missing-path error.
        if !path_may_exist(&checkout).await {
            missing = true;
            continue;
        }
        paths.insert(checkout, false);
        if let Some(directory) = &worktree.git_dir
            && !common_dirs
                .iter()
                .any(|common| directory.starts_with(common.join("worktrees")))
            && path_may_exist(directory).await
        {
            paths.insert(directory.clone(), false);
            for child in ["refs", "worktrees"] {
                let path = directory.join(child);
                if path_may_exist(&path).await {
                    paths.insert(path, true);
                }
            }
        }
    }
    // Canonicalize watch paths so aliases such as /var and /private/var do not
    // create duplicate watches or miss events during path matching.
    let mut canonical = BTreeMap::new();
    for (path, recursive) in paths {
        let path = canonical_watch_path(&path).await;
        canonical
            .entry(path)
            .and_modify(|value| *value |= recursive)
            .or_insert(recursive);
    }
    Ok((canonical, missing))
}

/// A watched path that no longer exists. Other watcher errors are genuine
/// notification loss and keep their full refresh.
fn vanished(error: &notify::Error) -> bool {
    !error.paths.is_empty()
        && match &error.kind {
            notify::ErrorKind::PathNotFound => true,
            notify::ErrorKind::Io(io) => io.kind() == std::io::ErrorKind::NotFound,
            _ => false,
        }
}

/// Coalesces a repeating signal to at most one per interval.
struct RateGate {
    interval: Duration,
    last: Mutex<Option<std::time::Instant>>,
}

impl RateGate {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(None),
        }
    }

    fn allow(&self) -> bool {
        let Ok(mut last) = self.last.lock() else {
            return true;
        };
        let now = std::time::Instant::now();
        if last.is_some_and(|last| now.duration_since(last) < self.interval) {
            return false;
        }
        *last = Some(now);
        true
    }
}

async fn canonical_watch_path(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_owned())
}

fn update_watches(
    mut watcher: WtWatcher,
    mut registered: BTreeMap<PathBuf, bool>,
    desired: BTreeMap<PathBuf, bool>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> (WtWatcher, BTreeMap<PathBuf, bool>, bool) {
    if registered == desired || cancellation.is_cancelled() {
        return (watcher, registered, cancellation.is_cancelled());
    }
    // Apply the whole inventory in one transaction so backends that rebuild
    // their polling snapshots do not leave gaps between individual edits.
    let started = std::time::Instant::now();
    let mut changes = Vec::new();
    for (path, recursive) in &registered {
        if desired.get(path) != Some(recursive) {
            changes.push(notify::PathOp::unwatch(path.clone()));
        }
    }
    for (path, recursive) in &desired {
        if registered.get(path) != Some(recursive) {
            changes.push(if *recursive {
                notify::PathOp::watch_recursive(path.clone())
            } else {
                notify::PathOp::watch_non_recursive(path.clone())
            });
        }
    }
    if cancellation.is_cancelled() {
        return (watcher, registered, true);
    }
    match watcher.update_paths(changes) {
        Ok(()) => {
            registered = actual_watches(&watcher);
            if registered != desired {
                tracing::debug!(
                    desired = desired.len(),
                    registered = registered.len(),
                    "watch backend accepted an incomplete inventory; reconciliation will retry"
                );
            }
        }
        Err(error) => {
            tracing::warn!(%error, "cannot apply filesystem watches; reconciliation will retry");
            // A batch may apply only its prefix. Never record unattempted
            // operations as successful, or forget watches that still exist.
            registered = actual_watches(&watcher);
        }
    }
    tracing::debug!(
        paths = registered.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "filesystem watches registered"
    );
    (watcher, registered, false)
}

fn actual_watches(watcher: &WtWatcher) -> BTreeMap<PathBuf, bool> {
    watcher
        .watched_paths()
        .map(|paths| {
            paths
                .into_iter()
                .map(|(path, mode)| (path, mode == RecursiveMode::Recursive))
                .collect()
        })
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot inspect filesystem watches; reconciliation will retry");
            BTreeMap::new()
        })
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

/// Only a confirmed NotFound drops a watch. An unreadable path stays in the
/// set so the watcher reports it, rather than failing the whole rebuild.
async fn path_may_exist(path: &std::path::Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "subprocess helper for the SQLite watcher lock test"]
    fn sqlite_writer_probe() {
        let Some(path) = std::env::var_os("WT_WATCHER_SQLITE_PROBE") else {
            return;
        };
        let db = rusqlite::Connection::open(path).unwrap();
        db.busy_timeout(Duration::ZERO).unwrap();
        let error = db.execute_batch("BEGIN IMMEDIATE;").unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn shallow_directory_link_changes_preserve_sqlite_writer_locks() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let path = root.join("state.sqlite");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE fixture(value); BEGIN IMMEDIATE;")
            .unwrap();
        let assert_locked = || {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "freshness::tests::sqlite_writer_probe",
                    "--ignored",
                    "--nocapture",
                ])
                .env("WT_WATCHER_SQLITE_PROBE", &path)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "watcher released another connection's SQLite lock: {}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        };
        assert_locked();
        let (send, receive) = std::sync::mpsc::channel();
        let mut watcher = watcher::new(send).unwrap();
        watcher.watch(&root, RecursiveMode::NonRecursive).unwrap();
        // Directory links emit NOTE_LINK on macOS. Rebuilding this shallow
        // watch must never adopt DB/SHM files: closing an unrelated SHM fd
        // releases SQLite's process-wide POSIX locks, risking SIGBUS/corruption.
        for index in 0..3 {
            // PollWatcher intentionally keeps directory timestamps at second
            // precision. Put each link-count change in a distinct timestamp
            // bucket so the assertion does not depend on scheduler timing.
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
            std::thread::sleep(
                Duration::from_secs(1) - Duration::from_nanos(u64::from(now.subsec_nanos()))
                    + Duration::from_millis(25),
            );
            std::fs::create_dir(root.join(format!("child-{index}"))).unwrap();
            receive
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            // The inventory request is processed after the event's watch update.
            watcher.watched_paths().unwrap();
            while receive.try_recv().is_ok() {}
        }
        assert_locked();
        drop(watcher);
        assert_locked();
        db.execute_batch("ROLLBACK;").unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_metadata_only_polling() {
        assert_eq!(WtWatcher::kind(), notify::WatcherKind::PollWatcher);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn nonrecursive_checkout_watch_never_adopts_existing_generated_subtree() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let generated = root.join("node_modules/package/deep");
        std::fs::create_dir_all(&generated).unwrap();
        let deep_file = generated.join("file.js");
        std::fs::write(&deep_file, "before").unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let mut watcher = watcher::new(send).unwrap();
        watcher.watch(&root, RecursiveMode::NonRecursive).unwrap();
        // Wait for each shallow metadata poll before writing deep in the tree.
        // A nonrecursive checkout watch must not adopt generated descendants.
        for index in 0..2 {
            std::fs::write(root.join(format!("new-{index}")), "new").unwrap();
            receive
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
        }
        while receive.try_recv().is_ok() {}
        std::fs::write(&deep_file, "after").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match receive.recv_timeout(remaining) {
                Ok(Ok(event)) => assert!(
                    event.paths.iter().all(|path| !path.starts_with(&generated)),
                    "a shallow checkout watch must not open/watch generated descendants: {event:?}"
                ),
                Ok(Err(error)) => panic!("unexpected watcher error: {error}"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        drop(watcher);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn partial_watch_batch_tracks_only_registered_paths_and_reconciles() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let first = root.join("a-existing");
        let missing = root.join("b-missing");
        let last = root.join("c-existing");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&last).unwrap();
        let watcher = watcher::new(|_: notify::Result<Event>| {}).unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let desired = BTreeMap::from([
            (first.clone(), false),
            (missing.clone(), false),
            (last.clone(), false),
        ]);
        let (watcher, registered, cancelled) =
            update_watches(watcher, BTreeMap::new(), desired, &cancel);
        assert!(!cancelled);
        assert_eq!(registered, BTreeMap::from([(first.clone(), false)]));
        std::fs::create_dir(&missing).unwrap();
        let desired = BTreeMap::from([(missing.clone(), false), (last.clone(), true)]);
        let (watcher, registered, cancelled) =
            update_watches(watcher, registered, desired.clone(), &cancel);
        assert!(!cancelled);
        assert_eq!(registered, desired);
        cancel.cancel();
        let (watcher, registered, cancelled) =
            update_watches(watcher, registered, BTreeMap::new(), &cancel);
        assert!(cancelled);
        assert_eq!(registered, desired);
        drop(watcher);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn polling_watch_batch_tracks_only_existing_paths_and_reconciles() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let first = root.join("a-existing");
        let missing = root.join("b-missing");
        let last = root.join("c-existing");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&last).unwrap();
        let watcher = watcher::new(|_: notify::Result<Event>| {}).unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let desired = BTreeMap::from([
            (first.clone(), false),
            (missing.clone(), false),
            (last.clone(), false),
        ]);
        let (watcher, registered, cancelled) =
            update_watches(watcher, BTreeMap::new(), desired, &cancel);
        assert!(!cancelled);
        assert_eq!(
            registered,
            BTreeMap::from([(first.clone(), false), (last.clone(), false)])
        );
        std::fs::create_dir(&missing).unwrap();
        let desired = BTreeMap::from([(missing.clone(), false), (last.clone(), true)]);
        let (watcher, registered, cancelled) =
            update_watches(watcher, registered, desired.clone(), &cancel);
        assert!(!cancelled);
        assert_eq!(registered, desired);
        cancel.cancel();
        let (watcher, registered, cancelled) =
            update_watches(watcher, registered, BTreeMap::new(), &cancel);
        assert!(cancelled);
        assert_eq!(registered, desired);
        drop(watcher);
    }

    #[tokio::test]
    async fn rapid_native_watch_reconfiguration_and_drop_stays_bounded() {
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..4 {
            tasks.spawn_blocking(|| {
                let root = tempfile::tempdir().unwrap();
                let root = std::fs::canonicalize(root.path()).unwrap();
                for _ in 0..2 {
                    let mut watcher = watcher::new(|_: notify::Result<Event>| {}).unwrap();
                    watcher
                        .update_paths(vec![notify::PathOp::watch_non_recursive(root.clone())])
                        .unwrap();
                    watcher
                        .update_paths(vec![notify::PathOp::unwatch(root.clone())])
                        .unwrap();
                    watcher
                        .update_paths(vec![notify::PathOp::watch_non_recursive(root.clone())])
                        .unwrap();
                    drop(watcher);
                }
            });
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(result) = tasks.join_next().await {
                result.unwrap();
            }
        })
        .await
        .expect("rapid native watcher replacement and teardown must drain");
    }

    #[test]
    fn delayed_native_events_do_not_repeat_an_observed_file_invalidation() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("tracked.txt");
        std::fs::write(&file, "old").unwrap();
        let paths = BTreeMap::from([(root.path().to_owned(), false)]);
        let cancel = tokio_util::sync::CancellationToken::new();
        let before = watch_fingerprint(&paths, &cancel).unwrap();
        std::fs::write(&file, "new").unwrap();
        let after = watch_fingerprint(&paths, &cancel).unwrap();
        assert_eq!(
            changed_paths(Some(&before), &after),
            std::slice::from_ref(&file)
        );
        let observed = RwLock::new(Some(Arc::new(after)));
        assert!(unobserved_paths(std::slice::from_ref(&file), &observed).is_empty());
        std::fs::write(&file, "newer").unwrap();
        assert_eq!(
            unobserved_paths(std::slice::from_ref(&file), &observed),
            std::slice::from_ref(&file)
        );

        let after = watch_fingerprint(&paths, &cancel).unwrap();
        *observed.write().unwrap() = Some(Arc::new(after));
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, "newer").unwrap();
        std::fs::rename(&replacement, &file).unwrap();
        assert_eq!(
            unobserved_paths(std::slice::from_ref(&file), &observed),
            std::slice::from_ref(&file)
        );
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            unobserved_paths(std::slice::from_ref(&file), &observed),
            [file]
        );
        let directory = root.path().to_owned();
        assert_eq!(
            unobserved_paths(std::slice::from_ref(&directory), &observed),
            [directory]
        );
        let unknown = root.path().join("deep/new.txt");
        assert_eq!(
            unobserved_paths(std::slice::from_ref(&unknown), &observed),
            [unknown]
        );
        assert!(unobserved_paths(&[], &observed).is_empty());
    }

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
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let inventory = fixture
            .ctx
            .repository
            .inventory(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let (paths, missing) = desired_paths(&fixture.ctx.config, &inventory)
            .await
            .unwrap();
        assert!(!missing);
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
            board,
            git.view(),
            metadata.view(),
            notifications,
            None,
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
        // Match the production source grace. Native teardown used to block
        // this executor, preventing the old two-second timer from firing;
        // joined blocking-pool cleanup now makes that deadline real.
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[test]
    fn only_missing_path_errors_are_coalesced() {
        let missing = notify::Error::io(std::io::Error::from(std::io::ErrorKind::NotFound))
            .add_path(PathBuf::from("/gone"));
        assert!(vanished(&missing));
        assert!(vanished(
            &notify::Error::path_not_found().add_path(PathBuf::from("/gone"))
        ));
        // Without a path, or for any other failure, events may truly be lost.
        assert!(!vanished(&notify::Error::io(std::io::Error::from(
            std::io::ErrorKind::NotFound
        ))));
        assert!(!vanished(
            &notify::Error::io(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                .add_path(PathBuf::from("/denied"))
        ));
        assert!(!vanished(&notify::Error::generic(
            "poll thread disconnected"
        )));
        let gate = RateGate::new(Duration::from_secs(60));
        assert!(gate.allow());
        assert!(!gate.allow());
        assert!(RateGate::new(Duration::ZERO).allow());
    }

    #[tokio::test]
    async fn removing_and_deleted_checkouts_leave_the_watch_set_with_bounded_refreshes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let fetches = Arc::new(AtomicUsize::new(0));
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
                let fetches = fetches.clone();
                move |cancel| {
                    let repository = repository.clone();
                    fetches.fetch_add(1, Ordering::SeqCst);
                    async move { repository.inventory_status(&cancel).await }
                }
            },
        );
        let (metadata, metadata_publisher) = leaf_channel(
            &scope,
            SourceSpec {
                name: "test-metadata",
                domain: Domain::Metadata,
                persistence: Persistence::Never("isolated test"),
                policy: RefreshPolicy::default(),
            },
        );
        metadata_publisher.publish_live((serde_json::json!({}), BTreeSet::new()), Stamp::now());
        let (board, _board_publisher) = wt_runtime::derived_channel(&scope, "test-board");
        let probe = WatchProbe::default();
        let _edits = start_with_notifications(
            &scope,
            fixture.ctx.config.clone(),
            board,
            git.view(),
            metadata.view(),
            true,
            Some(probe.clone()),
        );
        let root = &fixture.ctx.config.paths.worktree_root;
        let one = tokio::fs::canonicalize(root.join("one")).await.unwrap();
        let two = tokio::fs::canonicalize(root.join("two")).await.unwrap();
        let watched = |path: &Path| probe.lock().unwrap().contains_key(path);
        git.refresh();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !(watched(&one) && watched(&two)) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "checkouts were never watched"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // An accepted removal drops its checkout before any file disappears.
        metadata_publisher.publish_live(
            (
                serde_json::json!({"removals": {"one": {
                    "path": one.to_string_lossy(), "branch": "feature/one", "error": null
                }}}),
                BTreeSet::new(),
            ),
            Stamp::now(),
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while watched(&one) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "removing checkout stayed watched"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(watched(&two));

        // A checkout deleted without a removal record is unwatched promptly,
        // and the missing-path reports stop refreshing Git every poll.
        tokio::fs::remove_dir_all(&two).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while watched(&two) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "deleted checkout stayed watched"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // Let follow-up scans (index stat refreshes after `git status`) settle.
        // Before the fix, each one-second poll's missing-path error refreshed
        // Git, i.e. at least four refreshes in this window.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let settled = fetches.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(4)).await;
        let later = fetches.load(Ordering::SeqCst) - settled;
        assert!(
            later <= 2,
            "{later} Git refreshes in 4s after the deleted checkout was unwatched"
        );
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
        fixture.close().await.unwrap();
    }
}
