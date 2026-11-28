//! Filesystem-driven invalidation with a low-frequency reconciliation backstop.
//! Watch Git metadata and checkout roots, not generated dependency trees.
//! Edit timestamps therefore cover filesystem events at those watched roots;
//! this does not recursively walk worktree contents.
//!
//! A change is routed to the checkouts it can affect, so one commit re-reads
//! one row and not the fleet. Where the platform reports changes itself, its
//! notifications drive this and a metadata scan of the same paths catches
//! lost ones. On macOS the scan is the only detector: its native watcher is
//! unsafe next to SQLite (see `wt_app_core::watcher`), and a second poller
//! over the same tree would only double the cost.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use wt_app_core::watcher::{self, WtWatcher};
use wt_config::Config;
use wt_core::Domain;
use wt_runtime::{
    Persistence, RefreshPolicy, Request, SourceHandle, SourceSpec, SourceState, Stamp, TaskScope,
    leaf_channel,
};
use wt_tui::Board;
use wt_vcs::{WorktreeRecord, WorktreeSnapshot};

const BACKSTOP: Duration = Duration::from_secs(60);
/// Spacing of metadata scans, from the start of the preceding one. Where the
/// scan is the only detector it runs twice a second: the same number of walks
/// as the poller and the backstop it replaces made between them, for half
/// the wait.
const SCAN_INTERVAL: Duration = Duration::from_millis(if POLLING { 500 } else { 1000 });
/// A scan slower than `SCAN_INTERVAL / SCAN_DUTY` stretches the spacing to
/// this many times its own duration, so a large watch set is sampled less
/// often rather than abandoned, and scanning never takes more than one part
/// in this many of a thread.
const SCAN_DUTY: u32 = 4;
/// How often the files other processes signal through are checked where
/// changes are only found by polling.
const SIGNAL_TICK: Duration = Duration::from_millis(75);
/// A deleted watch root makes the poller report it missing on every poll.
/// Those reports re-read the watch list at most this often.
const VANISHED_INTERVAL: Duration = Duration::from_secs(5);
/// How long a Git listing waits for its checkouts to be watched. Past this
/// it proceeds, and the periodic sweep covers what a watch would have.
const COVER_WAIT: Duration = Duration::from_secs(2);

/// The least spacing between metadata reads caused by other processes'
/// writes to the state database. Forty agents writing status would
/// otherwise have every wt read its state, and project its board again, at
/// the tick's own rate. A lone write is still read at once; a read wt's own
/// write asks for never goes through this.
const METADATA_PACE: Duration = Duration::from_millis(250);

/// Whether ref names differ only by case name the same file here.
const REFS_FOLD_CASE: bool = cfg!(any(target_os = "macos", windows));

/// Whether filesystem changes are found only by polling on this platform.
pub(crate) const POLLING: bool = cfg!(target_os = "macos");

/// Asks the watcher to cover checkouts before they are first read.
#[derive(Clone)]
pub struct Coverage {
    requests: mpsc::Sender<CoverRequest>,
    /// Checkout paths already watched, so the usual listing asks nothing.
    covered: Arc<RwLock<BTreeSet<String>>>,
}

pub struct CoverageRequests {
    requests: mpsc::Receiver<CoverRequest>,
    covered: Arc<RwLock<BTreeSet<String>>>,
}

struct CoverRequest {
    records: Vec<WorktreeRecord>,
    done: oneshot::Sender<()>,
}

pub fn coverage() -> (Coverage, CoverageRequests) {
    let (requests, receiver) = mpsc::channel(4);
    let covered = Arc::new(RwLock::new(BTreeSet::new()));
    (
        Coverage {
            requests,
            covered: covered.clone(),
        },
        CoverageRequests {
            requests: receiver,
            covered,
        },
    )
}

impl Coverage {
    /// Resolve once every listed checkout is watched and has a baseline to
    /// compare later samples against. A read that starts after this cannot
    /// miss a change: whatever happens next differs from the baseline.
    pub async fn cover(&self, records: &[WorktreeRecord]) {
        let known = self.covered.read().is_ok_and(|covered| {
            records
                .iter()
                .all(|record| covered.contains(&record.target.path))
        });
        if known {
            return;
        }
        let (done, covered) = oneshot::channel();
        let request = CoverRequest {
            records: records.to_vec(),
            done,
        };
        // A watcher that is gone or stuck must not hold the listing.
        let _ = tokio::time::timeout(COVER_WAIT, async {
            if self.requests.send(request).await.is_ok() {
                let _ = covered.await;
            }
        })
        .await;
    }
}

pub fn start(
    scope: &TaskScope,
    config: Arc<Config>,
    source: SourceHandle<Board>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<crate::local_source::Metadata>,
    coverage: CoverageRequests,
) -> SourceHandle<BTreeMap<String, i64>> {
    start_with_notifications(
        scope, config, source, git, metadata, coverage, !POLLING, None,
    )
}

/// Test seam: the watch set the backend actually holds.
type WatchProbe = Arc<Mutex<BTreeMap<PathBuf, bool>>>;

/// What the files another process changes look like, as last seen.
type Seen = Arc<Mutex<BTreeMap<PathBuf, Option<FileSignature>>>>;

/// Call `changed` soon after any of `files` changes, for the few files
/// other processes signal through (the state database, the events daemon's
/// snapshot). A full scan of the watch set finds the same change up to a
/// second later; this only shortens the wait, on a thread of its own so it
/// never queues behind blocking work. It reads nothing but file metadata.
pub(crate) fn watch_signal_files(
    scope: &TaskScope,
    files: Vec<PathBuf>,
    changed: impl Fn() + Send + 'static,
) {
    signal_tick(scope, files, changed);
}

/// `watch_signal_files`, also returning what the tick has seen so a scan
/// does not report the same change a second time.
fn signal_tick(
    scope: &TaskScope,
    files: Vec<PathBuf>,
    changed: impl Fn() + Send + 'static,
) -> Seen {
    let seen = Seen::default();
    let cancellation = scope.token();
    let (stopped, joined) = oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("wt-signal-tick".into())
        .spawn({
            let seen = seen.clone();
            let cancellation = cancellation.clone();
            move || {
                let _stopped = stopped;
                // The WAL comes and goes; name it through its directory.
                let files: Vec<PathBuf> = files
                    .into_iter()
                    .map(|file| match (file.parent(), file.file_name()) {
                        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
                            .map_or_else(|_| file.clone(), |parent| parent.join(name)),
                        _ => file,
                    })
                    .collect();
                let mut first = true;
                while !cancellation.is_cancelled() {
                    let mut moved = false;
                    if let Ok(mut seen) = seen.lock() {
                        for file in &files {
                            let now = std::fs::symlink_metadata(file)
                                .ok()
                                .map(|metadata| FileSignature::from_metadata(&metadata));
                            if seen.get(file) != Some(&now) {
                                seen.insert(file.clone(), now);
                                moved = true;
                            }
                        }
                    }
                    if moved && !std::mem::take(&mut first) {
                        changed();
                    }
                    first = false;
                    std::thread::sleep(SIGNAL_TICK);
                }
            }
        });
    match thread {
        // Shutdown waits for the thread, which notices cancellation within
        // one tick.
        Ok(_) => {
            scope.spawn(async move {
                cancellation.cancelled().await;
                let _ = joined.await;
            });
        }
        Err(error) => tracing::warn!(%error, "cannot start the signal-file tick; using the scan"),
    }
    seen
}

