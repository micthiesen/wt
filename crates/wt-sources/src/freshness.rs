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
/// Spacing of walks of the whole watch set, from the start of the preceding
/// one. The walk is what finds a file nobody knew to look for: a new ref, a
/// change at the top of a working tree.
const SCAN_INTERVAL: Duration = Duration::from_secs(1);
/// A walk slower than `SCAN_INTERVAL / SCAN_DUTY` stretches the spacing to
/// this many times its own duration, so a large watch set is sampled less
/// often rather than abandoned, and walking never takes more than one part
/// in this many of a thread.
const SCAN_DUTY: u32 = 4;
/// How often the few files that carry a checkout's Git facts (its HEAD,
/// index, branch and upstream refs, operation state) are checked where
/// changes are only found by polling. They are a handful of stats per
/// checkout, so they can be looked at far more often than the set is walked.
const HOT_TICK: Duration = Duration::from_millis(125);
/// How many steps the rolling backstop takes through the fleet per
/// `BACKSTOP`.
const BACKSTOP_STEPS: u32 = 20;
/// A checkout nobody is working in (archived) is doubted this many times
/// less often.
const BACKSTOP_IDLE_FACTOR: u32 = 5;
/// The least spacing between reads of every checkout caused by changes that
/// cannot be routed. A burst of them is one doubt, not a sweep each.
const UNROUTED_PACE: Duration = Duration::from_secs(5);
/// When more rows are due than the usual step reads (all of them, a period
/// after a read of every checkout), they are cleared within this many steps
/// rather than a whole further period.
const BACKSTOP_CATCH_UP: usize = 4;
/// How often a scanner thread that died is started again before detection
/// is left to the backstop.
const SCANNER_RESTARTS: u32 = 5;
/// The wait before the first restart, doubled for each one after.
const SCANNER_BACKOFF: Duration = Duration::from_millis(250);
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
    /// Only Git's own transient lock files: nothing to read. The file a
    /// lock replaces is reported by its own path when the lock is renamed
    /// over it.
    Nothing,
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

    /// The first of `paths` that cannot be placed, for the log.
    fn unrouted(&self, paths: &[PathBuf], worktree_root: &Path) -> Option<PathBuf> {
        paths
            .iter()
            .find(|path| self.touch(std::slice::from_ref(*path), worktree_root) == Touch::All)
            .cloned()
    }

    /// The files whose change moves what a row shows: each checkout's HEAD,
    /// index and operation state, the refs of its branch and upstream, and
    /// the packed refs they may live in.
    fn hot_files(&self) -> Vec<PathBuf> {
        let mut files = BTreeSet::new();
        for row in &self.rows {
            if let Some(git_dir) = &row.git_dir {
                for name in [
                    "HEAD",
                    "index",
                    "MERGE_HEAD",
                    "rebase-merge",
                    "rebase-apply",
                ] {
                    files.insert(git_dir.join(name));
                }
            }
            let Some(common) = &row.common_dir else {
                continue;
            };
            files.insert(common.join("packed-refs"));
            if !row.branch.is_empty() {
                files.insert(common.join("refs/heads").join(&row.branch));
            }
            if let Some(upstream) = &row.upstream {
                files.insert(common.join("refs/remotes").join(upstream));
                files.insert(common.join("refs/heads").join(upstream));
            }
        }
        files.into_iter().collect()
    }

    /// Route Git-relevant paths to rows. Anything that cannot be placed
    /// reads everything: a wrong guess here would leave a row stale until
    /// the next sweep.
    fn touch(&self, paths: &[PathBuf], worktree_root: &Path) -> Touch {
        if paths.is_empty() {
            return Touch::All;
        }
        let mut keys = BTreeSet::new();
        let mut routed = false;
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
                // Git writes X by creating X.lock and renaming it over X.
                // Where changes are reported as they happen, the lock shows
                // up beside the file: alone, and again paired with it in the
                // rename.
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".lock"))
                {
                    continue;
                }
                routed = true;
                let relative = path.strip_prefix(common).unwrap_or(path);
                let mut parts = relative.components().map(|part| part.as_os_str());
                let mut first = parts.next().and_then(|part| part.to_str());
                // A reflog changes with the ref or HEAD it logs, and reaches
                // what that does.
                if first == Some("logs") {
                    first = parts.next().and_then(|part| part.to_str());
                    if first.is_none() {
                        continue;
                    }
                }
                match first {
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
            routed = true;
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
        if !routed {
            return Touch::Nothing;
        }
        Touch::Rows(keys)
    }
}

/// Passes a request on no more often than `interval`: the first at once, and
/// one more when the interval has passed if anything asked meanwhile. For
/// requests that arrive in bursts and each cost a whole read.
struct Paced {
    ask: Box<dyn Fn() + Send + Sync>,
    interval: Duration,
    /// When the last read was asked for, and whether another is held back.
    state: Mutex<(Option<std::time::Instant>, bool)>,
    held: tokio::sync::Notify,
}

impl Paced {
    fn new(ask: impl Fn() + Send + Sync + 'static, interval: Duration) -> Self {
        Self {
            ask: Box::new(ask),
            interval,
            state: Mutex::default(),
            held: tokio::sync::Notify::new(),
        }
    }

    /// Metadata reads for other processes' writes to the state database.
    fn metadata(metadata: SourceHandle<crate::local_source::Metadata>) -> Self {
        Self::new(
            move || {
                metadata.refresh();
            },
            METADATA_PACE,
        )
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
            (self.ask)();
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
            (self.ask)();
        }
    }
}

/// Turns changed paths into source requests.
struct Router {
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    /// Metadata reads for other processes' writes to the state database.
    metadata: Arc<Paced>,
    metadata_now: SourceHandle<crate::local_source::Metadata>,
    /// Reads of every checkout for changes that cannot be routed.
    sweeps: Arc<Paced>,
    state_db: PathBuf,
    /// The main clone and the worktree root.
    checkout_roots: [PathBuf; 2],
    layout: Arc<RwLock<Layout>>,
    contents: Mutex<Contents>,
    /// Paths a notification was routed for, as they looked then. The scan
    /// finds the same change a moment later and must not route it twice.
    reported: Mutex<BTreeMap<PathBuf, Option<FileSignature>>>,
    /// When each row was last asked for, by a change or by the backstop.
    asked: Arc<Mutex<BTreeMap<String, std::time::Instant>>>,
    unrouted_log: RateGate,
}

