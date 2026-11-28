//! The board's part of what watches depend on, read by a process with no
//! board: `wt wait` judges its conditions itself, so it works while no TUI
//! is evaluating watches. One pass reads the inventory, the state and, only
//! where a condition asks about a pull request, GitHub once for exactly the
//! worktrees named: the events daemon's snapshot while it is fresh, else one
//! batched request at the pace every other watch read keeps. That pace is
//! shared by every waiting process through a file, so ten waits ask GitHub
//! as often as one. While a low budget holds the next read back, an answer
//! older than a minute proves only its merges: the rest reads unknown, with
//! the time of the next read as the reason. Rows are built by the same rule the engine uses, so a
//! word means the same in both.
//!
//! A wait does not sit out an interval to learn of a change. [`Signals`]
//! looks at the few files a pass reads from (the state database, the
//! daemon's snapshot, origin's base branches as last fetched), and
//! [`RowReader::concerns`] tells a change that can alter the answer from
//! the many that cannot, so the pass runs when there is something to find.
//! The intervals stay as the backstop for what leaves no trace in those
//! files.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};

use wt_app_core::context::AppContext;
use wt_core::watch::{Condition, PrFacts, RowFacts, RowState, Watch};
use wt_github::{GithubClient, GithubData, GithubOptions};
use wt_platform::lock::FileLock;

use crate::{
    automation_facts::{WatchRowInput, watch_row},
    watch_facts::{PR_INTERVAL, epoch_ms},
};

/// A branch stays in the shared read this long after a wait last asked
/// about it, so waits on different worktrees share one request.
const ASKED_KEEP_MS: u64 = 5 * 60 * 1000;
/// How stale a wait lets its own entry in `asked` get before rewriting it.
const ASKED_REFRESH_MS: u64 = 2 * 60 * 1000;
const UNHELD_KEEP_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const SHARED_LOCK: &str = "wait-github";

/// The GitHub read every `wt wait` of a repository shares, as stored in
/// `wait-github-<repository>.json` under the cache root. Written only while holding the
/// `wait-github` lock, which a process keeps across its fetch, so a second
/// wait finds the first one's answer instead of asking again.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SharedRead {
    /// The repository the read is of (`wt_ops::events::repository_identity`).
    /// A cache root can serve several repositories, and a branch name means
    /// a different pull request in each: a file naming another repository,
    /// or none, is no answer.
    repository: Option<String>,
    /// When the last fetch began, successful or not.
    fetch_started_ms: u64,
    /// No fetch before this: the last one failed.
    backoff_until_ms: u64,
    failures: u32,
    problem: Option<String>,
    /// `None` after a failed fetch: an answer is only as good as the read
    /// that produced it.
    data: Option<GithubData>,
    /// When a wait last asked about each branch.
    asked: BTreeMap<String, u64>,
    /// Armings a fetch showed GitHub not holding, as `slug|armedAtMs` to
    /// the start of the first such fetch.
    unheld: BTreeMap<String, u64>,
}

#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// The stored answer is young enough and covers the branches.
    Use,
    /// The stored answer covers the branches and is older than a wait would
    /// normally accept, and GitHub's budget keeps the next read until
    /// `next_ms`.
    Paced { next_ms: u64 },
    /// A failed fetch is still backing off.
    Hold,
    /// Ask GitHub about these branches.
    Fetch(Vec<String>),
}

impl SharedRead {
    fn plan(&self, branches: &[String], now_ms: u64) -> Plan {
        if now_ms < self.backoff_until_ms {
            return Plan::Hold;
        }
        // Waits poll in the background, so a low GitHub budget stretches
        // their interval as it does every other poller's. Judged as of the
        // fetch that reported the budget, which the interval counts from.
        let floor = u64::try_from(PR_INTERVAL.as_millis()).unwrap_or(u64::MAX);
        let interval = wt_events::paced_interval_ms(
            floor,
            self.data
                .as_ref()
                .and_then(|data| data.rate_limit)
                .map(wt_ops::events::rate_budget),
            self.fetch_started_ms,
        );
        // A start in the future is a clock that moved, not a fresh answer.
        let age = now_ms.checked_sub(self.fetch_started_ms);
        let covers = self.data.as_ref().is_some_and(|data| {
            branches
                .iter()
                .all(|branch| data.covered_branches.contains(branch))
        });
        if covers && let Some(age) = age.filter(|age| *age < interval) {
            return if age < floor {
                Plan::Use
            } else {
                Plan::Paced {
                    next_ms: self.fetch_started_ms.saturating_add(interval),
                }
            };
        }
        let mut all = branches.iter().cloned().collect::<BTreeSet<_>>();
        all.extend(
            self.asked
                .iter()
                .filter(|(_, at)| now_ms.saturating_sub(**at) <= ASKED_KEEP_MS)
                .map(|(branch, _)| branch.clone()),
        );
        Plan::Fetch(all.into_iter().collect())
    }

    /// Note that a wait asked about `branches` now. Returns whether the
    /// record is worth writing for it.
    fn note_asked(&mut self, branches: &[String], now_ms: u64) -> bool {
        let stale = branches.iter().any(|branch| {
            self.asked
                .get(branch)
                .is_none_or(|at| now_ms.saturating_sub(*at) >= ASKED_REFRESH_MS)
        });
        if stale {
            for branch in branches {
                self.asked.insert(branch.clone(), now_ms);
            }
            self.asked
                .retain(|_, at| now_ms.saturating_sub(*at) <= ASKED_KEEP_MS);
        }
        stale
    }

    /// Whether the arming `key`, which the fetch begun at `fetch_ms` shows
    /// GitHub not holding, was shown so by an earlier fetch too. One read
    /// can land between an arm and GitHub recording it; two cannot both.
    fn unheld_twice(&mut self, key: &str, fetch_ms: u64) -> (bool, bool) {
        match self.unheld.get(key) {
            Some(first) if *first < fetch_ms => (true, false),
            Some(_) => (false, false),
            None => {
                self.unheld.insert(key.to_owned(), fetch_ms);
                (false, true)
            }
        }
    }
}

/// What an answer held past its minute by the budget still proves: a merge
/// is final, so a merged pull request stays one. Everything else it says
/// may have changed, and reads as not covered.
fn settled_only(mut data: GithubData) -> GithubData {
    data.prs.retain(|_, pr| pr.state == "MERGED");
    data.covered_branches = data.prs.keys().cloned().collect();
    data.merge_queue.clear();
    data.merge_queue_complete = false;
    data
}

/// Why a wait has no current pull-request answer while the budget is low.
fn budget_problem(next_ms: u64, utc_offset_minutes: i32) -> String {
    let minutes = (i64::try_from(next_ms / 60_000).unwrap_or(0) + i64::from(utc_offset_minutes))
        .rem_euclid(24 * 60);
    format!(
        "GitHub budget low, next read at {:02}:{:02}",
        minutes / 60,
        minutes % 60
    )
}

/// One file per repository, so two repositories on one cache root keep
/// their own pace instead of each discarding the other's read.
fn shared_path(ctx: &AppContext) -> PathBuf {
    let repository: String = ctx
        .config
        .repo_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    ctx.config
        .paths
        .cache_root
        .join(format!("wait-github-{repository}.json"))
}

fn shared_identity(ctx: &AppContext) -> String {
    wt_ops::events::repository_identity(&ctx.config)
}

/// The stored read of `repository`, or an empty one for it.
async fn load_shared(path: &Path, repository: &str) -> SharedRead {
    let stored = match tokio::fs::read(path).await {
        // A file this build cannot read is a missing answer, never an error.
        Ok(bytes) => serde_json::from_slice::<SharedRead>(&bytes).ok(),
        Err(_) => None,
    };
    stored
        .filter(|shared| shared.repository.as_deref() == Some(repository))
        .unwrap_or_else(|| SharedRead {
            repository: Some(repository.to_owned()),
            ..SharedRead::default()
        })
}

async fn save_shared(path: &Path, shared: &SharedRead) {
    let write = async {
        let bytes = serde_json::to_vec(shared).map_err(std::io::Error::other)?;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        tokio::fs::write(&tmp, bytes).await?;
        if let Err(error) = tokio::fs::rename(&tmp, path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error);
        }
        Ok::<_, std::io::Error>(())
    };
    if let Err(error) = write.await {
        tracing::warn!(%error, path = %path.display(), "cannot store the shared GitHub read");
    }
}