/// One checkout as the router sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LayoutRow {
    key: String,
    path: PathBuf,
    git_dir: Option<PathBuf>,
    common_dir: Option<PathBuf>,
    branch: String,
    upstream: Option<String>,
}

/// Which checkouts a set of changed paths can affect.
#[derive(Debug, PartialEq, Eq)]
enum Touch {
    /// A path whose reach is not known: read every row.
    All,
    /// Read these rows and list the checkouts again. An empty set is a
    /// change to the checkout set or to something no row's status reads.
    Rows(BTreeSet<String>),
}

#[derive(Debug, Default)]
struct Layout {
    rows: Vec<LayoutRow>,
}

impl Layout {
    fn new(rows: &[WorktreeSnapshot]) -> Self {
        let keys = crate::local_source::git_keys(rows.iter().map(|row| &row.worktree));
        Self {
            rows: rows
                .iter()
                .zip(keys)
                .map(|(row, key)| LayoutRow {
                    key,
                    path: PathBuf::from(&row.worktree.target.path),
                    git_dir: row.worktree.git_dir.clone(),
                    common_dir: row.worktree.common_dir.clone(),
                    branch: row.worktree.target.branch.clone(),
                    upstream: row
                        .status
                        .as_ref()
                        .and_then(|status| status.upstream.clone()),
                })
                .collect(),
        }
    }

    /// Route Git-relevant paths to rows. Anything that cannot be placed
    /// reads everything: a wrong guess here would leave a row stale until
    /// the next sweep.
    fn touch(&self, paths: &[PathBuf], worktree_root: &Path) -> Touch {
        if paths.is_empty() {
            return Touch::All;
        }
        let mut keys = BTreeSet::new();
        for path in paths {
            let common = self
                .rows
                .iter()
                .filter_map(|row| row.common_dir.as_deref())
                .filter(|common| path.starts_with(common))
                .max_by_key(|common| common.components().count());
            if let Some(common) = common {
                let owner = |directory: &Path| {
                    self.rows
                        .iter()
                        .find(|row| row.git_dir.as_deref() == Some(directory))
                        .map(|row| row.key.clone())
                };
                let relative = path.strip_prefix(common).unwrap_or(path);
                let mut parts = relative.components().map(|part| part.as_os_str());
                match parts.next().and_then(|part| part.to_str()) {
                    // The directory itself changes with every lock file made
                    // in it, and the object store with every commit. What a
                    // status reads there (HEAD, the index, operation state)
                    // is reported as its own path.
                    None | Some("objects") => {}
                    // A linked checkout's own administration directory. One
                    // that is not listed is a checkout arriving or leaving.
                    Some("worktrees") => {
                        if let Some(name) = parts.next() {
                            keys.extend(owner(&common.join("worktrees").join(name)));
                        }
                    }
                    // A ref reaches the checkouts on that branch or tracking
                    // it. Any other ref moves no status, but facts derived
                    // from refs wake on the listing it asks for.
                    Some("refs") => {
                        let kind = parts.next().and_then(|part| part.to_str());
                        let name = parts
                            .filter_map(|part| part.to_str())
                            .collect::<Vec<_>>()
                            .join("/");
                        // The path is spelled as the file is, which on a
                        // case-folding filesystem need not be how the branch
                        // is.
                        let fold = |text: &str| {
                            if REFS_FOLD_CASE {
                                text.to_lowercase()
                            } else {
                                text.to_owned()
                            }
                        };
                        let name = fold(&name);
                        // A directory under refs changes with every lock
                        // file made in it; the refs that really moved are
                        // reported as their own paths.
                        let names = |candidate: &str| !name.is_empty() && fold(candidate) == name;
                        for row in &self.rows {
                            if row.common_dir.as_deref() != Some(common) {
                                continue;
                            }
                            let upstream = row.upstream.as_deref().unwrap_or_default();
                            let reached = match kind {
                                Some("heads") => names(&row.branch) || names(upstream),
                                Some("remotes") => names(upstream),
                                _ => false,
                            };
                            if reached {
                                keys.insert(row.key.clone());
                            }
                        }
                    }
                    // Every checkout resolves refs and upstreams through
                    // the first two, and counts untracked files by the
                    // excludes under the third.
                    Some("packed-refs" | "config" | "info") => return Touch::All,
                    // HEAD, the index and operation state of the checkout
                    // this directory administers.
                    _ => match owner(common) {
                        Some(key) => {
                            keys.insert(key);
                        }
                        None => return Touch::All,
                    },
                }
                continue;
            }
            let checkout = self
                .rows
                .iter()
                .filter(|row| path.starts_with(&row.path))
                .max_by_key(|row| row.path.components().count());
            match checkout {
                Some(row) => {
                    keys.insert(row.key.clone());
                }
                // The root itself, or a directory in it that is not listed.
                None if path.starts_with(worktree_root) => {}
                None => return Touch::All,
            }
        }
        Touch::Rows(keys)
    }
}

/// Requests metadata reads for changes other processes made, no more often
/// than `interval`: the first at once, and one more when the interval has
/// passed if anything changed meanwhile.
struct Paced {
    metadata: SourceHandle<crate::local_source::Metadata>,
    interval: Duration,
    /// When the last read was asked for, and whether another is held back.
    state: Mutex<(Option<std::time::Instant>, bool)>,
    held: tokio::sync::Notify,
}

impl Paced {
    fn new(metadata: SourceHandle<crate::local_source::Metadata>, interval: Duration) -> Self {
        Self {
            metadata,
            interval,
            state: Mutex::default(),
            held: tokio::sync::Notify::new(),
        }
    }

    fn signal(&self) {
        let now = std::time::Instant::now();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .0
            .is_none_or(|last| now.duration_since(last) >= self.interval)
        {
            *state = (Some(now), false);
            drop(state);
            self.metadata.refresh();
        } else if !std::mem::replace(&mut state.1, true) {
            self.held.notify_one();
        }
    }

    /// When the read being held back is due.
    fn due(&self) -> Option<tokio::time::Instant> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.1.then(|| {
            let waited = state.0.map_or(self.interval, |last| last.elapsed());
            tokio::time::Instant::now() + self.interval.saturating_sub(waited)
        })
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if std::mem::take(&mut state.1) {
            state.0 = Some(std::time::Instant::now());
            drop(state);
            self.metadata.refresh();
        }
    }
}

/// Turns changed paths into source requests.
struct Router {
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: Arc<Paced>,
    state_db: PathBuf,
    /// The main clone and the worktree root.
    checkout_roots: [PathBuf; 2],
    layout: RwLock<Layout>,
    contents: Mutex<Contents>,
}

/// Files Git rewrites in place with the bytes they already had (the config
/// on every `git branch --force`, packed refs on a fetch that moved
/// nothing), each of which would otherwise read every checkout again.
const REWRITTEN_FILES: [&str; 2] = ["config", "packed-refs"];

/// The largest of those files that is read to compare. A larger one (packed
/// refs of a very large repository) always counts as changed.
const REWRITTEN_LIMIT: u64 = 4 * 1024 * 1024;