/// Enough for any burst between two scans; past it the scan may route a
/// change a second time, which costs a read and loses nothing.
const REPORTED_LIMIT: usize = 4096;

/// Files of a Git directory that every checkout resolves refs or upstreams
/// through, so a change to one could reach any row. Git rewrites them whole
/// for a change to one entry (and often for none: the config on every
/// `git branch --force`), so what changed is found by comparing their
/// entries, and only the rows those entries name are read.
const REWRITTEN_FILES: [&str; 2] = ["config", "packed-refs"];

/// The largest of those files that is read to compare. A larger one (packed
/// refs of a very large repository) always counts as changed throughout.
const REWRITTEN_LIMIT: u64 = 4 * 1024 * 1024;

/// What those files held when last read, for the Git directories among the
/// watch roots. Only a file that was read both times can be compared: a path
/// that is missing, unreadable, too large or not a regular file has no
/// entry, and a change to it is a change to everything it could hold.
#[derive(Default)]
struct Contents {
    /// Watch roots already looked at, and those of them that are Git
    /// directories. A working tree may hold a `config` of its own (a
    /// directory, in many projects); that is never one of these files.
    examined: BTreeSet<PathBuf>,
    git_dirs: BTreeSet<PathBuf>,
    /// Per file, its entries: a config's sections by header, packed refs by
    /// name.
    entries: BTreeMap<PathBuf, BTreeMap<String, String>>,
}

fn rewritten_entries(path: &Path) -> Option<BTreeMap<String, String>> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > REWRITTEN_LIMIT {
        return None;
    }
    let text = String::from_utf8(std::fs::read(path).ok()?).ok()?;
    let mut entries = BTreeMap::<String, String>::new();
    if path.file_name().is_some_and(|name| name == "config") {
        let mut section = String::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                section = trimmed.to_owned();
                entries.entry(section.clone()).or_default();
            } else {
                let body = entries.entry(section.clone()).or_default();
                body.push_str(trimmed);
                body.push('\n');
            }
        }
    } else {
        let mut last = String::new();
        for line in text.lines() {
            // A peeled value belongs to the tag above it; anything else
            // that is not a ref (the header) is an entry of its own, so a
            // change to it is not mistaken for a change to refs alone.
            match line.strip_prefix('^') {
                Some(peeled) => entries.entry(last.clone()).or_default().push_str(peeled),
                None => {
                    let (value, name) = line.split_once(' ').unwrap_or((line, line));
                    last = name.to_owned();
                    entries.insert(last.clone(), value.to_owned());
                }
            }
        }
    }
    Some(entries)
}

/// The ref a changed entry of a rewritten file is about: a packed ref
/// itself, or the branch a config section configures (its upstream).
fn entry_ref(file: &Path, entry: &str) -> Option<String> {
    if file.file_name().is_some_and(|name| name == "config") {
        let name = entry.strip_suffix("\"]")?;
        let (kind, name) = name.split_once(" \"")?;
        return kind
            .eq_ignore_ascii_case("[branch")
            .then(|| format!("refs/heads/{name}"));
    }
    entry.starts_with("refs/").then(|| entry.to_owned())
}

/// What a reported change to a path amounts to.
enum Rewrite {
    /// Not one of the rewritten files, or one that cannot be compared: the
    /// path stands for itself.
    Path,
    /// Rewritten with the entries it already had.
    Same,
    /// Only these refs differ.
    Refs(Vec<String>),
}

/// Whether a watch root is a Git directory (a repository's own, or a linked
/// checkout's under `worktrees/`), as opposed to a working tree.
fn is_git_dir(root: &Path) -> bool {
    root.join("HEAD").is_file()
        && (root.join("objects").is_dir() || root.join("commondir").is_file())
}