/// How often the signal files are looked at: one `stat` each.
const SIGNAL_TICK: Duration = Duration::from_millis(100);

/// What changed on disk since a wait last looked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changed {
    /// The state database: a status, a gate, an arming.
    pub state: bool,
    /// The events daemon's GitHub snapshot.
    pub github: bool,
    /// What origin's base branches were last fetched as.
    pub git: bool,
    /// The worktrees themselves: one was added or removed.
    pub inventory: bool,
}

impl Changed {
    pub fn any(self) -> bool {
        self.state || self.github || self.git || self.inventory
    }

    pub fn absorb(&mut self, other: Self) {
        self.state |= other.state;
        self.github |= other.github;
        self.git |= other.git;
        self.inventory |= other.inventory;
    }
}

#[derive(Clone, Copy)]
enum Signal {
    State,
    Github,
    Git,
    Inventory,
}

/// A file as last seen: `None` while it does not exist.
type Seen = Option<(SystemTime, u64)>;

fn seen(path: &Path) -> Seen {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The files whose change means a pass may read something new. Portable and
/// independent of any running wt: a file nobody writes just never changes.
pub struct Signals {
    files: Vec<(PathBuf, Signal, Seen)>,
}

impl Signals {
    /// Takes each file as it is now: create this before the first pass, so
    /// a write made during that pass is still seen as a change.
    pub fn new(ctx: &AppContext) -> Self {
        let paths = &ctx.config.paths;
        let mut wal = paths.state_db.clone().into_os_string();
        wal.push("-wal");
        let files = [
            (paths.state_db.clone(), Signal::State),
            (PathBuf::from(wal), Signal::State),
            (
                paths.cache_root.join("events").join("github.json"),
                Signal::Github,
            ),
        ];
        // A fetch run in any worktree moves the refs every checkout shares,
        // and writes FETCH_HEAD only in its own: the refs are the signal.
        // A base branch is a loose file until Git packs it.
        let git = paths.main_clone.join(".git");
        let branch = &ctx.config.branch;
        let refs = std::iter::once(&branch.base)
            .chain(&branch.production)
            .map(|name| git.join("refs/remotes/origin").join(name))
            .chain([git.join("packed-refs")])
            .map(|path| (path, Signal::Git));
        // Adding or removing a worktree adds or removes an entry in each of
        // these directories, which changes the directory's own times.
        let listing = [git.join("worktrees"), paths.worktree_root.clone()]
            .into_iter()
            .map(|path| (path, Signal::Inventory));
        Self {
            files: files
                .into_iter()
                .chain(refs)
                .chain(listing)
                .map(|(path, signal)| {
                    let seen = seen(&path);
                    (path, signal, seen)
                })
                .collect(),
        }
    }

    fn poll(&mut self) -> Changed {
        let mut changed = Changed::default();
        for (path, signal, last) in &mut self.files {
            let now = seen(path);
            if now == *last {
                continue;
            }
            *last = now;
            match signal {
                Signal::State => changed.state = true,
                Signal::Github => changed.github = true,
                Signal::Git => changed.git = true,
                Signal::Inventory => changed.inventory = true,
            }
        }
        changed
    }

    /// Resolves when a file changed. Dropping it loses nothing: a change
    /// not yet returned is found by the next call.
    pub async fn changed(&mut self) -> Changed {
        loop {
            tokio::time::sleep(SIGNAL_TICK).await;
            let changed = self.poll();
            if changed.any() {
                return changed;
            }
        }
    }
}

/// The answer the waits of this repository last got from GitHub, with when
/// its fetch began (epoch ms). A wait asks GitHub itself about the merge
/// queue, so this can be newer than the events daemon's snapshot for the
/// branches it covers. For display: nothing is decided from it.
pub async fn latest_shared_read(ctx: &AppContext) -> Option<(GithubData, u64)> {
    let shared = load_shared(&shared_path(ctx), &shared_identity(ctx)).await;
    Some((shared.data?, shared.fetch_started_ms))
}

/// The board's last GitHub read, as it saved it, with when that read began
/// (epoch ms). Display only: a restored value is never a witness.
pub async fn board_read(ctx: &AppContext) -> Option<(GithubData, u64)> {
    let caches = ctx.caches.clone();
    let (data, saved_at_ms) = tokio::task::spawn_blocking(move || {
        wt_runtime::DiskCache::<GithubData>::new(&caches).peek("github")
    })
    .await
    .ok()??;
    // Data the board merged from a part with no observation time says
    // nothing of how old it is; the time it was saved is not that.
    let started_ms = data.observed_at_ms?.min(saved_at_ms);
    Some((data, started_ms))
}

/// A read of every one of `branches` that some wt process already made and
/// saved, begun no longer ago than the events daemon's snapshot may be old
/// and still count as fresh: the board's own, or the one waits and display
/// commands share. The newest of them, with when it began (epoch ms). For a
/// command that displays pull requests, so that it need not ask GitHub
/// again for what was just read.
pub async fn recent_read(ctx: &AppContext, branches: &[String]) -> Option<(GithubData, u64)> {
    let now = epoch_ms();
    let fresh = u64::try_from(crate::github_events_source::FRESH.as_millis()).unwrap_or(u64::MAX);
    let (board, shared) = tokio::join!(board_read(ctx), latest_shared_read(ctx));
    [board, shared]
        .into_iter()
        .flatten()
        .filter(|(data, started_ms)| {
            // A start in the future is a clock that moved, not a new answer.
            now.checked_sub(*started_ms).is_some_and(|age| age <= fresh)
                && branches
                    .iter()
                    .all(|branch| data.covered_branches.contains(branch))
        })
        .max_by_key(|(_, started_ms)| *started_ms)
}

/// Save a display command's own read, begun at `started_ms`, as the shared
/// one. The next command to print pull requests, and every wait, then finds
/// it instead of asking again.
///
/// Only a read that leaves no wait worse off is saved: one at least as wide
/// as the stored read and as what waits asked about lately. A narrower one
/// (a single worktree's, or only the rows Git left open) would leave their
/// branches uncovered, and they would ask GitHub at once instead of keeping
/// their pace. Nor does it wait for the lock: a wait holds it across its
/// whole fetch, and is about to save a newer read than this one.
pub async fn record_read(ctx: &AppContext, data: &GithubData, started_ms: u64) {
    let Ok(Some(_lock)) = FileLock::try_acquire(
        &ctx.config.paths.lock_dir,
        SHARED_LOCK,
        "record a GitHub read",
    )
    .await
    else {
        return;
    };
    let path = shared_path(ctx);
    let mut shared = load_shared(&path, &shared_identity(ctx)).await;
    if shared.data.is_some() && shared.fetch_started_ms >= started_ms {
        return;
    }
    let stored = shared
        .data
        .iter()
        .flat_map(|stored| stored.covered_branches.iter());
    let asked = shared
        .asked
        .iter()
        .filter(|(_, at)| started_ms.saturating_sub(**at) <= ASKED_KEEP_MS)
        .map(|(branch, _)| branch);
    if !stored
        .chain(asked)
        .all(|branch| data.covered_branches.contains(branch))
    {
        return;
    }
    shared.fetch_started_ms = started_ms;
    shared.failures = 0;
    shared.backoff_until_ms = 0;
    shared.problem = None;
    shared.data = Some(data.clone());
    save_shared(&path, &shared).await;
}

/// Why GitHub took `pr` out of the merge queue, in words, when it did so
/// after `arm` was sent for it and nothing holds the merge now. GitHub
/// keeps the latest removal on the pull request for good, so one from
/// before the arming says nothing about it, and neither does one a later
/// arming (by wt or by a person on GitHub) has made history: the pull
/// request must be open, not set to auto-merge, and known not to be queued.
/// `queued` is `None` where the read cannot say: no entry in a read that
/// held only part of the queue. The one rule for every surface that says it.
pub fn queue_removal(
    pr: &wt_github::PullRequest,
    arm: Option<&wt_core::MergeArm>,
    queued: Option<bool>,
) -> Option<String> {
    if pr.state != "OPEN" || pr.auto_merge.is_some() || queued != Some(false) {
        return None;
    }
    let arm = arm.filter(|arm| arm.pr == pr.number)?;
    let removal = pr.queue_removal.as_ref()?;
    if i64::try_from(removal.at_ms).unwrap_or(i64::MAX) <= arm.armed_at_ms {
        return None;
    }
    let reason = wt_core::sanitize_terminal_text(removal.reason.trim());
    Some(if reason.is_empty() {
        "removed from the merge queue".to_owned()
    } else {
        format!("removed from the merge queue: {reason}")
    })
}

/// What one pass established.
#[derive(Clone, Debug, Default)]
pub struct RowsRead {
    pub rows: BTreeMap<String, RowFacts>,
    /// The named worktrees' pull requests by number.
    pub prs: BTreeMap<u64, PrFacts>,
    /// Each live worktree's branch, for a check's `{{branch}}`.
    pub branches: BTreeMap<String, String>,
    /// Why pull-request answers are missing, when they are.
    pub problem: Option<String>,
    /// What the removal records say of the worktrees that are gone.
    pub removed: crate::watch_facts::Removed,
    /// Why GitHub took a named worktree's pull request out of the merge
    /// queue since its merge was armed, by slug: see [`queue_removal`].
    pub removals: BTreeMap<String, String>,
    /// When the shared GitHub read may next ask GitHub (epoch ms), where
    /// this pass went through it: nothing newer can be had before then.
    pub github_due_ms: Option<u64>,
}

/// Reads rows for one process's watches.
#[derive(Default)]
pub struct RowReader {
    problem: Option<String>,
    github_due_ms: Option<u64>,
    /// What the last pass read from the state for the worktrees its watches
    /// name: see [`concern`].
    state_seen: Option<serde_json::Value>,
    /// The state [`RowReader::concerns`] read to compare, kept for the pass
    /// it then asks for.
    state_next: Option<serde_json::Value>,
    /// Each live worktree's branch as the last pass listed them.
    branches: BTreeMap<String, String>,
    /// What the daemon's snapshot said, when the last pass began, of the
    /// pull requests that pass read from it: see [`RowReader::github_concern`].
    github_seen: Option<serde_json::Value>,
    /// Armings GitHub was seen holding during this process, as
    /// `(slug, armed_at_ms)`: the engine records that on the arm itself,
    /// and none may be running.
    seen_armed: BTreeSet<(String, i64)>,
}

/// One GitHub answer and, when it came from the shared read, when the
/// fetch behind it began.
struct GithubRead {
    data: GithubData,
    fetch_started_ms: Option<u64>,
}

/// What the watches ask of each worktree they name.
#[derive(Clone, Copy, Default)]
struct Asked {
    pr: bool,
    landing: bool,
    queue: bool,
}

/// The fields of a worktree's record a pass reads.
const ROW_FIELDS: [&str; 5] = ["createdAt", "work", "mergeArm", "baseSha", "baseBranch"];

/// The fields of a removal record a pass reads.
const REMOVED_FIELDS: [&str; 7] = [
    "removedAt",
    "landedOnAtRemoval",
    "landedOn",
    "prState",
    "gitState",
    "prMergeCommitOid",
    wt_store::LANDING_COMMIT_OID,
];

/// The part of the state a pass over `watches` depends on: for each
/// worktree they name, the fields it reads of its record and of its removal
/// records, and the removals under way (a removal leaves the worktree's own
/// record as it was). Most writes to a fleet's state are about other
/// worktrees or other fields and leave this as it was.
fn concern(state: &serde_json::Value, watches: &[Watch]) -> serde_json::Value {
    let mut slugs = BTreeSet::new();
    for watch in watches {
        slugs.insert(watch.slug.as_str());
        for cond in &watch.when {
            if let Condition::Worktree { slug, .. } | Condition::Gone { slug } = &cond.cond {
                slugs.insert(slug.as_str());
            }
        }
    }
    let rows = slugs
        .iter()
        .map(|slug| {
            let stored = &state["slugs"][*slug];
            let fields = ROW_FIELDS
                .iter()
                .map(|field| stored[*field].clone())
                .collect::<Vec<_>>();
            ((*slug).to_owned(), serde_json::Value::Array(fields))
        })
        .collect::<serde_json::Map<_, _>>();
    let removed = state["removed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry["slug"]
                .as_str()
                .is_some_and(|slug| slugs.contains(slug))
        })
        .map(|entry| {
            let fields = REMOVED_FIELDS
                .iter()
                .map(|field| entry[*field].clone())
                .collect::<Vec<_>>();
            serde_json::json!([entry["slug"], fields])
        })
        .collect::<Vec<_>>();
    serde_json::json!([rows, removed, state["removals"]])
}