/// What those files held when last read, for the Git directories among the
/// watch roots. Only a file that was read both times can compare equal: a
/// path that is missing, unreadable, too large or not a regular file has no
/// entry, and a change to it is always a change.
#[derive(Default)]
struct Contents {
    /// Watch roots already looked at, and those of them that are Git
    /// directories. A working tree may hold a `config` of its own (a
    /// directory, in many projects); that is never one of these files.
    examined: BTreeSet<PathBuf>,
    git_dirs: BTreeSet<PathBuf>,
    hashes: BTreeMap<PathBuf, u64>,
}

fn content_hash(path: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > REWRITTEN_LIMIT {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

/// Whether a watch root is a Git directory (a repository's own, or a linked
/// checkout's under `worktrees/`), as opposed to a working tree.
fn is_git_dir(root: &Path) -> bool {
    root.join("HEAD").is_file()
        && (root.join("objects").is_dir() || root.join("commondir").is_file())
}

impl Router {
    /// Remember what the rewritten files of new roots hold, before the roots
    /// are sampled: a change after this differs from it.
    async fn baseline(self: Arc<Self>, roots: Vec<PathBuf>) {
        let _ = tokio::task::spawn_blocking(move || {
            let new: Vec<PathBuf> = match self.contents.lock() {
                Ok(mut contents) => roots
                    .into_iter()
                    .filter(|root| contents.examined.insert(root.clone()))
                    .collect(),
                Err(_) => return,
            };
            // Files are read with the lock released: it is also taken where
            // changes are routed.
            let read: Vec<(PathBuf, Vec<(PathBuf, u64)>)> = new
                .into_iter()
                .filter(|root| is_git_dir(root))
                .map(|root| {
                    let hashes = REWRITTEN_FILES
                        .iter()
                        .map(|name| root.join(name))
                        .filter_map(|path| content_hash(&path).map(|hash| (path, hash)))
                        .collect();
                    (root, hashes)
                })
                .collect();
            if let Ok(mut contents) = self.contents.lock() {
                for (root, hashes) in read {
                    contents.git_dirs.insert(root);
                    contents.hashes.extend(hashes);
                }
            }
        })
        .await;
    }

    /// Whether a changed path is one of the rewritten files holding exactly
    /// what it held before.
    fn rewritten_unchanged(&self, path: &Path) -> bool {
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| REWRITTEN_FILES.contains(&name))
        {
            return false;
        }
        let in_git_dir = self.contents.lock().is_ok_and(|contents| {
            path.parent()
                .is_some_and(|parent| contents.git_dirs.contains(parent))
        });
        if !in_git_dir {
            return false;
        }
        let now = content_hash(path);
        let Ok(mut contents) = self.contents.lock() else {
            return false;
        };
        match now {
            Some(now) => contents.hashes.insert(path.to_path_buf(), now) == Some(now),
            None => {
                contents.hashes.remove(path);
                false
            }
        }
    }

    fn invalidate(&self, paths: &[PathBuf]) {
        let changed: Vec<PathBuf> = paths
            .iter()
            .filter(|path| !self.rewritten_unchanged(path))
            .cloned()
            .collect();
        if changed.is_empty() && !paths.is_empty() {
            return;
        }
        let paths = changed.as_slice();
        let (git, metadata) = invalidations(paths, &self.state_db, &self.checkout_roots);
        tracing::debug!(?paths, git, metadata, "filesystem invalidation");
        if metadata {
            self.metadata.signal();
        }
        if !git {
            return;
        }
        let git_paths: Vec<PathBuf> = paths
            .iter()
            .filter(|path| invalidation(path, &self.state_db, &self.checkout_roots).0)
            .cloned()
            .collect();
        let touch = self.layout.read().map_or(Touch::All, |layout| {
            layout.touch(&git_paths, &self.checkout_roots[1])
        });
        match touch {
            Touch::All => self.git.request(Request::Recheck),
            Touch::Rows(keys) => self.git.request_keys(Request::Recheck, keys),
        };
    }
}

/// The watch set, its backend and the baseline later samples compare with.
struct Watch {
    /// `None` where the scan is the only detector, or the backend failed.
    watcher: Option<WtWatcher>,
    /// Roots being watched: with the backend when there is one.
    registered: BTreeMap<PathBuf, bool>,
    /// Roots the fingerprint holds a sample of.
    sampled: BTreeMap<PathBuf, bool>,
    fingerprint: Option<Arc<WatchFingerprint>>,
    /// Shared with the notification callback, to drop repeats of a change
    /// a scan already reported.
    observed: Arc<RwLock<Option<Arc<WatchFingerprint>>>>,
    probe: Option<WatchProbe>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl Watch {
    /// Watch exactly `desired`, and sample every root that is new so a
    /// later scan has something to compare it with. Returns the new roots.
    async fn apply(&mut self, desired: BTreeMap<PathBuf, bool>) -> BTreeSet<PathBuf> {
        match self.watcher.take() {
            Some(active) => {
                let registered = std::mem::take(&mut self.registered);
                let cancel = self.cancellation.clone();
                let wanted = desired.clone();
                let update = tokio::task::spawn_blocking(move || {
                    update_watches(active, registered, wanted, &cancel)
                });
                match update.await {
                    Ok((active, registered, _cancelled)) => {
                        self.watcher = Some(active);
                        self.registered = registered;
                    }
                    Err(error) => {
                        tracing::error!(%error, "filesystem watch worker stopped; using the scan");
                        self.registered = desired;
                    }
                }
            }
            None => self.registered = desired,
        }
        if let Some(probe) = &self.probe
            && let Ok(mut probe) = probe.lock()
        {
            *probe = self.registered.clone();
        }
        let before = self.sampled.clone();
        self.sampled
            .retain(|root, recursive| self.registered.get(root) == Some(recursive));
        let fresh: BTreeMap<PathBuf, bool> = self
            .registered
            .iter()
            .filter(|(root, recursive)| self.sampled.get(*root) != Some(*recursive))
            .map(|(root, recursive)| (root.clone(), *recursive))
            .collect();
        let mut fingerprint = self.fingerprint.as_deref().cloned().unwrap_or_default();
        if self.sampled != before {
            fingerprint.retain(|path, _| covers(&self.sampled, path));
        }
        if !fresh.is_empty() {
            let roots = fresh.clone();
            let cancel = self.cancellation.clone();
            let sample =
                tokio::task::spawn_blocking(move || watch_fingerprint(&roots, &cancel)).await;
            let Ok(Some(sample)) = sample else {
                // Cancelled: nothing was sampled, so nothing is covered.
                return BTreeSet::new();
            };
            for (path, signature) in sample {
                // A path an older root already samples keeps its older
                // signature: a change since then is still owed to the next
                // scan, and overwriting it here would swallow it.
                if !covers(&self.sampled, &path) {
                    fingerprint.insert(path, signature);
                }
            }
            self.sampled.extend(fresh.clone());
        }
        self.publish(fingerprint);
        fresh.into_keys().collect()
    }

    fn publish(&mut self, fingerprint: WatchFingerprint) {
        let fingerprint = Arc::new(fingerprint);
        if let Ok(mut observed) = self.observed.write() {
            *observed = Some(fingerprint.clone());
        }
        self.fingerprint = Some(fingerprint);
    }

    /// Fold in a finished scan of `roots` and return what changed since the
    /// last sample. A path counts only where both samples looked: a root
    /// added or dropped while the scan ran is not a change to its files.
    fn settle(&mut self, roots: &BTreeMap<PathBuf, bool>, scan: WatchFingerprint) -> Vec<PathBuf> {
        let Some(previous) = self.fingerprint.clone() else {
            self.sampled = roots.clone();
            self.publish(scan);
            return Vec::new();
        };
        if *roots == self.sampled {
            let changed = changed_paths(&previous, &scan);
            self.publish(scan);
            return changed;
        }
        let both = |path: &PathBuf| covers(roots, path) && covers(&self.sampled, path);
        let changed = changed_paths(&previous, &scan)
            .into_iter()
            .filter(both)
            .collect();
        let mut next: WatchFingerprint = scan
            .into_iter()
            .filter(|(path, _)| covers(&self.sampled, path))
            .collect();
        for (path, signature) in previous.iter() {
            if !covers(roots, path) {
                next.insert(path.clone(), signature.clone());
            }
        }
        self.publish(next);
        changed
    }
}

/// Whether a sample of `roots` includes `path`: a root, a direct child of
/// one, or anything below a recursive one.
fn covers(roots: &BTreeMap<PathBuf, bool>, path: &Path) -> bool {
    if roots.contains_key(path)
        || path
            .parent()
            .is_some_and(|parent| roots.contains_key(parent))
    {
        return true;
    }
    path.ancestors()
        .skip(1)
        .any(|ancestor| roots.get(ancestor) == Some(&true))
}

#[expect(
    clippy::too_many_arguments,
    reason = "one caller per mode; the last two are test seams"
)]
fn start_with_notifications(
    scope: &TaskScope,
    config: Arc<Config>,
    source: SourceHandle<Board>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<crate::local_source::Metadata>,
    mut coverage: CoverageRequests,
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
    // Native notifications can arrive after the metadata scan already
    // observed and invalidated an edit. Share that witness across both lanes.
    let observed_files = Arc::new(RwLock::new(None::<Arc<WatchFingerprint>>));
    // A watched checkout disappeared; re-read the watch list.
    let rewatch = Arc::new(tokio::sync::Notify::new());
    let vanished_gate = Arc::new(RateGate::new(VANISHED_INTERVAL));
    let cancellation = scope.token();
    // Another process's write to the state database reaches the board
    // through this file alone, so it is not left to the one-second scan.
    let paced = Arc::new(Paced::new(metadata.clone(), METADATA_PACE));
    let signals = (!notifications).then(|| {
        let database = config.paths.state_db.clone();
        let mut wal = database.clone().into_os_string();
        wal.push("-wal");
        let paced = paced.clone();
        signal_tick(scope, vec![database, wal.into()], move || paced.signal())
    });
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
        let state_db = canonical_watch_path(&config.paths.state_db).await;
        let router = Arc::new(Router {
            git: git.clone(),
            metadata: paced.clone(),
            state_db: state_db.clone(),
            checkout_roots: [
                canonical_watch_path(&config.paths.main_clone).await,
                canonical_watch_path(&config.paths.worktree_root).await,
            ],
            layout: RwLock::default(),
            contents: Mutex::default(),
        });
        let watcher = if notifications {
            let callback_router = router.clone();
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
                        if event.paths.is_empty() || !paths.is_empty() {
                            callback_router.invalidate(&paths);
                        }
                        if edit_event(&event) {
                            let roots = callback_roots.read().ok();
                            let mut changed = false;
                            if let Some(roots) = roots.as_deref() {
                                let timestamp = now_ms();
                                let slugs: BTreeSet<_> = event.paths.iter().take(256)
                                    .filter(|path| !is_state_db_path(path, &callback_router.state_db))
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
                            callback_router.invalidate(&error.paths);
                            callback_router.metadata.metadata.refresh();
                            callback_rewatch.notify_one();
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "filesystem notification lost; refreshing source");
                        callback_router.git.refresh();
                        callback_router.metadata.metadata.refresh();
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
            match watcher {
                Ok(Ok(watcher)) => Some(watcher),
                error => {
                    tracing::warn!(?error, "filesystem watcher unavailable; using the scan");
                    None
                }
            }
        } else {
            None
        };
        let mut watch = Watch {
            watcher,
            registered: BTreeMap::new(),
            sampled: BTreeMap::new(),
            fingerprint: None,
            observed: observed_files,
            probe,
            cancellation: cancellation.clone(),
        };
        let mut updates = source.subscribe();
        updates.mark_changed();
        // The watch list derives from the Git source's inventory, minus
        // checkouts with an accepted removal. No separate inventory scan.
        let mut git_updates = git.subscribe();
        let mut metadata_updates = metadata.subscribe();
        git_updates.mark_changed();
        metadata_updates.mark_changed();
        let mut inputs = Vec::<WorktreeRecord>::new();
        // Inventory checkouts kept out of `inputs`: their removal is accepted.
        let mut unwatched = BTreeSet::<String>::new();
        // Checkouts a listing named that no published inventory shows yet,
        // with the moment each was named.
        let mut listed = Vec::<(WorktreeRecord, Stamp)>::new();
        // Some listed checkout was missing at the last reconcile; any later
        // inventory may be the one that shows it again.
        let mut inputs_missing = false;
        let mut latest_edits = BTreeMap::<String, i64>::new();
        let mut edits_published = false;
        // File invalidation belongs to the host even when no automation
        // subscribes to the optional edit-timestamp stream.
        let mut edits_open = true;
        let mut coverage_open = true;
        let mut reconcile = true;
        let mut backstop = tokio::time::interval_at(tokio::time::Instant::now() + BACKSTOP, BACKSTOP);
        backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut next_scan = tokio::time::Instant::now() + SCAN_INTERVAL;
        let mut scan = None::<Scan>;
        let slow_scan = RateGate::new(BACKSTOP);
        loop {
            if std::mem::take(&mut reconcile) {
                let records: Vec<WorktreeRecord> = inputs
                    .iter()
                    .chain(listed.iter().map(|(record, _)| record))
                    .cloned()
                    .collect();
                match desired_paths(&config, &records).await {
                    Ok((paths, missing)) => {
                        inputs_missing = missing;
                        router.clone().baseline(paths.keys().cloned().collect()).await;
                        let fresh = watch.apply(paths).await;
                        if cancellation.is_cancelled() {
                            break;
                        }
                        // A root that was not watched until now may have
                        // changed since its rows were read, and no sample
                        // will ever show that. Read what it can reach again.
                        //
                        // That needs rows read before now. A listing waits
                        // for its checkouts to be covered before reading
                        // them, so until the leaf has published live rows
                        // there are none: the first load is still to come or
                        // is waiting on this very baseline, and asking again
                        // would only read every checkout twice at start.
                        let read_before = git
                            .snapshot()
                            .domains()
                            .get(&Domain::Git)
                            .is_some_and(|meta| meta.is_live());
                        if !fresh.is_empty() && read_before {
                            let rows: Option<Vec<String>> = fresh
                                .iter()
                                .map(|root| {
                                    records
                                        .iter()
                                        .zip(crate::local_source::git_keys(&records))
                                        .find(|(record, _)| {
                                            Path::new(&record.target.path) == root.as_path()
                                        })
                                        .map(|(_, key)| key)
                                })
                                .collect();
                            tracing::debug!(?fresh, ?rows, "new watch roots invalidate Git");
                            match rows {
                                Some(rows) => git.request_keys(Request::Recheck, rows),
                                None => git.request(Request::Recheck),
                            };
                        }
                        // A checkout left unwatched on purpose (its removal is
                        // accepted) has nothing more to wait for either.
                        if let Ok(mut covered) = coverage.covered.write() {
                            *covered = records
                                .iter()
                                .map(|record| record.target.path.clone())
                                .chain(unwatched.iter().cloned())
                                .collect();
                        }
                        if !edits_published {
                            edits_published = true;
                            edit_publisher.publish_live(latest_edits.clone(), Stamp::now());
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "cannot reconcile filesystem watches");
                    }
                }
            }
            let metadata_due = paced.due();
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
                    if let Some(rows) = snapshot.data()
                        && let Ok(mut layout) = router.layout.write()
                    {
                        *layout = Layout::new(rows);
                    }
                    // A live inventory listed at or after a checkout was
                    // named decides whether it is still there.
                    if let Some(meta) = snapshot.domains().get(&Domain::Git).filter(|meta| meta.is_live()) {
                        let index = meta.keyed().map_or(meta.observed(), |keyed| keyed.index());
                        let named = listed.len();
                        listed.retain(|(_, named)| index.at_or_after(named) != Some(true));
                        reconcile |= listed.len() != named;
                    }
                    let next = watch_inputs(&snapshot, &metadata.snapshot());
                    reconcile |= next != inputs || inputs_missing;
                    unwatched = unwatched_paths(&snapshot, &next);
                    inputs = next;
                }
                changed = metadata_updates.changed() => {
                    if changed.is_err() { break; }
                    let snapshot = metadata_updates.borrow_and_update().clone();
                    let rows = git.snapshot();
                    let next = watch_inputs(&rows, &snapshot);
                    reconcile |= next != inputs;
                    unwatched = unwatched_paths(&rows, &next);
                    inputs = next;
                }
                _ = rewatch.notified() => { reconcile = true; }
                request = coverage.requests.recv(), if coverage_open => {
                    let Some(request) = request else {
                        coverage_open = false;
                        continue;
                    };
                    let state = metadata.snapshot();
                    let state = state.data().map(|metadata| &metadata.0);
                    let named = Stamp::now();
                    let mut records: Vec<WorktreeRecord> = inputs
                        .iter()
                        .chain(listed.iter().map(|(record, _)| record))
                        .cloned()
                        .collect();
                    let mut asked = Vec::with_capacity(request.records.len());
                    let asked_keys = crate::local_source::git_keys(&request.records);
                    for record in request.records {
                        asked.push(record.target.path.clone());
                        if !state.is_some_and(|state| removing(state, &record))
                            && !records.iter().any(|known| known.target.path == record.target.path)
                        {
                            records.push(record.clone());
                            listed.push((record, named));
                        }
                    }
                    // The listing waits for this, so its rows are read
                    // against a baseline: nothing here needs a second read.
                    match desired_paths(&config, &records).await {
                        Ok((paths, missing)) => {
                            inputs_missing |= missing;
                            router.clone().baseline(paths.keys().cloned().collect()).await;
                            watch.apply(paths).await;
                            if cancellation.is_cancelled() {
                                break;
                            }
                            if let Ok(mut covered) = coverage.covered.write() {
                                covered.extend(asked);
                            }
                        }
                        Err(error) => tracing::warn!(%error, "cannot cover listed checkouts"),
                    }
                    // A listing that stopped waiting has read its rows, or is
                    // reading them, without this baseline: whatever changed
                    // between such a read and now will never differ from a
                    // sample. Read those rows again against it.
                    if request.done.send(()).is_err() {
                        tracing::debug!(rows = ?asked_keys, "checkouts were covered after their listing stopped waiting");
                        git.request_keys(Request::Recheck, asked_keys);
                    }
                }
                _ = paced.held.notified() => {}
                _ = tokio::time::sleep_until(metadata_due.unwrap_or_else(tokio::time::Instant::now)), if metadata_due.is_some() => {
                    paced.release();
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
                finished = async {
                    match scan.as_mut() {
                        Some(scan) => (&mut scan.work).await,
                        None => std::future::pending().await,
                    }
                }, if scan.is_some() => {
                    let Some(Scan { roots, started, .. }) = scan.take() else { continue };
                    let elapsed = started.elapsed();
                    next_scan = started + SCAN_INTERVAL.max(elapsed * SCAN_DUTY);
                    if elapsed > SCAN_INTERVAL && slow_scan.allow() {
                        tracing::warn!(
                            roots = roots.len(),
                            elapsed_ms = elapsed.as_millis(),
                            "the filesystem scan is slow; changes are noticed that much later"
                        );
                    }
                    let Ok(Some(sample)) = finished else { continue };
                    let entries = sample.len();
                    let mut changed = watch.settle(&roots, sample);
                    // A signal file the tick has already reported as it is now.
                    if let Some(signals) = &signals
                        && let (Ok(seen), Some(now)) = (signals.lock(), watch.fingerprint.as_deref())
                    {
                        changed.retain(|path| seen.get(path).is_none_or(|seen| seen.as_ref() != now.get(path)));
                    }
                    changed.retain(|path| relevant_path(path));
                    tracing::debug!(entries, elapsed_ms = elapsed.as_millis(), ?changed, "filesystem scan");
                    if changed.is_empty() {
                        continue;
                    }
                    // Routing may read a rewritten file to see whether its
                    // bytes changed.
                    let routing = (router.clone(), changed.clone());
                    let _ = tokio::task::spawn_blocking(move || routing.0.invalidate(&routing.1)).await;
                    // Without notifications the scan is also what dates edits.
                    if watch.watcher.is_none()
                        && let Ok(roots) = edit_roots.read()
                    {
                        let timestamp = now_ms();
                        let slugs: BTreeSet<_> = changed.iter().take(256)
                            .filter(|path| !is_state_db_path(path, &state_db))
                            .filter_map(|path| edited_slug(path, &roots))
                            .collect();
                        if !slugs.is_empty() {
                            for slug in slugs {
                                latest_edits.entry(slug).and_modify(|current| *current = (*current).max(timestamp)).or_insert(timestamp);
                            }
                            edit_publisher.publish_live(latest_edits.clone(), Stamp::now());
                        }
                    }
                }
                _ = tokio::time::sleep_until(next_scan), if scan.is_none() => {
                    let roots = watch.registered.clone();
                    let scan_roots = roots.clone();
                    let scan_cancel = cancellation.clone();
                    scan = Some(Scan {
                        roots,
                        started: tokio::time::Instant::now(),
                        work: tokio::task::spawn_blocking(move || watch_fingerprint(&scan_roots, &scan_cancel)),
                    });
                }
                _ = backstop.tick() => { git.refresh(); metadata.refresh(); reconcile = true; }
            }
        }
        // A scan in flight stops at its next cancellation check.
        if let Some(scan) = scan {
            let _ = scan.work.await;
        }
        wt_app_core::watcher_cleanup::drop_watcher(watch.watcher).await;
    });
    edits
}