impl Router {
    fn new(
        git: SourceHandle<Vec<WorktreeSnapshot>>,
        metadata_now: SourceHandle<crate::local_source::Metadata>,
        metadata: Arc<Paced>,
        state_db: PathBuf,
        checkout_roots: [PathBuf; 2],
        unrouted_pace: Duration,
    ) -> Self {
        let layout = Arc::new(RwLock::new(Layout::default()));
        let asked = Arc::new(Mutex::new(BTreeMap::new()));
        Self {
            metadata,
            metadata_now,
            sweeps: Arc::new(Paced::new(
                {
                    let (git, layout, asked) = (git.clone(), layout.clone(), asked.clone());
                    move || {
                        // Rows count as asked for when the read of all of
                        // them really is, not when the change was seen: a
                        // read held back by the pace must not keep the
                        // backstop off rows nothing has read.
                        if let (Ok(layout), Ok(mut asked)) = (layout.read(), asked.lock()) {
                            let now = std::time::Instant::now();
                            asked.extend(layout.rows.iter().map(|row| (row.key.clone(), now)));
                        }
                        git.request(Request::Recheck);
                    }
                },
                unrouted_pace,
            )),
            git,
            state_db,
            checkout_roots,
            layout,
            contents: Mutex::default(),
            reported: Mutex::default(),
            asked,
            unrouted_log: RateGate::new(unrouted_pace),
        }
    }

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
            type Entries = Vec<(PathBuf, BTreeMap<String, String>)>;
            let read: Vec<(PathBuf, Entries)> = new
                .into_iter()
                .filter(|root| is_git_dir(root))
                .map(|root| {
                    let entries = REWRITTEN_FILES
                        .iter()
                        .map(|name| root.join(name))
                        .filter_map(|path| rewritten_entries(&path).map(|entries| (path, entries)))
                        .collect();
                    (root, entries)
                })
                .collect();
            if let Ok(mut contents) = self.contents.lock() {
                for (root, entries) in read {
                    contents.git_dirs.insert(root);
                    contents.entries.extend(entries);
                }
            }
        })
        .await;
    }

    /// Compare a changed path with what it held, if it is one of the
    /// rewritten files of a Git directory.
    fn rewritten(&self, path: &Path) -> Rewrite {
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| REWRITTEN_FILES.contains(&name))
        {
            return Rewrite::Path;
        }
        let in_git_dir = self.contents.lock().is_ok_and(|contents| {
            path.parent()
                .is_some_and(|parent| contents.git_dirs.contains(parent))
        });
        if !in_git_dir {
            return Rewrite::Path;
        }
        let now = rewritten_entries(path);
        let Ok(mut contents) = self.contents.lock() else {
            return Rewrite::Path;
        };
        let Some(now) = now else {
            contents.entries.remove(path);
            return Rewrite::Path;
        };
        let Some(then) = contents.entries.insert(path.to_path_buf(), now.clone()) else {
            return Rewrite::Path;
        };
        let refs: Option<Vec<String>> = then
            .keys()
            .chain(now.keys())
            .filter(|entry| then.get(*entry) != now.get(*entry))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|entry| entry_ref(path, entry))
            .collect();
        match refs {
            Some(refs) if refs.is_empty() => Rewrite::Same,
            Some(refs) => Rewrite::Refs(refs),
            // Something other than a ref or a branch's settings changed.
            None => Rewrite::Path,
        }
    }

    /// Route paths a notification named, remembering how they look now.
    fn notified(&self, paths: &[PathBuf]) {
        if let Ok(mut reported) = self.reported.lock() {
            if reported.len() + paths.len() > REPORTED_LIMIT {
                reported.clear();
            }
            for path in paths {
                let now = std::fs::symlink_metadata(path)
                    .ok()
                    .map(|metadata| FileSignature::from_metadata(&metadata));
                reported.insert(path.clone(), now);
            }
        }
        self.invalidate(paths);
    }

    /// Whether a change the scan found, to a path that now looks like `now`,
    /// is one a notification has already been routed for. Asked once per
    /// change: the answer is forgotten with it.
    fn already_notified(&self, path: &Path, now: Option<&FileSignature>) -> bool {
        self.reported
            .lock()
            .ok()
            .and_then(|mut reported| reported.remove(path))
            .is_some_and(|then| then.as_ref() == now)
    }

    fn invalidate(&self, paths: &[PathBuf]) {
        let mut changed = Vec::with_capacity(paths.len());
        for path in paths {
            match self.rewritten(path) {
                Rewrite::Path => changed.push(path.clone()),
                Rewrite::Same => {}
                // Routed as the loose refs would be, in the same directory.
                Rewrite::Refs(refs) => {
                    let directory = path.parent().unwrap_or(path);
                    changed.extend(refs.iter().map(|name| directory.join(name)));
                }
            }
        }
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
        let worktree_root = &self.checkout_roots[1];
        let (touch, unrouted) = match self.layout.read() {
            Ok(layout) => {
                let touch = layout.touch(&git_paths, worktree_root);
                let unrouted = (touch == Touch::All)
                    .then(|| layout.unrouted(&git_paths, worktree_root))
                    .flatten();
                (touch, unrouted)
            }
            Err(_) => (Touch::All, None),
        };
        match touch {
            Touch::Nothing => {}
            Touch::All => {
                // Without this a fleet-wide read has no visible cause.
                if self.unrouted_log.allow() {
                    tracing::info!(
                        path = ?unrouted,
                        paths = git_paths.len(),
                        "a change that cannot be routed to its checkouts reads all of them"
                    );
                }
                self.sweeps.signal();
            }
            Touch::Rows(keys) => {
                if let Ok(mut asked) = self.asked.lock() {
                    let now = std::time::Instant::now();
                    asked.extend(keys.iter().map(|key| (key.clone(), now)));
                }
                self.git.request_keys(Request::Recheck, keys);
            }
        }
    }

    /// The rows the rolling backstop reads at this step: those not asked for
    /// within their period, oldest first. It doubts what no detector can see
    /// (edits deep in a working tree), so a row a change was just routed to
    /// has nothing more to gain from it yet. A step reads a twentieth of the
    /// fleet, or more when that would leave the due rows waiting.
    fn backstop(&self, idle: &BTreeSet<String>, now: std::time::Instant) -> Vec<String> {
        let Ok(layout) = self.layout.read() else {
            return Vec::new();
        };
        let Ok(mut asked) = self.asked.lock() else {
            return Vec::new();
        };
        asked.retain(|key, _| layout.rows.iter().any(|row| row.key == *key));
        let mut due = Vec::new();
        for row in &layout.rows {
            // A row seen for the first time has just been read.
            let last = *asked.entry(row.key.clone()).or_insert(now);
            let period = if idle.contains(&row.key) {
                BACKSTOP * BACKSTOP_IDLE_FACTOR
            } else {
                BACKSTOP
            };
            if now.duration_since(last) >= period {
                due.push((last, row.key.clone()));
            }
        }
        due.sort();
        let step = layout.rows.len().div_ceil(BACKSTOP_STEPS as usize).max(1);
        due.truncate(step.max(due.len().div_ceil(BACKSTOP_CATCH_UP)));
        for (_, key) in &due {
            asked.insert(key.clone(), now);
        }
        due.into_iter().map(|(_, key)| key).collect()
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
    let paced = Arc::new(Paced::metadata(metadata.clone()));
    let signals = (!notifications).then(|| {
        let database = config.paths.state_db.clone();
        let mut wal = database.clone().into_os_string();
        wal.push("-wal");
        let paced = paced.clone();
        signal_tick(scope, vec![database, wal.into()], move || paced.signal())
    });
    let plan = Arc::new(Mutex::new(ScanPlan::default()));
    let mut scans = Scanner::start(scope.token(), plan.clone());
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
        let router = Arc::new(Router::new(
            git.clone(),
            metadata.clone(),
            paced.clone(),
            state_db.clone(),
            [
                canonical_watch_path(&config.paths.main_clone).await,
                canonical_watch_path(&config.paths.worktree_root).await,
            ],
            UNROUTED_PACE,
        ));

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
                            callback_router.notified(&paths);
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
                            callback_router.metadata_now.refresh();
                            callback_rewatch.notify_one();
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "filesystem notification lost; refreshing source");
                        callback_router.git.refresh();
                        callback_router.metadata_now.refresh();
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
        let mut backstop_step = tokio::time::interval(BACKSTOP / BACKSTOP_STEPS);
        backstop_step.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Checkouts nobody is working in, doubted less often.
        let mut idle = BTreeSet::<String>::new();
        let mut scans_open = true;
        let mut scan_slow = false;
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
                        {
                            let mut plan = plan.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            plan.roots.clone_from(&watch.registered);
                        }
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
            let sweep_due = router.sweeps.due();
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
                        idle = board.rows.iter()
                            .filter(|row| row.host.is_none() && row.archived)
                            .map(|row| row.key.clone())
                            .collect();
                        let live_keys: BTreeSet<_> = board.rows.iter().map(|row| row.key.as_str()).collect();
                        latest_edits.retain(|slug, _| live_keys.contains(slug.as_str()));
                    }
                }
                changed = git_updates.changed() => {
                    if changed.is_err() { break; }
                    let snapshot = git_updates.borrow_and_update().clone();
                    if let Some(rows) = snapshot.data() {
                        let layout = Layout::new(rows);
                        // With notifications the files report themselves.
                        if watch.watcher.is_none() {
                            plan.lock().unwrap_or_else(std::sync::PoisonError::into_inner).hot =
                                layout.hot_files();
                        }
                        if let Ok(mut current) = router.layout.write() {
                            *current = layout;
                        }
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
                            {
                            let mut plan = plan.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                plan.roots.clone_from(&watch.registered);
                            }
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
                scanned = scans.next(), if scans_open => {
                    let Some(scanned) = scanned else {
                        scans_open = false;
                        continue;
                    };
                    let (mut changed, hot) = match scanned {
                        // Found between walks, at one of the files that carry
                        // a checkout's Git facts. The next walk finds the same
                        // change; routing it as notified keeps that from
                        // counting twice.
                        Scanned::Hot(paths) => (paths, true),
                        Scanned::Walk { roots, sample, walk } => {
                            if walk > SCAN_INTERVAL && !scan_slow {
                                scan_slow = true;
                                tracing::warn!(
                                    roots = roots.len(),
                                    entries = sample.len(),
                                    walk_ms = walk.as_millis() as u64,
                                    "walking the watched files has become slow; changes outside the checked files are noticed that much later"
                                );
                            } else if walk <= SCAN_INTERVAL / 2 && scan_slow {
                                scan_slow = false;
                                tracing::info!(
                                    walk_ms = walk.as_millis() as u64,
                                    "walking the watched files is quick again"
                                );
                            }
                            let entries = sample.len();
                            let mut changed = watch.settle(&roots, sample);
                            // A signal file the tick has already reported as it is now.
                            if let Some(signals) = &signals
                                && let (Ok(seen), Some(now)) = (signals.lock(), watch.fingerprint.as_deref())
                            {
                                changed.retain(|path| seen.get(path).is_none_or(|seen| seen.as_ref() != now.get(path)));
                            }
                            if let Some(now) = watch.fingerprint.as_deref() {
                                changed.retain(|path| !router.already_notified(path, now.get(path)));
                            }
                            tracing::debug!(entries, walk_ms = walk.as_millis() as u64, ?changed, "filesystem scan");
                            (changed, false)
                        }
                    };
                    changed.retain(|path| relevant_path(path));
                    if changed.is_empty() {
                        continue;
                    }
                    // Routing may read a rewritten file to see what changed
                    // in it.
                    let routing = (router.clone(), changed.clone());
                    let _ = tokio::task::spawn_blocking(move || {
                        if hot {
                            routing.0.notified(&routing.1);
                        } else {
                            routing.0.invalidate(&routing.1);
                        }
                    })
                    .await;
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
                _ = router.sweeps.held.notified() => {}
                _ = tokio::time::sleep_until(sweep_due.unwrap_or_else(tokio::time::Instant::now)), if sweep_due.is_some() => {
                    router.sweeps.release();
                }
                // What no detector sees (an edit deep in a working tree) is
                // found by reading the checkouts again, a few at a time and
                // round the fleet once per period, rather than all at once:
                // forty statuses in one burst every minute cost seconds of
                // every core with nobody asking.
                _ = backstop_step.tick() => {
                    let due = router.backstop(&idle, std::time::Instant::now());
                    if !due.is_empty() {
                        git.request_rows(Request::Recheck, due);
                    }
                }
                // The listing itself is doubted once a period: a checkout
                // made or removed with no event is found here.
                _ = backstop.tick() => {
                    git.request_keys(Request::Recheck, Vec::<String>::new());
                    metadata.refresh();
                    reconcile = true;
                }
            }
        }
        scans.joined().await;
        wt_app_core::watcher_cleanup::drop_watcher(watch.watcher).await;
    });
    edits
}