fn asked(watches: &[Watch]) -> BTreeMap<String, Asked> {
    let mut asked: BTreeMap<String, Asked> = BTreeMap::new();
    for watch in watches {
        for cond in watch.when.iter().filter(|cond| !cond.latched) {
            let (slug, state) = match &cond.cond {
                Condition::Own(state) => (&watch.slug, *state),
                Condition::Worktree { slug, state } => (slug, *state),
                _ => continue,
            };
            let entry = asked.entry(slug.clone()).or_default();
            match state {
                RowState::Ready | RowState::Verified => {}
                RowState::Merged | RowState::Staging | RowState::Production => {
                    entry.pr = true;
                    entry.landing = true;
                }
                RowState::Evicted => {
                    entry.pr = true;
                    entry.queue = true;
                }
                RowState::Green | RowState::Closed | RowState::Settled | RowState::Reviewed => {
                    entry.pr = true;
                }
            }
        }
    }
    asked
}

impl RowReader {
    /// Whether what `changed` on disk can alter what a pass over `watches`
    /// reads, so that a new pass is worth its Git and GitHub reads now. A
    /// change it cannot judge counts as one that can.
    pub async fn concerns(
        &mut self,
        ctx: &AppContext,
        watches: &[Watch],
        changed: Changed,
    ) -> bool {
        let asked = asked(watches);
        if changed.git && asked.values().any(|asked| asked.landing) {
            return true;
        }
        // A worktree came or went: what "gone" and every row condition read.
        let names_worktree = watches.iter().any(|watch| {
            watch.when.iter().any(|cond| {
                matches!(
                    cond.cond,
                    Condition::Own(_) | Condition::Worktree { .. } | Condition::Gone { .. }
                )
            })
        });
        if changed.inventory && names_worktree {
            return true;
        }
        // The daemon rewrites its snapshot for every pull request of the
        // fleet; only what it says of the ones asked about is a reason.
        if changed.github
            && (asked.values().any(|asked| asked.pr)
                || !wt_core::watch::needs(watches).prs.is_empty())
            && self.github_seen != Some(self.github_concern(ctx, watches).await)
        {
            return true;
        }
        if !changed.state {
            return false;
        }
        let Ok(state) = ctx.database.call(|store| Ok(store.read_wt_state()?)).await else {
            return true;
        };
        if self.state_seen.as_ref() == Some(&concern(&state, watches)) {
            return false;
        }
        self.state_next = Some(state);
        true
    }

    /// The part of the daemon's snapshot a pass over `watches` reads, as the
    /// snapshot stands now: the pull requests of the branches its row
    /// conditions ask about, and the state of the ones named by number. A
    /// question about the merge queue goes to GitHub itself (see
    /// [`RowReader::github`]), so the snapshot answers no row then.
    async fn github_concern(&self, ctx: &AppContext, watches: &[Watch]) -> serde_json::Value {
        use crate::github_events_source::Held;
        let asked = asked(watches);
        let rows = if asked.values().any(|asked| asked.queue) {
            Vec::new()
        } else {
            self.branches
                .iter()
                .filter(|(slug, branch)| {
                    !branch.is_empty() && asked.get(*slug).is_some_and(|asked| asked.pr)
                })
                .map(|(_, branch)| branch.clone())
                .collect::<Vec<_>>()
        };
        let off = std::env::var("WT_GITHUB").as_deref() == Ok("off");
        let rows = if rows.is_empty() || off {
            None
        } else {
            crate::github_events_source::load_for_display(ctx, &rows)
                .await
                // Past fresh a pass takes only the merges from it.
                .map(|shown| serde_json::json!([shown.held == Held::Fresh, shown.read.data.prs]))
        };
        let numbers = wt_core::watch::needs(watches).prs;
        let numbered = if numbers.is_empty() {
            None
        } else {
            let branches = self.branches.values().cloned().collect::<Vec<_>>();
            self.fleet_prs(ctx, &branches).await.map(|data| {
                data.prs
                    .values()
                    .filter(|pr| numbers.contains(&pr.number))
                    .map(|pr| serde_json::json!([pr.number, pr.state, pr.merge_commit_oid]))
                    .collect::<Vec<_>>()
            })
        };
        serde_json::json!([rows, numbered])
    }