/// A metadata scan in flight, with the roots it was started for.
struct Scan {
    roots: BTreeMap<PathBuf, bool>,
    started: tokio::time::Instant,
    work: tokio::task::JoinHandle<Option<WatchFingerprint>>,
}

/// File metadata of the watch set, not a recursive checkout scan. The watch
/// contract includes immediate checkout children and recursive Git metadata
/// only. Symlinks and generated dependency trees are never followed.
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

fn changed_paths(previous: &WatchFingerprint, next: &WatchFingerprint) -> Vec<PathBuf> {
    previous
        .keys()
        .chain(next.keys())
        .filter(|path| previous.get(*path) != next.get(*path))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Sample the whole watch set. Only cancellation ends a scan early: a sample
/// cut short by a size or time limit would compare as nothing changed while
/// still costing the walk. A large set is paced by its caller instead. A
/// directory that cannot be listed (it is being removed) keeps its own entry
/// and loses its children, which reads as their removal.
fn watch_fingerprint(
    paths: &BTreeMap<PathBuf, bool>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Option<WatchFingerprint> {
    let mut result = BTreeMap::new();
    let mut pending: Vec<_> = paths
        .iter()
        .map(|(path, recursive)| (path.clone(), *recursive, true))
        .collect();
    while let Some((path, recursive, descend)) = pending.pop() {
        if cancellation.is_cancelled() {
            return None;
        }
        if generated_path(&path) {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        result.insert(path.clone(), FileSignature::from_metadata(&metadata));
        if descend && metadata.is_dir() {
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !generated_path(&path) {
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

/// Paths of inventory checkouts that `inputs` leaves out.
fn unwatched_paths(
    git: &wt_runtime::SourceSnapshot<Vec<WorktreeSnapshot>>,
    inputs: &[WorktreeRecord],
) -> BTreeSet<String> {
    git.data()
        .into_iter()
        .flat_map(|rows| rows.iter())
        .map(|row| &row.worktree.target.path)
        .filter(|path| !inputs.iter().any(|record| record.target.path == **path))
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
    event.paths.is_empty() || event.paths.iter().any(|path| relevant_path(path))
}

/// Lock files and fetch bookkeeping change nothing the board shows.
fn relevant_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    !name.ends_with(".lock") && !matches!(name, "FETCH_HEAD" | "ORIG_HEAD") && !generated_path(path)
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
    paths.iter().fold((false, false), |(git, metadata), path| {
        let path = invalidation(path, state_db, checkout_roots);
        (git | path.0, metadata | path.1)
    })
}

/// Whether one changed path invalidates Git facts, presentation state, both
/// or neither.
fn invalidation(path: &Path, state_db: &Path, checkout_roots: &[PathBuf]) -> (bool, bool) {
    if is_state_db_path(path, state_db) {
        (false, true)
    } else if Some(path) == state_db.parent() {
        // Creating/removing a WAL changes its parent directory's mtime.
        // The metadata scan samples that directory too; it is not a Git
        // edit unless the shared state directory is inside a checkout.
        (
            checkout_roots.iter().any(|root| path.starts_with(root)),
            true,
        )
    } else {
        (
            path.parent() != state_db.parent()
                || checkout_roots.iter().any(|root| path.starts_with(root)),
            false,
        )
    }
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
        assert_eq!(changed_paths(&before, &after), std::slice::from_ref(&file));
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
        assert!(changed_paths(&first, &watch_fingerprint(&paths, &cancel).unwrap()).is_empty());
        std::fs::write(root.path().join("tracked.txt"), "changed").unwrap();
        let next = watch_fingerprint(&paths, &cancel).unwrap();
        assert_eq!(
            changed_paths(&first, &next),
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
            coverage().1,
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
            coverage().1,
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

    fn layout_row(
        key: &str,
        path: &str,
        git_dir: &str,
        branch: &str,
        upstream: Option<&str>,
    ) -> LayoutRow {
        LayoutRow {
            key: key.into(),
            path: path.into(),
            git_dir: Some(git_dir.into()),
            common_dir: Some("/repo/main/.git".into()),
            branch: branch.into(),
            upstream: upstream.map(Into::into),
        }
    }

    #[test]
    fn changed_paths_reach_only_the_checkouts_they_can_affect() {
        let layout = Layout {
            rows: vec![
                layout_row(
                    "main",
                    "/repo/main",
                    "/repo/main/.git",
                    "main",
                    Some("origin/main"),
                ),
                layout_row(
                    "one",
                    "/wt/one",
                    "/repo/main/.git/worktrees/one",
                    "feature/one",
                    Some("origin/feature/one"),
                ),
                layout_row(
                    "two",
                    "/wt/two",
                    "/repo/main/.git/worktrees/two",
                    "feature/two",
                    None,
                ),
            ],
        };
        let touch = |path: &str| layout.touch(&[PathBuf::from(path)], Path::new("/wt"));
        let rows = |keys: &[&str]| Touch::Rows(keys.iter().map(|key| (*key).to_owned()).collect());
        // A checkout's files and its own Git administration reach that row.
        assert_eq!(touch("/wt/one/src/lib.rs"), rows(&["one"]));
        assert_eq!(touch("/wt/one/.git"), rows(&["one"]));
        assert_eq!(touch("/repo/main/.git/worktrees/one/index"), rows(&["one"]));
        assert_eq!(touch("/repo/main/.git/worktrees/one"), rows(&["one"]));
        assert_eq!(touch("/repo/main/.git/index"), rows(&["main"]));
        assert_eq!(touch("/repo/main/.git/HEAD"), rows(&["main"]));
        assert_eq!(touch("/repo/main/tracked.txt"), rows(&["main"]));
        // The Git directory itself and its object store change with every
        // lock file and commit; nothing a status reads is in that.
        assert_eq!(touch("/repo/main/.git"), rows(&[]));
        assert_eq!(touch("/repo/main/.git/objects"), rows(&[]));
        // A ref reaches the checkouts on it or tracking it. A directory of
        // refs reaches none: it changes with every lock file made in it, and
        // the refs that moved are reported themselves.
        assert_eq!(
            touch("/repo/main/.git/refs/heads/feature/one"),
            rows(&["one"])
        );
        assert_eq!(touch("/repo/main/.git/refs/heads/feature"), rows(&[]));
        assert_eq!(touch("/repo/main/.git/refs/remotes/origin"), rows(&[]));
        assert_eq!(
            touch("/repo/main/.git/refs/remotes/origin/feature/one"),
            rows(&["one"])
        );
        assert_eq!(
            touch("/repo/main/.git/refs/remotes/origin/main"),
            rows(&["main"])
        );
        assert_eq!(touch("/repo/main/.git/refs/heads/feature/o"), rows(&[]));
        // Nothing a status reads, but still a listing: facts derived from
        // refs wake on it, and a new or removed checkout shows there.
        assert_eq!(touch("/repo/main/.git/refs/tags/v1"), rows(&[]));
        assert_eq!(
            touch("/repo/main/.git/refs/remotes/origin/other"),
            rows(&[])
        );
        assert_eq!(touch("/repo/main/.git/worktrees"), rows(&[]));
        assert_eq!(touch("/repo/main/.git/worktrees/arriving/HEAD"), rows(&[]));
        assert_eq!(touch("/wt"), rows(&[]));
        assert_eq!(touch("/wt/arriving/file"), rows(&[]));
        // Shared by every checkout, or not placeable at all: read everything.
        assert_eq!(touch("/repo/main/.git/packed-refs"), Touch::All);
        assert_eq!(touch("/repo/main/.git/config"), Touch::All);
        // The excludes every checkout counts its untracked files by.
        assert_eq!(touch("/repo/main/.git/info"), Touch::All);
        assert_eq!(touch("/repo/main/.git/info/exclude"), Touch::All);
        // Where the filesystem folds case, a ref file need not be spelled
        // as its branch is.
        let folded: &[&str] = if REFS_FOLD_CASE { &["one"] } else { &[] };
        assert_eq!(
            touch("/repo/main/.git/refs/heads/Feature/one"),
            rows(folded)
        );
        assert_eq!(
            touch("/repo/main/.git/refs/remotes/origin/FEATURE/one"),
            rows(folded)
        );
        assert_eq!(touch("/elsewhere/file"), Touch::All);
        assert_eq!(layout.touch(&[], Path::new("/wt")), Touch::All);
        assert_eq!(
            layout.touch(
                &[
                    "/wt/one/a".into(),
                    "/repo/main/.git/worktrees/two/HEAD".into()
                ],
                Path::new("/wt")
            ),
            rows(&["one", "two"])
        );
        assert_eq!(
            layout.touch(&["/wt/one/a".into(), "/elsewhere".into()], Path::new("/wt")),
            Touch::All
        );
        // With no inventory yet nothing can be placed.
        assert_eq!(
            Layout::default().touch(&["/repo/main/.git/index".into()], Path::new("/wt")),
            Touch::All
        );
    }

    #[test]
    fn a_scan_past_the_old_entry_limit_is_still_a_whole_sample() {
        let root = tempfile::tempdir().unwrap();
        let refs = root.path().join("refs");
        std::fs::create_dir(&refs).unwrap();
        for index in 0..9_000 {
            std::fs::write(refs.join(format!("ref-{index}")), "").unwrap();
        }
        let paths = BTreeMap::from([(refs.clone(), true)]);
        let cancel = tokio_util::sync::CancellationToken::new();
        let first = watch_fingerprint(&paths, &cancel).expect("a large watch set is sampled");
        assert_eq!(first.len(), 9_001);
        std::fs::write(refs.join("ref-8999"), "moved").unwrap();
        let next = watch_fingerprint(&paths, &cancel).unwrap();
        assert_eq!(changed_paths(&first, &next), [refs.join("ref-8999")]);
    }

    #[tokio::test]
    async fn new_roots_get_a_baseline_and_changes_since_it_are_reported() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let (old, new) = (root.join("old"), root.join("new"));
        std::fs::create_dir(&old).unwrap();
        std::fs::create_dir(&new).unwrap();
        std::fs::write(old.join("file"), "a").unwrap();
        std::fs::write(new.join("file"), "a").unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut watch = Watch {
            watcher: None,
            registered: BTreeMap::new(),
            sampled: BTreeMap::new(),
            fingerprint: None,
            observed: Arc::default(),
            probe: None,
            cancellation: cancel.clone(),
        };
        let only_old = BTreeMap::from([(root.clone(), false), (old.clone(), false)]);
        assert_eq!(watch.apply(only_old.clone()).await.len(), 2);
        // The first sample of a root is a baseline, not a change.
        assert!(
            watch
                .settle(&only_old, watch_fingerprint(&only_old, &cancel).unwrap())
                .is_empty()
        );

        // A scan starts, then a root is added and baselined while it runs.
        let scan = watch_fingerprint(&only_old, &cancel).unwrap();
        let both = BTreeMap::from([
            (root.clone(), false),
            (old.clone(), false),
            (new.clone(), false),
        ]);
        assert_eq!(
            watch.apply(both.clone()).await,
            BTreeSet::from([new.clone()])
        );
        std::fs::write(new.join("file"), "changed after the baseline").unwrap();
        std::fs::write(old.join("file"), "changed too").unwrap();
        // The older scan knows nothing of the new root: that is not a change
        // to its files, and it must not cost the root its baseline.
        assert!(watch.settle(&only_old, scan).is_empty());
        let changed = watch.settle(&both, watch_fingerprint(&both, &cancel).unwrap());
        assert_eq!(changed, [new.join("file"), old.join("file")]);

        // Dropping a root is not a change to its files either.
        let scan = watch_fingerprint(&both, &cancel).unwrap();
        watch.apply(only_old.clone()).await;
        assert!(watch.settle(&both, scan).is_empty());
        assert!(
            !watch
                .fingerprint
                .as_ref()
                .unwrap()
                .contains_key(&new.join("file"))
        );
        assert!(covers(&only_old, &old.join("file")));
        assert!(!covers(&only_old, &old.join("deep/file")));
        assert!(covers(
            &BTreeMap::from([(old.clone(), true)]),
            &old.join("deep/file")
        ));
    }

    #[tokio::test]
    async fn signal_files_are_noticed_within_a_few_ticks_and_only_when_they_change() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.sqlite");
        let wal = root.path().join("state.sqlite-wal");
        std::fs::write(&database, "a").unwrap();
        let scope = TaskScope::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = signal_tick(&scope, vec![database.clone(), wal.clone()], {
            let calls = calls.clone();
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
            }
        });
        tokio::time::sleep(SIGNAL_TICK * 4).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the first look is a baseline"
        );
        // A file that appears is a change, as the WAL does on first write.
        let written = std::time::Instant::now();
        std::fs::write(&wal, "frame").unwrap();
        while calls.load(Ordering::SeqCst) == 0 {
            assert!(
                written.elapsed() < Duration::from_secs(5),
                "the tick never fired"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let noticed = written.elapsed();
        tokio::time::sleep(SIGNAL_TICK * 4).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "an unchanged file is not reported again"
        );
        let wal = std::fs::canonicalize(&wal).unwrap();
        assert!(seen.lock().unwrap().get(&wal).is_some_and(Option::is_some));
        scope.shutdown(Duration::from_secs(2)).await.unwrap();
        // Generous for a loaded machine; the one-second scan it replaces
        // would average half a second and reach a full one.
        assert!(
            noticed < Duration::from_millis(600),
            "noticed after {noticed:?}"
        );
    }

    #[tokio::test]
    async fn checkouts_covered_after_their_listing_gave_up_are_read_again() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let records = fixture
            .ctx
            .repository
            .inventory(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let scope = TaskScope::new();
        fn spec<T>(name: &'static str, domain: Domain) -> SourceSpec<T> {
            SourceSpec {
                name,
                domain,
                persistence: Persistence::Never("isolated test"),
                policy: RefreshPolicy::default(),
            }
        }
        let (git, mut git_publisher) =
            leaf_channel::<Vec<WorktreeSnapshot>>(&scope, spec("test-git", Domain::Git));
        let (metadata, _metadata_publisher) = leaf_channel::<crate::local_source::Metadata>(
            &scope,
            spec("test-metadata", Domain::Metadata),
        );
        let (board, _board_publisher) = wt_runtime::derived_channel(&scope, "test-board");
        let (coverage, requests) = coverage();
        // The listing asked for coverage and stopped waiting before the
        // watcher got to it: nobody is listening for the answer.
        let (done, gave_up) = oneshot::channel();
        drop(gave_up);
        coverage
            .requests
            .send(CoverRequest {
                records: records.clone(),
                done,
            })
            .await
            .unwrap();
        let _edits = start_with_notifications(
            &scope,
            fixture.ctx.config.clone(),
            board,
            git.view(),
            metadata.view(),
            requests,
            false,
            None,
        );
        // Its rows were read without a baseline, so each is asked for again.
        let mut asked = BTreeSet::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while asked.len() < records.len() {
            tokio::time::timeout_at(deadline, git_publisher.requested())
                .await
                .expect("the late rows were never asked for again")
                .unwrap();
            let demand = git_publisher.take_demand();
            assert!(
                demand
                    .keys
                    .values()
                    .all(|request| *request == Request::Recheck)
            );
            asked.extend(demand.keys.into_keys());
        }
        assert_eq!(
            asked,
            BTreeSet::from(["main".to_owned(), "one".to_owned(), "two".to_owned()])
        );
        // And they count as covered from here on.
        assert!(
            coverage
                .covered
                .read()
                .unwrap()
                .contains(&records[0].target.path)
        );
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn other_processes_writes_are_read_at_once_and_then_paced() {
        let scope = TaskScope::new();
        let (metadata, mut publisher) = leaf_channel::<crate::local_source::Metadata>(
            &scope,
            SourceSpec {
                name: "test-metadata",
                domain: Domain::Metadata,
                persistence: Persistence::Never("isolated test"),
                policy: RefreshPolicy::default(),
            },
        );
        let interval = Duration::from_millis(200);
        let paced = Paced::new(metadata.view(), interval);
        let asked =
            |publisher: &mut wt_runtime::LeafPublisher<_>| publisher.take_demand().whole.is_some();
        // A lone write is read at once.
        paced.signal();
        assert!(asked(&mut publisher));
        assert!(paced.due().is_none());
        // Writes that follow closely are held back, however many.
        for _ in 0..20 {
            paced.signal();
        }
        assert!(!asked(&mut publisher));
        let due = paced.due().expect("one read is held back");
        assert!(due <= tokio::time::Instant::now() + interval);
        paced.held.notified().await;
        // The held read goes out when its time comes, once.
        tokio::time::sleep_until(due).await;
        paced.release();
        assert!(asked(&mut publisher));
        paced.release();
        assert!(!asked(&mut publisher));
        // After a quiet interval the next write is read at once again.
        tokio::time::sleep(interval).await;
        paced.signal();
        assert!(asked(&mut publisher));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn a_file_git_rewrites_with_the_same_bytes_is_not_a_change() {
        let root = tempfile::tempdir().unwrap();
        let git_dir = std::fs::canonicalize(root.path()).unwrap();
        let config = git_dir.join("config");
        std::fs::write(&config, "[branch \"main\"]\n\tremote = origin\n").unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(git_dir.join("objects")).unwrap();
        // A working tree with a `config` directory of its own, and a file
        // that merely shares the name.
        let checkout = git_dir.join("checkout");
        std::fs::create_dir_all(checkout.join("config")).unwrap();
        let named = git_dir.join("named");
        std::fs::create_dir(&named).unwrap();
        std::fs::write(named.join("config"), "same").unwrap();
        let scope = TaskScope::new();
        fn spec<T>(name: &'static str, domain: Domain) -> SourceSpec<T> {
            SourceSpec {
                name,
                domain,
                persistence: Persistence::Never("isolated test"),
                policy: RefreshPolicy::default(),
            }
        }
        let (git, mut git_publisher) =
            leaf_channel::<Vec<WorktreeSnapshot>>(&scope, spec("test-git", Domain::Git));
        let (metadata, _metadata_publisher) = leaf_channel::<crate::local_source::Metadata>(
            &scope,
            spec("test-metadata", Domain::Metadata),
        );
        let router = Arc::new(Router {
            git: git.view(),
            metadata: Arc::new(Paced::new(metadata.view(), METADATA_PACE)),
            state_db: "/state/wt.sqlite".into(),
            checkout_roots: ["/repo/main".into(), "/wt".into()],
            layout: RwLock::default(),
            contents: Mutex::default(),
        });
        router
            .clone()
            .baseline(vec![git_dir.clone(), checkout.clone(), named.clone()])
            .await;
        // Only files in a Git directory are ever compared. A directory named
        // `config` in a checkout changes when a file in it is added, and
        // that is the only signal there is.
        for _ in 0..2 {
            router.invalidate(&[checkout.join("config")]);
            assert_eq!(git_publisher.take_demand().whole, Some(Request::Recheck));
            router.invalidate(&[named.join("config")]);
            assert_eq!(git_publisher.take_demand().whole, Some(Request::Recheck));
        }
        // What \`git branch --force\` does on every origin fetch: a new file
        // with the bytes the old one had.
        let rewrite = |bytes: &str| {
            let next = git_dir.join("config.lock");
            std::fs::write(&next, bytes).unwrap();
            std::fs::rename(&next, &config).unwrap();
        };
        rewrite("[branch \"main\"]\n\tremote = origin\n");
        router.invalidate(std::slice::from_ref(&config));
        assert!(git_publisher.take_demand().whole.is_none());
        // Alongside a path that did change, only that path counts.
        router.invalidate(&[config.clone(), git_dir.join("packed-refs.absent")]);
        assert_eq!(git_publisher.take_demand().whole, Some(Request::Recheck));
        // New bytes are a change, once.
        rewrite("[branch \"main\"]\n\tremote = upstream\n");
        router.invalidate(std::slice::from_ref(&config));
        assert_eq!(git_publisher.take_demand().whole, Some(Request::Recheck));
        router.invalidate(std::slice::from_ref(&config));
        assert!(git_publisher.take_demand().whole.is_none());
        // A file that is gone, unreadable or too large to compare is always
        // a change, however often it is reported.
        std::fs::remove_file(&config).unwrap();
        for _ in 0..2 {
            router.invalidate(std::slice::from_ref(&config));
            assert_eq!(git_publisher.take_demand().whole, Some(Request::Recheck));
        }
        let packed = git_dir.join("packed-refs");
        let large = std::fs::File::create(&packed).unwrap();
        large.set_len(REWRITTEN_LIMIT + 1).unwrap();
        for _ in 0..2 {
            router.invalidate(std::slice::from_ref(&packed));
            assert_eq!(git_publisher.take_demand().whole, Some(Request::Recheck));
        }
        // A file with no baseline is always a change.
        let elsewhere = tempfile::tempdir().unwrap();
        let other = elsewhere.path().join("config");
        std::fs::write(&other, "x").unwrap();
        router.invalidate(std::slice::from_ref(&other));
        assert_eq!(git_publisher.take_demand().whole, Some(Request::Recheck));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