/// What the scanner thread looks at: the watch roots it walks, and the files
/// it checks between walks.
#[derive(Default)]
struct ScanPlan {
    roots: BTreeMap<PathBuf, bool>,
    hot: Vec<PathBuf>,
    /// Test seam: the thread panics once at its next look.
    #[cfg(test)]
    fail: bool,
}

enum Scanned {
    /// A walk of `roots`, and how long the walk itself took.
    Walk {
        roots: BTreeMap<PathBuf, bool>,
        sample: WatchFingerprint,
        walk: Duration,
    },
    /// Checked files that changed since the thread last looked.
    Hot(Vec<PathBuf>),
}

/// Samples the watch set on a thread of its own. The work is a few thousand
/// stats, tens of milliseconds, but on the shared blocking pool it queued
/// behind whatever else was there (every status sweep, every file read), and
/// a change was then noticed seconds late. Nothing here waits on the runtime
/// except to hand over a result.
///
/// Where polling is the only detector this thread is all of detection, so
/// its death is logged and it is started again, a bounded number of times.
struct Scanner {
    plan: Arc<Mutex<ScanPlan>>,
    cancellation: tokio_util::sync::CancellationToken,
    results: mpsc::Receiver<Scanned>,
    /// Resolves when the current thread has ended.
    stopped: oneshot::Receiver<()>,
    restarts: u32,
    restart_at: Option<tokio::time::Instant>,
}

impl Scanner {
    fn start(
        cancellation: tokio_util::sync::CancellationToken,
        plan: Arc<Mutex<ScanPlan>>,
    ) -> Self {
        let (results, stopped) = spawn_scanner(&cancellation, plan.clone());
        Self {
            plan,
            cancellation,
            results,
            stopped,
            restarts: 0,
            restart_at: None,
        }
    }

    /// The next thing the thread found. `None` once the scope is cancelled,
    /// or the thread has died more often than it is restarted. Safe to drop
    /// and call again: a restart that was due stays due.
    async fn next(&mut self) -> Option<Scanned> {
        loop {
            match self.results.recv().await {
                Some(scanned) => {
                    if matches!(scanned, Scanned::Walk { .. }) {
                        self.restarts = 0;
                    }
                    return Some(scanned);
                }
                None if self.cancellation.is_cancelled() => return None,
                None => {}
            }
            let at = match self.restart_at {
                Some(at) => at,
                None => {
                    if self.restarts >= SCANNER_RESTARTS {
                        tracing::error!(
                            restarts = self.restarts,
                            "the filesystem scanner keeps stopping; changes are found only by the periodic re-read of each checkout"
                        );
                        return None;
                    }
                    let wait = SCANNER_BACKOFF * 2u32.pow(self.restarts);
                    self.restarts += 1;
                    tracing::warn!(
                        restart = self.restarts,
                        wait_ms = wait.as_millis() as u64,
                        "the filesystem scanner stopped; starting it again"
                    );
                    *self.restart_at.insert(tokio::time::Instant::now() + wait)
                }
            };
            tokio::select! {
                _ = self.cancellation.cancelled() => return None,
                _ = tokio::time::sleep_until(at) => {}
            }
            self.restart_at = None;
            (self.results, self.stopped) = spawn_scanner(&self.cancellation, self.plan.clone());
        }
    }