    /// One pass. `Err` when the worktrees or the state cannot be read
    /// at all; a failed GitHub read leaves its answers unknown instead.
    pub async fn read(&mut self, ctx: &AppContext, watches: &[Watch]) -> Result<RowsRead, String> {
        // Taken before anything can fail: a state kept past a failed pass
        // would be an old one by the pass that then used it.
        let prompted = self.state_next.take();
        let records = ctx
            .repository
            .inventory(&ctx.cancellation)
            .await
            .map_err(|error| format!("cannot list the worktrees: {error:#}"))?
            .into_iter()
            .filter(|record| !record.is_main)
            .collect::<Vec<_>>();
        let state = match prompted {
            Some(state) => state,
            None => ctx
                .database
                .call(|store| Ok(store.read_wt_state()?))
                .await
                .map_err(|error| format!("cannot read wt's state: {error:#}"))?,
        };
        self.state_seen = Some(concern(&state, watches));
        let asked = asked(watches);
        let named = records
            .iter()
            .filter(|record| asked.contains_key(record.target.slug()))
            .cloned()
            .collect::<Vec<_>>();
        let branches = named
            .iter()
            .filter(|record| asked[record.target.slug()].pr)
            .map(|record| record.target.branch.clone())
            .filter(|branch| !branch.is_empty())
            .collect::<Vec<_>>();
        let queue = asked.values().any(|asked| asked.queue);
        // Before the snapshot is read for the pass: one written in between
        // then reads as a change, which costs a pass and misses nothing.
        self.branches = records
            .iter()
            .map(|record| {
                (
                    record.target.slug().to_owned(),
                    record.target.branch.clone(),
                )
            })
            .collect();
        self.github_seen = Some(self.github_concern(ctx, watches).await);
        let read_started_ms = now_ms();
        self.github_due_ms = None;
        let github_read = self.github(ctx, &branches, queue).await;
        let fetch_started_ms = github_read.as_ref().and_then(|read| read.fetch_started_ms);
        let github = github_read.map(|read| read.data);
        let empty = GithubData::default();
        // Armings the fetch shows GitHub holding, and not holding.
        let mut held = Vec::new();
        let mut unheld = Vec::new();
        let lifecycle = if asked.values().any(|asked| asked.landing) {
            let rows = named
                .iter()
                .filter(|record| asked[record.target.slug()].landing)
                .cloned()
                .collect();
            wt_ops::lifecycle_ops::landing_facts(
                ctx,
                rows,
                &state,
                github.as_ref().unwrap_or(&empty),
                &wt_ops::lifecycle_ops::wants_of(watches),
            )
            .await
            .map(|evidence| evidence.rows)
            .unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        let probed = |stage| ctx.config.environments.get(stage).is_some();
        let mut read = RowsRead {
            removed: crate::watch_facts::removed_rows(
                &state,
                ctx.config.branch.separate_production(),
            ),
            problem: self.problem.clone().filter(|_| !branches.is_empty()),
            github_due_ms: self.github_due_ms,
            ..RowsRead::default()
        };
        for record in &records {
            let slug = record.target.slug();
            let branch = &record.target.branch;
            let (Some(asked), Some(data), Some(fetch_ms)) =
                (asked.get(slug), github.as_ref(), fetch_started_ms)
            else {
                continue;
            };
            let Ok(arm) = serde_json::from_value::<wt_core::MergeArm>(
                state["slugs"][slug]["mergeArm"].clone(),
            ) else {
                continue;
            };
            let Some(pr) = data
                .prs
                .get(branch)
                .filter(|pr| pr.number == arm.pr && pr.state == "OPEN")
            else {
                continue;
            };
            let queued = data.merge_queue.contains_key(branch);
            if pr.auto_merge.is_some() || queued {
                held.push(arm_key(slug, &arm));
            } else if asked.queue
                && arm.seen_armed_at_ms.is_none()
                && data.merge_queue_complete
                && data.covered_branches.contains(branch)
                && i64::try_from(fetch_ms).unwrap_or(i64::MAX)
                    >= arm
                        .armed_at_ms
                        .saturating_add(wt_automations::MERGE_ARM_GRACE_MS)
            {
                unheld.push(arm_key(slug, &arm));
            }
        }
        let dropped = match fetch_started_ms {
            Some(fetch_ms) if !held.is_empty() || !unheld.is_empty() => {
                settle_unheld(ctx, &held, &unheld, fetch_ms).await
            }
            _ => BTreeSet::new(),
        };
        for record in &records {
            let slug = record.target.slug();
            let branch = &record.target.branch;
            let stored = &state["slugs"][slug];
            read.branches.insert(slug.to_owned(), branch.clone());
            let created_at = stored["createdAt"].as_str();
            if !asked.contains_key(slug) {
                read.rows.insert(
                    slug.to_owned(),
                    RowFacts {
                        exists: true,
                        incarnation: created_at.map(str::to_owned),
                        ..RowFacts::default()
                    },
                );
                continue;
            }
            let data = github.as_ref().unwrap_or(&empty);
            let pr = data.prs.get(branch);
            let queue = data.merge_queue.get(branch);
            let mut merge_arm =
                serde_json::from_value::<wt_core::MergeArm>(stored["mergeArm"].clone()).ok();
            if let Some(arm) = &mut merge_arm {
                let key = (slug.to_owned(), arm.armed_at_ms);
                if read_started_ms > arm.armed_at_ms
                    && pr.is_some_and(|pr| {
                        pr.number == arm.pr
                            && pr.state == "OPEN"
                            && (pr.auto_merge.is_some() || queue.is_some())
                    })
                {
                    self.seen_armed.insert(key.clone());
                }
                if self.seen_armed.contains(&key) {
                    arm.seen_armed_at_ms.get_or_insert(read_started_ms);
                } else if dropped.contains(&arm_key(slug, arm)) {
                    // No engine recorded GitHub holding this merge, or an
                    // earlier wait saw it and exited. Two fetches past the
                    // grace period showing it gone stand in for that
                    // sighting here; the record itself is not rewritten.
                    arm.seen_armed_at_ms.get_or_insert(arm.armed_at_ms);
                }
            }
            let queued = queue
                .map(|_| true)
                .or(data.merge_queue_complete.then_some(false));
            if let Some(why) = pr.and_then(|pr| queue_removal(pr, merge_arm.as_ref(), queued)) {
                read.removals.insert(slug.to_owned(), why);
            }
            let work = wt_core::parse_work_status(&stored["work"]);
            read.rows.insert(
                slug.to_owned(),
                watch_row(WatchRowInput {
                    created_at,
                    work: work.as_ref(),
                    pr,
                    pr_read: github.is_some() && data.covered_branches.contains(branch),
                    queue,
                    queue_complete: data.merge_queue_complete,
                    merge_arm,
                    now_ms: read_started_ms,
                    lifecycle: lifecycle.get(slug).copied().map(|mut lifecycle| {
                        lifecycle.deploys = lifecycle.deploys.for_probes(probed);
                        lifecycle
                    }),
                    separate_production: ctx.config.branch.separate_production(),
                }),
            );
        }
        let numbered = |data: &GithubData| {
            data.prs
                .values()
                .map(|pr| {
                    (
                        pr.number,
                        wt_ops::watch_probe::pr_facts(&pr.state, pr.merge_commit_oid.clone()),
                    )
                })
                .collect::<Vec<_>>()
        };
        // A pull request named by number is usually a live worktree's: the
        // daemon's snapshot then answers for it as soon as it is written,
        // and GitHub is not asked about that number at all.
        if !wt_core::watch::needs(watches).prs.is_empty() {
            read.prs.extend(
                self.fleet_prs(ctx, &self.branches.values().cloned().collect::<Vec<_>>())
                    .await
                    .iter()
                    .flat_map(numbered),
            );
        }
        if let Some(github) = &github {
            read.prs.extend(numbered(github));
        }
        Ok(read)
    }

    /// The daemon's snapshot for every live branch, when it holds them all
    /// and `WT_GITHUB` allows it. Older than the board would use, it keeps
    /// only its merges.
    async fn fleet_prs(&self, ctx: &AppContext, branches: &[String]) -> Option<GithubData> {
        use crate::github_events_source::Held;
        if std::env::var("WT_GITHUB").as_deref() == Ok("off") {
            return None;
        }
        let branches = branches
            .iter()
            .filter(|branch| !branch.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        if branches.is_empty() {
            return None;
        }
        // The display rule on purpose: a waiting session is usually on an
        // older build than the daemon, and its answer is an observation the
        // waiter reports, never a witness for a write. The snapshot is
        // still schema-gated, and a development build reads only its own.
        let shown = crate::github_events_source::load_for_display(ctx, &branches).await?;
        Some(match shown.held {
            Held::Fresh => shown.read.data,
            Held::Paced { .. } | Held::BudgetLow => settled_only(shown.read.data),
        })
    }

    /// GitHub's answer covering `branches`, or `None` when there is no
    /// current one. The daemon's snapshot does not say whether it holds the
    /// whole merge queue, so a question about the queue goes to GitHub.
    async fn github(
        &mut self,
        ctx: &AppContext,
        branches: &[String],
        queue: bool,
    ) -> Option<GithubRead> {
        if branches.is_empty() {
            return None;
        }
        if std::env::var("WT_GITHUB").as_deref() == Ok("off") {
            self.problem = Some("GitHub is switched off (WT_GITHUB=off)".into());
            return None;
        }
        // The display rule, for the reason given at the snapshot read above.
        if !queue
            && let Some(shown) = crate::github_events_source::load_for_display(ctx, branches).await
        {
            use crate::github_events_source::Held;
            match shown.held {
                Held::Fresh => {
                    self.problem = None;
                    return Some(GithubRead {
                        data: shown.read.data,
                        fetch_started_ms: None,
                    });
                }
                // The daemon is waiting out the budget and will read before
                // this process may: its old answer proves what the shared
                // read's would, and asking GitHub here would defeat the pace.
                Held::Paced { next_fetch_at_ms } => {
                    self.problem = Some(budget_problem(
                        next_fetch_at_ms,
                        wt_app_core::display_time::local_utc_offset_minutes(next_fetch_at_ms)
                            .unwrap_or(0),
                    ));
                    return Some(GithubRead {
                        data: settled_only(shown.read.data),
                        fetch_started_ms: None,
                    });
                }
                // The daemon's read is overdue: the shared read below keeps
                // the pace itself.
                Held::BudgetLow => {}
            }
        }
        let _lock = match FileLock::acquire(
            &ctx.config.paths.lock_dir,
            SHARED_LOCK,
            "wait GitHub read",
            &ctx.cancellation,
        )
        .await
        {
            Ok(lock) => lock,
            Err(error) => {
                self.problem = Some(format!("GitHub could not be read: {error}"));
                return None;
            }
        };
        let path = shared_path(ctx);
        let mut shared = load_shared(&path, &shared_identity(ctx)).await;
        let now = epoch_ms();
        let floor = u64::try_from(PR_INTERVAL.as_millis()).unwrap_or(u64::MAX);
        let all = match shared.plan(branches, now) {
            Plan::Hold => {
                self.problem = shared.problem.clone();
                self.github_due_ms = Some(shared.backoff_until_ms);
                return None;
            }
            Plan::Use => {
                if shared.note_asked(branches, now) {
                    save_shared(&path, &shared).await;
                }
                self.problem = None;
                self.github_due_ms = Some(shared.fetch_started_ms.saturating_add(floor));
                return shared.data.map(|data| GithubRead {
                    data,
                    fetch_started_ms: Some(shared.fetch_started_ms),
                });
            }
            // Waiting out the budget is not an answer: say so, and keep
            // only what cannot have changed, so the rest reads unknown.
            Plan::Paced { next_ms } => {
                if shared.note_asked(branches, now) {
                    save_shared(&path, &shared).await;
                }
                self.problem = Some(budget_problem(
                    next_ms,
                    wt_app_core::display_time::local_utc_offset_minutes(next_ms).unwrap_or(0),
                ));
                self.github_due_ms = Some(next_ms);
                return shared.data.map(|data| GithubRead {
                    data: settled_only(data),
                    fetch_started_ms: Some(shared.fetch_started_ms),
                });
            }
            Plan::Fetch(all) => all,
        };
        shared.note_asked(branches, now);
        shared.fetch_started_ms = now;
        let client = GithubClient::new(
            ctx.processes.clone(),
            ctx.config.paths.main_clone.clone(),
            GithubOptions::from_config(&ctx.config, true),
        );
        match client.fetch_worktrees(&all, &ctx.cancellation).await {
            Ok(data) => {
                shared.failures = 0;
                shared.backoff_until_ms = 0;
                shared.problem = None;
                shared.data = Some(data);
            }
            Err(error) => {
                let reset_at_ms = error.rate_limit_reset_ms();
                let error = error.to_string();
                let limited = wt_events::rate_limited(&error);
                shared.failures = shared.failures.saturating_add(1);
                let failed_at = epoch_ms();
                shared.backoff_until_ms = failed_at.saturating_add(wt_events::failure_wait_ms(
                    u64::try_from(PR_INTERVAL.as_millis()).unwrap_or(u64::MAX),
                    shared.failures,
                    limited,
                    reset_at_ms,
                    failed_at,
                ));
                shared.problem = Some(format!(
                    "GitHub could not be read: {}",
                    wt_core::sanitize_terminal_text(&error)
                ));
                shared.data = None;
            }
        }
        save_shared(&path, &shared).await;
        self.problem = shared.problem.clone();
        self.github_due_ms = Some(if shared.data.is_some() {
            shared.fetch_started_ms.saturating_add(floor)
        } else {
            shared.backoff_until_ms
        });
        shared.data.map(|data| GithubRead {
            data,
            fetch_started_ms: Some(shared.fetch_started_ms),
        })
    }
}

fn arm_key(slug: &str, arm: &wt_core::MergeArm) -> String {
    format!("{slug}|{}", arm.armed_at_ms)
}

/// Record what the fetch begun at `fetch_ms` showed of each arming, and
/// return those a second fetch has now shown GitHub not holding.
async fn settle_unheld(
    ctx: &AppContext,
    held: &[String],
    unheld: &[String],
    fetch_ms: u64,
) -> BTreeSet<String> {
    let Ok(_lock) = FileLock::acquire(
        &ctx.config.paths.lock_dir,
        SHARED_LOCK,
        "wait GitHub read",
        &ctx.cancellation,
    )
    .await
    else {
        return BTreeSet::new();
    };
    let path = shared_path(ctx);
    let mut shared = load_shared(&path, &shared_identity(ctx)).await;
    let mut changed = false;
    for key in held {
        changed |= shared.unheld.remove(key).is_some();
    }
    let mut dropped = BTreeSet::new();
    for key in unheld {
        let (twice, added) = shared.unheld_twice(key, fetch_ms);
        changed |= added;
        if twice {
            dropped.insert(key.clone());
        }
    }
    if changed {
        // An arming is cleared when its merge ends; its sighting is not.
        shared
            .unheld
            .retain(|_, first| fetch_ms.saturating_sub(*first) <= UNHELD_KEEP_MS);
        save_shared(&path, &shared).await;
    }
    dropped
}

fn now_ms() -> i64 {
    i64::try_from(epoch_ms()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_core::watch::{LocalClock, WatchActions, WatchCondition, parse};

    fn watch(slug: &str, conditions: &[&str]) -> Watch {
        let clock = LocalClock {
            now_ms: 0,
            utc_offset_minutes: 0,
        };
        Watch {
            id: "wait".into(),
            slug: slug.into(),
            incarnation: None,
            by: "one".into(),
            created_at_ms: 0,
            utc_offset_minutes: 0,
            when: conditions
                .iter()
                .map(|arg| parse(arg, clock).unwrap())
                .collect::<Vec<WatchCondition>>(),
            then: WatchActions::default(),
            met_at_ms: None,
            last_error: None,
            owner_gone: false,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn github_and_git_are_asked_only_for_what_conditions_name() {
        let asked = asked(&[watch(
            "one",
            &["ready", "wt:two:evicted", "three:merged", "pr:7", "5m"],
        )]);
        let of = |slug: &str| asked[slug];
        assert!(!of("one").pr && !of("one").landing);
        assert!(of("two").pr && of("two").queue && !of("two").landing);
        assert!(of("three").pr && of("three").landing && !of("three").queue);
        assert_eq!(asked.len(), 3);
    }

    fn covering(branches: &[&str], fetch_started_ms: u64) -> SharedRead {
        SharedRead {
            fetch_started_ms,
            data: Some(GithubData {
                covered_branches: branches.iter().map(|branch| (*branch).to_owned()).collect(),
                ..GithubData::default()
            }),
            ..SharedRead::default()
        }
    }

    #[test]
    fn waits_share_one_github_read_and_its_backoff() {
        let one = ["one".to_owned()];
        let minute = u64::try_from(PR_INTERVAL.as_millis()).unwrap();
        let start = 10 * minute;
        // Nothing stored: ask.
        assert_eq!(
            SharedRead::default().plan(&one, start),
            Plan::Fetch(vec!["one".into()])
        );
        // Another process's answer is used while it is young, whoever wrote it.
        let mut shared = covering(&["one", "two"], start);
        assert_eq!(shared.plan(&one, start + minute - 1), Plan::Use);
        assert_eq!(
            shared.plan(&one, start + minute),
            Plan::Fetch(vec!["one".into()])
        );
        // A clock that moved back does not make an answer last.
        assert!(matches!(shared.plan(&one, start - 1), Plan::Fetch(_)));
        // A branch it does not cover is asked for along with the branches
        // other waits asked about lately, so the next answer serves them all.
        shared.note_asked(&["two".to_owned()], start);
        shared.asked.insert("old".into(), 0);
        assert_eq!(
            shared.plan(&["three".to_owned()], start + 1),
            Plan::Fetch(vec!["three".into(), "two".into()])
        );
        // With only the reserve of GitHub's budget left, waits read once
        // per paced interval. The stored answer is current for its minute;
        // after that the plan names when the next read is due.
        let mut low = covering(&["one"], start);
        low.data.as_mut().unwrap().rate_limit = Some(wt_github::RateLimit {
            limit: 5_000,
            remaining: 1_000,
            cost: 5,
            reset_at_ms: start + 20 * minute,
        });
        assert_eq!(low.plan(&one, start + minute - 1), Plan::Use);
        assert_eq!(
            low.plan(&one, start + 14 * minute),
            Plan::Paced {
                next_ms: start + wt_events::MAX_PACED_WAIT_MS
            }
        );
        // A branch the stored answer lacks is still asked for.
        assert!(matches!(
            low.plan(&["new".to_owned()], start + 14 * minute),
            Plan::Fetch(_)
        ));
        // The reset is 20 minutes off, and the next read is not held for it.
        assert!(matches!(
            low.plan(&one, start + 15 * minute),
            Plan::Fetch(_)
        ));
        // A stored backoff holds a process that never saw the failure.
        let failed = SharedRead {
            fetch_started_ms: start,
            backoff_until_ms: start + 5 * minute,
            failures: 3,
            problem: Some("GitHub could not be read: 502".into()),
            ..SharedRead::default()
        };
        assert_eq!(failed.plan(&one, start + 5 * minute - 1), Plan::Hold);
        assert!(matches!(
            failed.plan(&one, start + 5 * minute),
            Plan::Fetch(_)
        ));
    }

    #[test]
    fn a_paced_answer_keeps_only_merges_and_says_when_the_next_read_is() {
        let pr = |state: &str| -> wt_github::PullRequest {
            serde_json::from_value(serde_json::json!({
                "number": 1, "url": "https://example.invalid/pull/1",
                "headRefName": "b", "headRefOid": "a".repeat(40),
                "baseRefName": "main", "mergeCommitOid": null,
                "title": "T", "isDraft": false, "state": state,
                "checks": "pass", "failedChecks": [], "review": "none",
                "reviewRequests": 0, "requestedReviewers": [], "suggestedReviewers": [],
                "comments": [], "unresolvedThreads": 0, "unresolvedThreadsTotal": 0
            }))
            .unwrap()
        };
        let mut data = GithubData {
            merge_queue_complete: true,
            ..GithubData::default()
        };
        for (branch, state) in [("done", "MERGED"), ("open", "OPEN"), ("shut", "CLOSED")] {
            data.prs.insert(branch.into(), pr(state));
            data.covered_branches.insert(branch.into());
        }
        data.covered_branches.insert("none".into());
        let kept = settled_only(data);
        assert_eq!(kept.prs.keys().collect::<Vec<_>>(), ["done"]);
        assert_eq!(kept.covered_branches.iter().collect::<Vec<_>>(), ["done"]);
        assert!(!kept.merge_queue_complete);
        // 13:05 UTC, read an hour east and half a day west.
        let at = (13 * 60 + 5) * 60_000;
        assert_eq!(
            budget_problem(at, 60),
            "GitHub budget low, next read at 14:05"
        );
        assert_eq!(
            budget_problem(at, -14 * 60),
            "GitHub budget low, next read at 23:05"
        );
    }

    #[test]
    fn an_asked_branch_is_rewritten_only_when_its_stamp_is_old() {
        let one = ["one".to_owned()];
        let mut shared = SharedRead::default();
        assert!(shared.note_asked(&one, 1_000));
        assert!(!shared.note_asked(&one, 1_000 + ASKED_REFRESH_MS - 1));
        shared.asked.insert("old".into(), 0);
        assert!(shared.note_asked(&one, 1_000 + ASKED_KEEP_MS));
        assert_eq!(shared.asked.keys().collect::<Vec<_>>(), ["one"]);
    }

    #[tokio::test]
    async fn the_shared_read_survives_the_process_and_an_unreadable_file_is_no_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wait-github.json");
        let empty = |repository: &str| SharedRead {
            repository: Some(repository.to_owned()),
            ..SharedRead::default()
        };
        assert_eq!(load_shared(&path, "repo:/a").await, empty("repo:/a"));
        let mut shared = covering(&["one"], 7);
        shared.repository = Some("repo:/a".into());
        shared.unheld.insert("one|5".into(), 7);
        save_shared(&path, &shared).await;
        assert_eq!(load_shared(&path, "repo:/a").await, shared);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        // Another repository's read of a branch with the same name, or one
        // stored before reads named their repository, answers nothing here.
        assert_eq!(load_shared(&path, "repo:/b").await, empty("repo:/b"));
        shared.repository = None;
        save_shared(&path, &shared).await;
        assert_eq!(load_shared(&path, "repo:/a").await, empty("repo:/a"));
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load_shared(&path, "repo:/a").await, empty("repo:/a"));
    }

    #[test]
    fn a_merge_is_dropped_only_when_a_second_fetch_shows_it_gone() {
        let mut shared = SharedRead::default();
        assert_eq!(shared.unheld_twice("one|5", 100), (false, true));
        // The same fetch read again, by this wait or another, proves nothing.
        assert_eq!(shared.unheld_twice("one|5", 100), (false, false));
        assert_eq!(shared.unheld_twice("one|5", 160), (true, false));
        // A new arming starts over.
        assert_eq!(shared.unheld_twice("one|900", 160), (false, true));
    }

    #[tokio::test]
    async fn rows_that_need_no_pull_request_are_read_without_github() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        let mut reader = RowReader::default();
        let read = reader
            .read(ctx, &[watch("one", &["ready", "wt:gone-one:gone"])])
            .await
            .unwrap();
        assert_eq!(read.rows["one"].ready, Some(false));
        assert!(read.rows["two"].exists);
        assert!(!read.rows.contains_key("gone-one"));
        assert_eq!(read.branches["one"], "feature/one");
        assert!(read.problem.is_none() && !shared_path(ctx).exists());
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn only_a_change_a_pass_would_read_differently_prompts_one() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        let watches = [watch("one", &["wt:two:ready"])];
        let title = |title: &'static str| {
            ctx.database.call(move |store| {
                store.set_slug_title("two", title, wt_store::TitleSource::Agent, None)?;
                Ok(())
            })
        };
        title("A title").await.unwrap();
        let mut signals = Signals::new(ctx);
        let mut reader = RowReader::default();
        let read = reader.read(ctx, &watches).await.unwrap();
        assert_eq!(read.rows["two"].ready, Some(false));
        let state = Changed {
            state: true,
            ..Changed::default()
        };

        // A write to the state is seen without a pass, and one about a
        // field no pass reads asks for none.
        title("Another title").await.unwrap();
        assert_eq!(signals.poll(), state);
        assert!(!reader.concerns(ctx, &watches, state).await);

        // The status of a worktree the watch names does.
        ctx.database
            .call(|store| {
                let ready = wt_core::WorkStatusRecord::new(
                    wt_core::WorkState::Ready,
                    "2026-11-27T12:00:00Z",
                );
                let stored: wt_store::WorkStatusRecord =
                    serde_json::from_value(serde_json::to_value(&ready)?)?;
                store.set_slug_work_status("two", Some(&stored))?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(signals.poll(), state);
        assert!(reader.concerns(ctx, &watches, state).await);
        let read = reader.read(ctx, &watches).await.unwrap();
        assert_eq!(read.rows["two"].ready, Some(true));
        assert!(!reader.concerns(ctx, &watches, state).await);
        assert!(!signals.poll().any());

        // A fetch concerns only a watch that asks about a landing.
        let fetched = Changed {
            git: true,
            ..Changed::default()
        };
        assert!(!reader.concerns(ctx, &watches, fetched).await);
        assert!(
            reader
                .concerns(ctx, &[watch("one", &["merged"])], fetched)
                .await
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_snapshot_write_prompts_a_pass_only_when_it_changes_what_is_asked() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let mut ctx = fixture.ctx.clone();
        let mut config = (*ctx.config).clone();
        config.github.events = Some(Default::default());
        ctx.config = std::sync::Arc::new(config);
        let ctx = &ctx;
        let events = ctx.config.paths.cache_root.join("events");
        std::fs::create_dir_all(&events).unwrap();
        let pr = |number: u64, branch: &str, checks: &str| {
            serde_json::json!({
                "number": number, "url": format!("https://example.invalid/pull/{number}"),
                "headRefName": branch, "baseRefName": "main", "title": "T",
                "isDraft": false, "state": "OPEN", "checks": checks, "failedChecks": [],
                "review": "none", "reviewRequests": 0, "requestedReviewers": [],
                "suggestedReviewers": [], "autoMerge": null, "comments": [],
                "unresolvedThreads": 0, "unresolvedThreadsTotal": 0,
                "mergedAt": null, "closedAt": null
            })
        };
        let write = |one: &str, two: &str| {
            let written = epoch_ms();
            let snapshot = serde_json::json!({
                "updatedAt": written, "fetchStartedAtMs": written - 1_000,
                "branches": ["feature/one", "feature/two"],
                "prs": {
                    "feature/one": pr(7, "feature/one", one),
                    "feature/two": pr(8, "feature/two", two),
                },
                "mergeQueue": {},
                "writerSha": wt_app_core::build_info::build_id(),
                "schema": wt_events::SNAPSHOT_SCHEMA,
                "schemaWriterSha": wt_app_core::build_info::build_id(),
                "repository": wt_ops::events::repository_identity(&ctx.config),
            });
            std::fs::write(events.join("github.json"), snapshot.to_string()).unwrap();
        };
        let github = Changed {
            github: true,
            ..Changed::default()
        };
        write("pending", "pending");
        for conditions in [&["green"][..], &["pr:7"][..]] {
            let watches = [watch("one", conditions)];
            let mut reader = RowReader::default();
            write("pending", "pending");
            let read = reader.read(ctx, &watches).await.unwrap();
            assert!(read.prs.contains_key(&7), "{conditions:?}");
            // The daemon wrote again with nothing new, then with news of
            // another worktree's pull request: no pass.
            write("pending", "pending");
            assert!(
                !reader.concerns(ctx, &watches, github).await,
                "{conditions:?}"
            );
            write("pending", "pass");
            assert!(
                !reader.concerns(ctx, &watches, github).await,
                "{conditions:?}"
            );
        }
        // News of the pull request a condition asks about is a reason.
        let watches = [watch("one", &["green"])];
        let mut reader = RowReader::default();
        reader.read(ctx, &watches).await.unwrap();
        write("pass", "pass");
        assert!(reader.concerns(ctx, &watches, github).await);
        let read = reader.read(ctx, &watches).await.unwrap();
        assert_eq!(read.rows["one"].green, Some(true));
        assert!(!reader.concerns(ctx, &watches, github).await);
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_pass_that_fails_does_not_leave_its_state_for_a_later_one() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        let watches = [watch("one", &["wt:two:ready"])];
        let status = |state: Option<wt_core::WorkState>| {
            ctx.database.call(move |store| {
                let stored = state
                    .map(|state| {
                        let record = wt_core::WorkStatusRecord::new(state, "2026-11-27T12:00:00Z");
                        serde_json::from_value::<wt_store::WorkStatusRecord>(serde_json::to_value(
                            &record,
                        )?)
                    })
                    .transpose()?;
                store.set_slug_work_status("two", stored.as_ref())?;
                Ok(())
            })
        };
        let mut reader = RowReader::default();
        reader.read(ctx, &watches).await.unwrap();
        status(Some(wt_core::WorkState::Ready)).await.unwrap();
        let state = Changed {
            state: true,
            ..Changed::default()
        };
        assert!(reader.concerns(ctx, &watches, state).await);
        // The pass that change asked for cannot list the worktrees.
        let mut failing = ctx.clone();
        failing.cancellation = ctx.cancellation.child_token();
        failing.cancellation.cancel();
        assert!(reader.read(&failing, &watches).await.is_err());
        // The next one reads the state as it is by then.
        status(Some(wt_core::WorkState::Working)).await.unwrap();
        let read = reader.read(ctx, &watches).await.unwrap();
        assert_eq!(read.rows["two"].ready, Some(false));
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_fetch_run_in_a_worktree_is_seen() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&ctx.cwd)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
        };
        let origin = fixture._root.path().join("origin.git");
        git(&["init", "--bare", origin.to_str().unwrap()]);
        git(&["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&["push", "origin", "main:refs/heads/main"]);
        git(&["fetch", "origin"]);
        let mut signals = Signals::new(ctx);
        assert!(!signals.poll().any());
        // The base moves on origin, and a fetch in the worktree (not in the
        // main clone) brings it in.
        std::fs::write(ctx.cwd.join("work.txt"), "work\n").unwrap();
        git(&["add", "work.txt"]);
        git(&["commit", "-m", "work"]);
        git(&["push", "origin", "HEAD:refs/heads/main"]);
        // The push moved the tracking ref too; put it back so the fetch is
        // what moves it.
        git(&["update-ref", "refs/remotes/origin/main", "main"]);
        signals.poll();
        git(&["fetch", "origin"]);
        assert!(signals.poll().git);
        fixture.close().await.unwrap();
    }

    #[test]
    fn a_queue_removal_is_a_reason_only_after_the_arming() {
        let pr = |removal: serde_json::Value| -> wt_github::PullRequest {
            serde_json::from_value(serde_json::json!({
                "number": 7, "url": "https://example.invalid/pull/7",
                "headRefName": "b", "headRefOid": "a".repeat(40),
                "baseRefName": "main", "mergeCommitOid": null,
                "title": "T", "isDraft": false, "state": "OPEN",
                "checks": "pass", "failedChecks": [], "review": "none",
                "reviewRequests": 0, "requestedReviewers": [], "suggestedReviewers": [],
                "comments": [], "unresolvedThreads": 0, "unresolvedThreadsTotal": 0,
                "queueRemoval": removal,
            }))
            .unwrap()
        };
        let arm = |pr: u64| -> wt_core::MergeArm {
            serde_json::from_value(
                serde_json::json!({"pr": pr, "armedAtMs": 1_000, "mechanism": "queue"}),
            )
            .unwrap()
        };
        let removed =
            |reason: &str, at_ms: u64| pr(serde_json::json!({"reason": reason, "atMs": at_ms}));
        let out = Some(false);
        assert_eq!(
            queue_removal(&removed("merge_conflict", 2_000), Some(&arm(7)), out).as_deref(),
            Some("removed from the merge queue: merge_conflict")
        );
        assert_eq!(
            queue_removal(&removed("", 2_000), Some(&arm(7)), out).as_deref(),
            Some("removed from the merge queue")
        );
        // An earlier round's removal, another pull request's arming, no
        // arming, no removal.
        assert_eq!(
            queue_removal(&removed("ci", 1_000), Some(&arm(7)), out),
            None
        );
        assert_eq!(
            queue_removal(&removed("ci", 2_000), Some(&arm(8)), out),
            None
        );
        assert_eq!(queue_removal(&removed("ci", 2_000), None, out), None);
        assert_eq!(
            queue_removal(&pr(serde_json::Value::Null), Some(&arm(7)), out),
            None
        );
        // Queued again, perhaps queued (the read held part of the queue),
        // re-armed on GitHub with no arm record of wt's, merged or closed:
        // the old removal is history.
        let again = removed("merge_conflict", 2_000);
        assert_eq!(queue_removal(&again, Some(&arm(7)), Some(true)), None);
        assert_eq!(queue_removal(&again, Some(&arm(7)), None), None);
        let mut rearmed = again.clone();
        rearmed.auto_merge = serde_json::from_value(
            serde_json::json!({"enabledAt": "2026-11-27T00:00:00Z", "mergeMethod": "SQUASH"}),
        )
        .unwrap();
        assert!(rearmed.auto_merge.is_some());
        assert_eq!(queue_removal(&rearmed, Some(&arm(7)), out), None);
        let mut merged = again;
        merged.state = "MERGED".into();
        assert_eq!(queue_removal(&merged, Some(&arm(7)), out), None);
    }

    #[tokio::test]
    async fn a_removal_of_a_named_worktree_prompts_a_pass() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        let watches = [watch("one", &["wt:two:gone"])];
        let mut signals = Signals::new(ctx);
        let mut reader = RowReader::default();
        reader.read(ctx, &watches).await.unwrap();
        let state = Changed {
            state: true,
            ..Changed::default()
        };
        // The removal record is written beside the worktree's own record,
        // which stays as it was.
        let removed = |slug: &'static str| {
            ctx.database.call(move |store| {
                let entry: wt_store::RemovedWorktree = serde_json::from_value(serde_json::json!({
                    "slug": slug, "branch": format!("feature/{slug}"),
                    "removedAt": "2026-11-27T12:00:00Z", "landedOnAtRemoval": "base",
                }))?;
                store.record_removed_worktrees(&[entry], 1_795_000_000_000)?;
                Ok(())
            })
        };
        removed("three").await.unwrap();
        assert!(!reader.concerns(ctx, &watches, state).await);
        removed("two").await.unwrap();
        assert!(reader.concerns(ctx, &watches, state).await);
        let read = reader.read(ctx, &watches).await.unwrap();
        assert_eq!(read.removed.get("two").unwrap().merged, Some(true));
        assert!(!reader.concerns(ctx, &watches, state).await);

        // The worktree list changing is seen, and concerns a watch that
        // names a worktree and no other.
        signals.poll();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&ctx.config.paths.main_clone)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
        };
        let two = ctx.config.paths.worktree_root.join("two");
        git(&["worktree", "remove", "--force", two.to_str().unwrap()]);
        let seen = signals.poll();
        assert!(seen.inventory, "{seen:?}");
        let listing = Changed {
            inventory: true,
            ..Changed::default()
        };
        assert!(reader.concerns(ctx, &watches, listing).await);
        assert!(
            !reader
                .concerns(ctx, &[watch("one", &["pr:7"])], listing)
                .await
        );
        let read = reader.read(ctx, &watches).await.unwrap();
        assert!(!read.rows.contains_key("two"));
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_display_read_is_saved_for_the_next_and_used_only_while_fresh() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        let one = ["feature/one".to_owned()];
        let both = ["feature/one".to_owned(), "feature/two".to_owned()];
        assert!(recent_read(ctx, &one).await.is_none());
        let read = |branches: &[&str]| GithubData {
            covered_branches: branches.iter().map(|branch| (*branch).to_owned()).collect(),
            ..GithubData::default()
        };
        let now = epoch_ms();
        record_read(ctx, &read(&["feature/one"]), now - 5_000).await;
        let (data, started) = recent_read(ctx, &one).await.unwrap();
        assert_eq!(started, now - 5_000);
        assert!(data.covered_branches.contains("feature/one"));
        // A read that does not cover every branch asked about is no answer.
        assert!(recent_read(ctx, &both).await.is_none());
        // An older read does not replace a newer one; a newer one does.
        record_read(ctx, &read(&["feature/one", "feature/two"]), now - 60_000).await;
        assert!(recent_read(ctx, &both).await.is_none());
        record_read(ctx, &read(&["feature/one", "feature/two"]), now - 1_000).await;
        assert_eq!(recent_read(ctx, &both).await.unwrap().1, now - 1_000);
        // The board's own saved read counts the same, by when it began.
        let save_board = |data: &GithubData| {
            std::fs::create_dir_all(ctx.caches.directory()).unwrap();
            let saved = serde_json::json!({
                "format": 1, "name": "github", "scope": ctx.caches.scope(),
                "saved_at_ms": now, "data": data,
            });
            std::fs::write(
                ctx.caches.directory().join("github.json"),
                saved.to_string(),
            )
            .unwrap();
        };
        let mut board = read(&["feature/one", "feature/two"]);
        board.observed_at_ms = Some(now - 500);
        save_board(&board);
        assert_eq!(recent_read(ctx, &both).await.unwrap().1, now - 500);
        // Past the bound neither is shown in place of a read.
        let fresh = u64::try_from(crate::github_events_source::FRESH.as_millis()).unwrap();
        board.observed_at_ms = Some(now - fresh - 60_000);
        save_board(&board);
        assert_eq!(recent_read(ctx, &both).await.unwrap().1, now - 1_000);
        let path = shared_path(ctx);
        let mut shared = load_shared(&path, &shared_identity(ctx)).await;
        shared.fetch_started_ms = now - fresh - 60_000;
        save_shared(&path, &shared).await;
        assert!(recent_read(ctx, &both).await.is_none());
        // A board read that cannot say when it was observed is not dated by
        // when its file was saved.
        board.observed_at_ms = None;
        save_board(&board);
        assert!(board_read(ctx).await.is_none());

        // A read narrower than the stored one would leave a wait's branches
        // uncovered and send it to GitHub ahead of its pace: it is not saved.
        record_read(ctx, &read(&["feature/one"]), now).await;
        let kept = load_shared(&path, &shared_identity(ctx)).await;
        assert_eq!(kept.fetch_started_ms, now - fresh - 60_000);
        assert_eq!(kept.data.as_ref().unwrap().covered_branches.len(), 2);
        // Neither is one that misses a branch a wait asked about lately.
        let mut asking = kept.clone();
        asking.asked.insert("feature/three".into(), now - 1_000);
        save_shared(&path, &asking).await;
        record_read(ctx, &read(&["feature/one", "feature/two"]), now).await;
        assert_eq!(load_shared(&path, &shared_identity(ctx)).await, asking);
        // A read as wide as both is, and a wait finds it as its own answer.
        let wide = read(&["feature/one", "feature/two", "feature/three"]);
        record_read(ctx, &wide, now).await;
        let shared = load_shared(&path, &shared_identity(ctx)).await;
        assert_eq!(shared.fetch_started_ms, now);
        assert_eq!(shared.plan(&both, now + 1_000), Plan::Use);
        // While a wait holds the lock across its fetch, nothing waits for
        // it and nothing is saved.
        let held = FileLock::try_acquire(&ctx.config.paths.lock_dir, SHARED_LOCK, "test")
            .await
            .unwrap()
            .unwrap();
        let began = std::time::Instant::now();
        record_read(ctx, &wide, now + 5_000).await;
        assert!(began.elapsed() < Duration::from_secs(5));
        drop(held);
        assert_eq!(
            load_shared(&path, &shared_identity(ctx))
                .await
                .fetch_started_ms,
            now
        );
        fixture.close().await.unwrap();
    }
}