    /// Wait for the thread to end, after cancellation.
    async fn joined(self) {
        let _ = self.stopped.await;
    }
}

fn spawn_scanner(
    cancellation: &tokio_util::sync::CancellationToken,
    plan: Arc<Mutex<ScanPlan>>,
) -> (mpsc::Receiver<Scanned>, oneshot::Receiver<()>) {
    let (results, receiver) = mpsc::channel(4);
    let (stopped, joined) = oneshot::channel::<()>();
    let cancellation = cancellation.clone();
    let thread = std::thread::Builder::new()
        .name("wt-fs-scan".into())
        .spawn(move || {
            let _stopped = stopped;
            let mut seen = BTreeMap::<PathBuf, Option<FileSignature>>::new();
            let mut next_walk = std::time::Instant::now();
            while !cancellation.is_cancelled() {
                // A plan another thread panicked while holding is still a
                // plan; giving up on it would end detection.
                #[cfg(test)]
                {
                    let fail = std::mem::take(
                        &mut plan
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .fail,
                    );
                    assert!(!fail, "scanner failure injected by a test");
                }
                let (roots, hot) = {
                    let plan = plan
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    (plan.roots.clone(), plan.hot.clone())
                };
                let mut changed = Vec::new();
                if seen.len() != hot.len() || !hot.iter().all(|path| seen.contains_key(path)) {
                    seen.retain(|path, _| hot.binary_search(path).is_ok());
                }
                for path in hot {
                    let now = std::fs::symlink_metadata(&path)
                        .ok()
                        .map(|metadata| FileSignature::from_metadata(&metadata));
                    match seen.get(&path) {
                        Some(then) if *then == now => {}
                        // The first look at a file is what later ones are
                        // compared with. The walk covers the time before it.
                        None => {
                            seen.insert(path, now);
                        }
                        Some(_) => {
                            seen.insert(path.clone(), now);
                            changed.push(path);
                        }
                    }
                }
                if !changed.is_empty() && results.blocking_send(Scanned::Hot(changed)).is_err() {
                    break;
                }
                let started = std::time::Instant::now();
                if started >= next_walk {
                    let Some(sample) = watch_fingerprint(&roots, &cancellation) else {
                        break;
                    };
                    let walk = started.elapsed();
                    next_walk = started + SCAN_INTERVAL.max(walk * SCAN_DUTY);
                    let walked = Scanned::Walk {
                        roots,
                        sample,
                        walk,
                    };
                    if results.blocking_send(walked).is_err() {
                        break;
                    }
                }
                std::thread::sleep(HOT_TICK);
            }
        });
    if let Err(error) = thread {
        tracing::warn!(%error, "cannot start the filesystem scanner");
    }
    (receiver, joined)
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
    config: &Arc<Config>,
    inventory: &[WorktreeRecord],
) -> Result<(BTreeMap<PathBuf, bool>, bool)> {
    // Several file checks per checkout: one hop to the blocking pool for all
    // of them, not one each behind whatever else is queued there.
    let (config, inventory) = (config.clone(), inventory.to_vec());
    Ok(tokio::task::spawn_blocking(move || desired_paths_blocking(&config, &inventory)).await?)
}

fn desired_paths_blocking(
    config: &Config,
    inventory: &[WorktreeRecord],
) -> (BTreeMap<PathBuf, bool>, bool) {
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
            if path_may_exist_blocking(&path) {
                paths.insert(path, true);
            }
        }
    }
    for worktree in inventory {
        let checkout = PathBuf::from(&worktree.target.path);
        // A deleted checkout cannot be watched; requesting it would only
        // repeat the backend's missing-path error.
        if !path_may_exist_blocking(&checkout) {
            missing = true;
            continue;
        }
        paths.insert(checkout, false);
        if let Some(directory) = &worktree.git_dir
            && !common_dirs
                .iter()
                .any(|common| directory.starts_with(common.join("worktrees")))
            && path_may_exist_blocking(directory)
        {
            paths.insert(directory.clone(), false);
            for child in ["refs", "worktrees"] {
                let path = directory.join(child);
                if path_may_exist_blocking(&path) {
                    paths.insert(path, true);
                }
            }
        }
    }
    // Canonicalize watch paths so aliases such as /var and /private/var do not
    // create duplicate watches or miss events during path matching.
    let mut canonical = BTreeMap::new();
    for (path, recursive) in paths {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        canonical
            .entry(path)
            .and_modify(|value| *value |= recursive)
            .or_insert(recursive);
    }
    (canonical, missing)
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
fn path_may_exist_blocking(path: &std::path::Path) -> bool {
    path.try_exists().unwrap_or(true)
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
        // Git's lock files, which notifications report beside the file each
        // replaces, reach nothing; paired with that file in a rename, only
        // the file counts.
        for lock in [
            "/repo/main/.git/config.lock",
            "/repo/main/.git/index.lock",
            "/repo/main/.git/packed-refs.lock",
            "/repo/main/.git/refs/remotes/origin/feature/one.lock",
            "/repo/main/.git/worktrees/one/index.lock",
        ] {
            assert_eq!(touch(lock), Touch::Nothing, "{lock}");
        }
        let renamed =
            |lock: &str, file: &str| layout.touch(&[lock.into(), file.into()], Path::new("/wt"));
        assert_eq!(
            renamed(
                "/repo/main/.git/refs/remotes/origin/feature/one.lock",
                "/repo/main/.git/refs/remotes/origin/feature/one"
            ),
            rows(&["one"])
        );
        assert_eq!(
            renamed("/repo/main/.git/index.lock", "/repo/main/.git/index"),
            rows(&["main"])
        );
        assert_eq!(
            renamed(
                "/repo/main/.git/worktrees/two/HEAD.lock",
                "/repo/main/.git/worktrees/two/HEAD"
            ),
            rows(&["two"])
        );
        assert_eq!(
            renamed("/repo/main/.git/config.lock", "/repo/main/.git/config"),
            Touch::All
        );
        // A lock file in a working tree is a project's own file.
        assert_eq!(touch("/wt/one/Cargo.lock"), rows(&["one"]));
        // A reflog reaches what the ref or HEAD it logs does.
        assert_eq!(
            touch("/repo/main/.git/logs/refs/remotes/origin/feature/one"),
            rows(&["one"])
        );
        assert_eq!(
            touch("/repo/main/.git/logs/refs/heads/feature/two"),
            rows(&["two"])
        );
        assert_eq!(
            touch("/repo/main/.git/logs/refs/remotes/origin/other"),
            rows(&[])
        );
        assert_eq!(touch("/repo/main/.git/logs/HEAD"), rows(&["main"]));
        assert_eq!(touch("/repo/main/.git/logs"), rows(&[]));
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
        // The tick's first look, whenever a loaded machine gets to it.
        let baseline = std::fs::canonicalize(&database).unwrap();
        while !seen.lock().unwrap().contains_key(&baseline) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
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
        tokio::time::sleep(SIGNAL_TICK * 4).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "an unchanged file is not reported again"
        );
        let wal = std::fs::canonicalize(&wal).unwrap();
        assert!(seen.lock().unwrap().get(&wal).is_some_and(Option::is_some));
        scope.shutdown(Duration::from_secs(2)).await.unwrap();
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
        // Long enough that a stalled test thread cannot outlast it between
        // two lines.
        let interval = Duration::from_secs(1);
        let paced = Paced::new(
            {
                let metadata = metadata.view();
                move || {
                    metadata.refresh();
                }
            },
            interval,
        );
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
        let router = Arc::new(Router::new(
            git.view(),
            metadata.view(),
            Arc::new(Paced::metadata(metadata.view())),
            "/state/wt.sqlite".into(),
            ["/repo/main".into(), "/wt".into()],
            Duration::ZERO,
        ));
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

    /// The paths native notifications deliver for the two commands a fetch
    /// and its ref maintenance run, per inotify and Git's lockfile protocol
    /// (create X.lock, write it, rename it over X): the lock alone, the file
    /// alone, and the pair for the rename. Routing them must cost what the
    /// polling scan's one report of X costs.
    #[tokio::test]
    async fn a_fetch_reported_by_notifications_reads_only_the_row_it_moved() {
        let root = tempfile::tempdir().unwrap();
        let git_dir = std::fs::canonicalize(root.path()).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(git_dir.join("objects")).unwrap();
        let config = git_dir.join("config");
        let config_lock = git_dir.join("config.lock");
        std::fs::write(&config, "[branch \"feature/one\"]\n\tremote = origin\n").unwrap();
        let reference = git_dir.join("refs/remotes/origin/feature/one");
        let reference_lock = git_dir.join("refs/remotes/origin/feature/one.lock");
        std::fs::create_dir_all(reference.parent().unwrap()).unwrap();
        std::fs::write(&reference, "a\n").unwrap();
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
        let common = git_dir.clone();
        let row = |key: &str, git_dir: PathBuf, branch: &str, upstream: Option<&str>| LayoutRow {
            key: key.into(),
            path: format!("/wt/{key}").into(),
            git_dir: Some(git_dir),
            common_dir: Some(common.clone()),
            branch: branch.into(),
            upstream: upstream.map(Into::into),
        };
        let router = Arc::new(Router::new(
            git.view(),
            metadata.view(),
            Arc::new(Paced::metadata(metadata.view())),
            "/state/wt.sqlite".into(),
            ["/wt/main".into(), "/wt".into()],
            Duration::ZERO,
        ));
        *router.layout.write().unwrap() = Layout {
            rows: vec![
                row("main", git_dir.clone(), "main", None),
                row(
                    "one",
                    git_dir.join("worktrees/one"),
                    "feature/one",
                    Some("origin/feature/one"),
                ),
                row("two", git_dir.join("worktrees/two"), "feature/two", None),
            ],
        };
        router.clone().baseline(vec![git_dir.clone()]).await;
        let replace = |lock: &Path, file: &Path, bytes: &str| {
            std::fs::write(lock, bytes).unwrap();
            std::fs::rename(lock, file).unwrap();
        };
        let mut demand = |router: &Router, events: Vec<Vec<&PathBuf>>| {
            for event in events {
                let paths: Vec<PathBuf> = event.into_iter().cloned().collect();
                router.notified(&paths);
            }
            git_publisher.take_demand()
        };
        let only_one = BTreeMap::from([("one".to_owned(), Request::Recheck)]);

        // git update-ref refs/remotes/origin/feature/one: the row tracking
        // it, and no other.
        replace(&reference_lock, &reference, "b\n");
        let asked = demand(
            &router,
            vec![
                vec![&reference_lock],
                vec![&reference],
                vec![&reference_lock, &reference],
            ],
        );
        assert_eq!((asked.whole, asked.keys), (None, only_one.clone()));

        // git config, rewriting the bytes it already had: nothing at all,
        // whichever of the rename's events is looked at.
        replace(
            &config_lock,
            &config,
            "[branch \"feature/one\"]\n\tremote = origin\n",
        );
        let asked = demand(
            &router,
            vec![
                vec![&config_lock],
                vec![&config],
                vec![&config_lock, &config],
            ],
        );
        assert_eq!(asked, wt_runtime::Demand::default());

        // A change to that branch's own settings reaches the rows on it,
        // once; the pair that follows the rename adds nothing.
        replace(
            &config_lock,
            &config,
            "[branch \"feature/one\"]\n\tremote = fork\n",
        );
        let asked = demand(
            &router,
            vec![
                vec![&config_lock],
                vec![&config],
                vec![&config_lock, &config],
            ],
        );
        assert_eq!(asked.whole, None);
        assert_eq!(
            asked.keys,
            BTreeMap::from([("one".to_owned(), Request::Recheck)])
        );
        // A change to anything else in the config still reads every row.
        replace(
            &config_lock,
            &config,
            "[core]\n\tbare = false\n[branch \"feature/one\"]\n\tremote = fork\n",
        );
        let asked = demand(
            &router,
            vec![
                vec![&config_lock],
                vec![&config],
                vec![&config_lock, &config],
            ],
        );
        assert_eq!(asked.whole, Some(Request::Recheck));
        let asked = demand(&router, vec![vec![&config_lock, &config]]);
        assert_eq!(asked, wt_runtime::Demand::default());

        // The scan then finds the same files changed. What a notification
        // was routed for, as the file still is, is not routed again; a
        // change since then is.
        let signature = |path: &Path| {
            std::fs::symlink_metadata(path)
                .ok()
                .map(|metadata| FileSignature::from_metadata(&metadata))
        };
        assert!(router.already_notified(&reference, signature(&reference).as_ref()));
        assert!(!router.already_notified(&reference, signature(&reference).as_ref()));
        router.notified(std::slice::from_ref(&reference));
        replace(&reference_lock, &reference, "c\n");
        assert!(!router.already_notified(&reference, signature(&reference).as_ref()));
        assert!(!router.already_notified(&git_dir.join("HEAD"), None));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    fn routed(
        scope: &TaskScope,
    ) -> (
        Arc<Router>,
        wt_runtime::LeafPublisher<Vec<WorktreeSnapshot>>,
    ) {
        fn spec<T>(name: &'static str, domain: Domain) -> SourceSpec<T> {
            SourceSpec {
                name,
                domain,
                persistence: Persistence::Never("isolated test"),
                policy: RefreshPolicy::default(),
            }
        }
        let (git, git_publisher) =
            leaf_channel::<Vec<WorktreeSnapshot>>(scope, spec("test-git", Domain::Git));
        let (metadata, _) = leaf_channel::<crate::local_source::Metadata>(
            scope,
            spec("test-metadata", Domain::Metadata),
        );
        let router = Arc::new(Router::new(
            git.view(),
            metadata.view(),
            Arc::new(Paced::metadata(metadata.view())),
            "/state/wt.sqlite".into(),
            ["/wt/main".into(), "/wt".into()],
            Duration::ZERO,
        ));
        (router, git_publisher)
    }

    #[tokio::test]
    async fn a_packed_refs_rewrite_reaches_only_the_rows_whose_refs_moved() {
        let root = tempfile::tempdir().unwrap();
        let git_dir = std::fs::canonicalize(root.path()).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(git_dir.join("objects")).unwrap();
        let packed = git_dir.join("packed-refs");
        let header = "# pack-refs with: peeled fully-peeled sorted \n";
        let refs = |one: &str, backup: &str| {
            format!(
                "{header}aaaa refs/heads/feature/two\n{backup} refs/heads/backup/x\n{one} refs/remotes/origin/feature/one\ncccc refs/tags/v1\n^dddd\n"
            )
        };
        std::fs::write(&packed, refs("1111", "2222")).unwrap();
        let scope = TaskScope::new();
        let (router, mut git_publisher) = routed(&scope);
        let row = |key: &str, branch: &str, upstream: Option<&str>| LayoutRow {
            key: key.into(),
            path: format!("/wt/{key}").into(),
            git_dir: Some(git_dir.join("worktrees").join(key)),
            common_dir: Some(git_dir.clone()),
            branch: branch.into(),
            upstream: upstream.map(Into::into),
        };
        *router.layout.write().unwrap() = Layout {
            rows: vec![
                row("one", "feature/one", Some("origin/feature/one")),
                row("two", "feature/two", None),
            ],
        };
        router.clone().baseline(vec![git_dir.clone()]).await;
        let mut rewritten = |text: String| {
            std::fs::write(&packed, text).unwrap();
            router.invalidate(std::slice::from_ref(&packed));
            git_publisher.take_demand()
        };
        // A fetch moved the upstream of "one": that row, and no sweep.
        let asked = rewritten(refs("3333", "2222"));
        assert_eq!(asked.whole, None);
        assert_eq!(asked.keys.keys().collect::<Vec<_>>(), ["one"]);
        // A restack dropped a backup ref no checkout is on: a listing, so
        // facts derived from refs wake, and no row.
        let asked = rewritten(refs("3333", "9999"));
        assert_eq!((asked.whole, asked.keys.len()), (None, 0));
        assert_eq!(asked.index, Some(Request::Recheck));
        // Rewritten as it was: nothing.
        let asked = rewritten(refs("3333", "9999"));
        assert_eq!(asked, wt_runtime::Demand::default());
        // Anything that is not a ref changing reads every row.
        let asked = rewritten(refs("3333", "9999").replace("sorted", "unsorted"));
        assert_eq!(asked.whole, Some(Request::Recheck));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn the_backstop_rolls_round_the_fleet_and_skips_rows_just_read() {
        let scope = TaskScope::new();
        let (router, mut git_publisher) = routed(&scope);
        let row = |key: &str| LayoutRow {
            key: key.into(),
            path: format!("/wt/{key}").into(),
            ..LayoutRow::default()
        };
        *router.layout.write().unwrap() = Layout {
            rows: ["a", "b", "c", "d", "e", "f"].map(row).to_vec(),
        };
        let none = BTreeSet::new();
        let start = std::time::Instant::now();
        // Rows seen for the first time have just been read.
        assert!(router.backstop(&none, start).is_empty());
        assert!(router.backstop(&none, start + BACKSTOP / 2).is_empty());
        // A change routed to a row is a read of it; the backstop has nothing
        // to add for a period.
        router.invalidate(&["/wt/c/src/lib.rs".into()]);
        assert_eq!(
            git_publisher.take_demand().keys.keys().collect::<Vec<_>>(),
            ["c"]
        );
        // A period on, the five others fall due together. They are read
        // oldest first, more than the usual one a step while that many
        // wait, so the last of them is not left a further period.
        let due = start + BACKSTOP;
        assert_eq!(router.backstop(&none, due), ["a", "b"]);
        assert_eq!(router.backstop(&none, due), ["d"]);
        assert_eq!(router.backstop(&none, due), ["e"]);
        assert_eq!(router.backstop(&none, due), ["f"]);
        assert!(router.backstop(&none, due).is_empty());
        assert_eq!(router.backstop(&none, due + Duration::from_secs(1)), ["c"]);
        // A checkout nobody is working in waits several periods.
        let idle = BTreeSet::from(["a".to_owned()]);
        let later = due + BACKSTOP;
        assert_eq!(router.backstop(&idle, later), ["b"]);
        assert_eq!(router.backstop(&idle, later), ["d"]);
        assert_eq!(router.backstop(&idle, later), ["e"]);
        assert_eq!(router.backstop(&idle, later), ["f"]);
        assert!(router.backstop(&idle, later).is_empty());
        let idle_due = due + BACKSTOP * BACKSTOP_IDLE_FACTOR;
        assert_eq!(router.backstop(&idle, idle_due)[0], "a");
        // A row that left the fleet is forgotten.
        router.layout.write().unwrap().rows.truncate(1);
        router.backstop(&none, later);
        assert_eq!(router.asked.lock().unwrap().len(), 1);
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn changes_that_cannot_be_routed_in_a_burst_are_one_sweep() {
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
        let (metadata, _) = leaf_channel::<crate::local_source::Metadata>(
            &scope,
            spec("test-metadata", Domain::Metadata),
        );
        let pace = Duration::from_secs(1);
        let router = Router::new(
            git.view(),
            metadata.view(),
            Arc::new(Paced::metadata(metadata.view())),
            "/state/wt.sqlite".into(),
            ["/wt/main".into(), "/wt".into()],
            pace,
        );
        *router.layout.write().unwrap() = Layout {
            rows: vec![LayoutRow {
                key: "a".into(),
                path: "/wt/a".into(),
                ..LayoutRow::default()
            }],
        };
        let asked = |router: &Router| router.asked.lock().unwrap().get("a").copied();
        assert_eq!(asked(&router), None);
        for _ in 0..13 {
            router.invalidate(&["/elsewhere/file".into()]);
        }
        assert_eq!(git_publisher.take_demand().whole, Some(Request::Recheck));
        assert!(git_publisher.take_demand().whole.is_none());
        // Rows count as asked for when a read of all of them is asked for,
        // not while one is only held back: until then the backstop must
        // still be able to reach them.
        let first = asked(&router).expect("the first read stamps its rows");
        std::thread::sleep(Duration::from_millis(5));
        router.invalidate(&["/elsewhere/file".into()]);
        assert_eq!(asked(&router), Some(first));
        // One more is owed for what arrived meanwhile, when its time comes.
        let due = router.sweeps.due().expect("one read is held back");
        tokio::time::sleep_until(due).await;
        router.sweeps.release();
        assert_eq!(git_publisher.take_demand().whole, Some(Request::Recheck));
        assert!(asked(&router).is_some_and(|second| second > first));
        router.sweeps.release();
        assert!(git_publisher.take_demand().whole.is_none());
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn the_files_that_carry_git_facts_are_noticed_between_walks() {
        let root = tempfile::tempdir().unwrap();
        let git_dir = std::fs::canonicalize(root.path()).unwrap();
        let head = git_dir.join("HEAD");
        std::fs::write(&head, "ref: refs/heads/main\n").unwrap();
        let layout = Layout {
            rows: vec![LayoutRow {
                key: "one".into(),
                path: "/wt/one".into(),
                git_dir: Some(git_dir.clone()),
                common_dir: Some(git_dir.clone()),
                branch: "feature/one".into(),
                upstream: Some("origin/feature/one".into()),
            }],
        };
        let hot = layout.hot_files();
        for file in [
            "HEAD",
            "index",
            "MERGE_HEAD",
            "rebase-merge",
            "packed-refs",
            "refs/heads/feature/one",
            "refs/remotes/origin/feature/one",
        ] {
            assert!(hot.contains(&git_dir.join(file)), "{file}");
        }
        let scope = TaskScope::new();
        let plan = Arc::new(Mutex::new(ScanPlan {
            roots: BTreeMap::from([(git_dir.clone(), false)]),
            hot,
            ..ScanPlan::default()
        }));
        let mut scans = Scanner::start(scope.token(), plan.clone());
        async fn next(scans: &mut Scanner) -> Scanned {
            tokio::time::timeout(Duration::from_secs(30), scans.next())
                .await
                .expect("the scanner reported nothing")
                .expect("the scanner stopped")
        }
        // The first walk follows the thread's first look at the files, so
        // from here a change to one is a change, however loaded the machine.
        let Scanned::Walk { roots, sample, .. } = next(&mut scans).await else {
            panic!("the first report is a walk");
        };
        assert_eq!(roots.len(), 1);
        assert!(sample.contains_key(&head));
        // HEAD moves and a rebase starts: both are reported as checked
        // files, not left for the next walk to find.
        std::fs::write(&head, "0123456789abcdef\n").unwrap();
        std::fs::create_dir(git_dir.join("rebase-merge")).unwrap();
        let mut seen = BTreeSet::new();
        while seen.len() < 2 {
            if let Scanned::Hot(paths) = next(&mut scans).await {
                seen.extend(paths);
            }
        }
        assert_eq!(
            seen,
            BTreeSet::from([head.clone(), git_dir.join("rebase-merge")])
        );

        // The thread dies. That is logged and it is started again: the next
        // walk and the next change still arrive.
        plan.lock().unwrap().fail = true;
        loop {
            if matches!(next(&mut scans).await, Scanned::Walk { .. }) && !plan.lock().unwrap().fail
            {
                break;
            }
        }
        assert!(scans.restarts <= 1);
        std::fs::write(&head, "fedcba9876543210\n").unwrap();
        loop {
            // The new thread's first look may come after the write; then
            // the walk is what reports it.
            match next(&mut scans).await {
                Scanned::Hot(paths) => {
                    assert_eq!(paths, std::slice::from_ref(&head));
                    break;
                }
                Scanned::Walk { sample, .. } => {
                    let now = std::fs::symlink_metadata(&head)
                        .ok()
                        .map(|metadata| FileSignature::from_metadata(&metadata));
                    if sample.get(&head) == now.as_ref() {
                        break;
                    }
                }
            }
        }
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
    }

    #[tokio::test]
    async fn a_scanner_that_keeps_dying_is_given_up_on_and_says_so() {
        // No runtime time passes between deaths except the backoff, so this
        // takes the sum of the backoffs: under eight seconds.
        let scope = TaskScope::new();
        let plan = Arc::new(Mutex::new(ScanPlan::default()));
        let mut scans = Scanner::start(scope.token(), plan.clone());
        let failing = plan.clone();
        let fail = std::thread::spawn(move || {
            for _ in 0..2_000 {
                failing
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .fail = true;
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        // Walks from threads that lived long enough to make one are ignored
        // here; each resets the count, so only uninterrupted deaths end it.
        let gave_up = tokio::time::timeout(Duration::from_secs(60), async {
            while scans.next().await.is_some() {}
        })
        .await;
        assert!(gave_up.is_ok(), "restarted without bound");
        assert_eq!(scans.restarts, SCANNER_RESTARTS);
        fail.join().unwrap();
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
    }
}
