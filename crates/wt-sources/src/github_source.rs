//! The GitHub leaf's scheduling: which branches to read, when to serve the
//! events daemon's snapshot instead of reading GitHub, and the network rate
//! floor with its deferred reads.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};
use tokio_util::sync::CancellationToken;
use wt_core::Domain;
use wt_github::{GithubClient, GithubData, GithubError, GithubOptions};
use wt_runtime::{
    DiskCache, Fetched, LeafHandle, Persistence, RefreshPolicy, Request, SourceHandle, SourceSpec,
    Stamp, TaskScope, start_requested_source,
};
use wt_vcs::WorktreeSnapshot;

use crate::{github_events_source::DaemonRead, local_source::Metadata};
use wt_app_core::context::AppContext;

/// Network reads of GitHub, measured from the prior read's start.
const GITHUB_MINIMUM: Duration = Duration::from_secs(10);
/// A direct read at least this long is logged at info level. An ordinary
/// read of a few dozen branches takes six to ten seconds.
const SLOW_READ: Duration = Duration::from_secs(15);
/// Rereads of the events daemon's local snapshot file.
const DAEMON_MINIMUM: Duration = Duration::from_millis(100);

/// Refresh requests for the GitHub leaf, including the one-shot request to
/// skip the events daemon's snapshot.
#[derive(Clone)]
pub struct GithubRefresh {
    github: LeafHandle<GithubData>,
    reads: Arc<Mutex<ReadTimes>>,
}

/// The latest direct GitHub read, for explaining a write that expired.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReadTimes {
    pub started: Option<Stamp>,
    /// When it finished, successfully or not; `None` while it is running.
    pub finished: Option<Stamp>,
}

impl GithubRefresh {
    /// The latest live GitHub read, for settling this host's own writes.
    pub fn confirmed(&self) -> Option<wt_runtime::Confirmed<GithubData>> {
        self.github.confirmed()
    }

    pub fn refresh(&self) {
        self.github.refresh();
    }

    /// The latest direct read's start and finish.
    pub fn last_read(&self) -> ReadTimes {
        *self
            .reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// After an accepted GitHub write: the read that may confirm it must be a
    /// direct fetch stamped here, not the events daemon's older snapshot.
    pub fn refresh_live(&self) {
        self.github.request(Request::Direct);
    }

    #[cfg(test)]
    pub fn detached() -> Self {
        let (github, _) = wt_runtime::leaf_channel(
            &TaskScope::new(),
            SourceSpec {
                name: "github",
                domain: Domain::Github,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        Self {
            github,
            reads: Arc::default(),
        }
    }
}

/// The one GitHub leaf. Each fetch chooses between the events daemon's
/// snapshot and a direct read; both publish the moment their GitHub request
/// started, so neither can claim to have seen a later write.
pub fn start(
    scope: &TaskScope,
    context: &AppContext,
    git: LeafHandle<Vec<WorktreeSnapshot>>,
    metadata: LeafHandle<Metadata>,
    enabled: bool,
    backstop: Duration,
) -> (LeafHandle<GithubData>, GithubRefresh) {
    let lane = Arc::new(GithubLane::new(
        context.clone(),
        git.clone(),
        metadata.clone(),
        enabled,
    ));
    let github = start_requested_source(
        scope,
        SourceSpec {
            name: "github",
            domain: Domain::Github,
            persistence: Persistence::Disk(DiskCache::new(&context.caches)),
            policy: RefreshPolicy {
                debounce: Duration::from_millis(100),
                minimum_interval: DAEMON_MINIMUM,
            },
        },
        {
            let lane = lane.clone();
            move |request, cancel| {
                let lane = lane.clone();
                async move { lane.fetch(request, &cancel).await }
            }
        },
    );
    let refresh = GithubRefresh {
        github: github.clone(),
        reads: lane.reads.clone(),
    };
    spawn_deferred_reads(scope, &lane, github.view());
    spawn_github_driver(
        scope,
        git.clone(),
        metadata.clone(),
        github.view(),
        backstop,
    );
    spawn_other_branch_recorder(scope, context, git, metadata, github.clone());
    if enabled {
        crate::github_events_source::watch(scope, context, github.view());
    }
    (github, refresh)
}

struct GithubLane {
    context: AppContext,
    git: LeafHandle<Vec<WorktreeSnapshot>>,
    metadata: LeafHandle<Metadata>,
    enabled: bool,
    clock: Mutex<LaneClock>,
    reads: Arc<Mutex<ReadTimes>>,
    wake: Notify,
}

#[derive(Default)]
struct LaneClock {
    /// Start of the last network read, successful or not: the rate floor.
    live_started: Option<Instant>,
    /// Start of the last successful network read. Daemon data whose request
    /// began earlier is older than what is shown.
    live_observed: Option<Instant>,
    /// The daemon file version and branch set currently published, so an
    /// unchanged file is not republished on every recheck.
    served_daemon: Option<(u64, Vec<String>)>,
    /// The branch set the published data covers.
    published: Option<Vec<String>>,
    /// The published data and its observation stamp, so an older daemon
    /// snapshot can contribute only branches the shown data lacks.
    shown: Option<(GithubData, Stamp)>,
    /// A network read the floor postponed is due then.
    deferred: Option<Instant>,
    /// A `Request::Direct` has been made and no network read has started
    /// since: daemon snapshots are not served until one does.
    direct_owed: bool,
    /// The floor between background network reads that the latest budget
    /// reading asks for, when that is longer than [`GITHUB_MINIMUM`].
    pace: Option<Duration>,
    /// The budget windows the reads so far reported: the pace follows the
    /// one with the least left, not the one the latest read happened to see.
    budget: wt_events::BudgetWindows,
    /// The daemon file version whose scoped reads were last merged into the
    /// shown data.
    merged_daemon: Option<u64>,
}

/// What one fetch does.
enum Plan {
    Skip,
    /// The floor postponed a network read; it is recorded in `deferred`.
    Defer,
    Publish(Fetched<GithubData>),
    Network,
}

/// One fetch's inputs besides the clock.
struct PlanInput<'a> {
    branches: &'a [String],
    daemon: Option<DaemonRead>,
    /// The strongest reason among the requests this fetch serves. Only a
    /// fetch asked for nothing but a recheck of the daemon snapshot may
    /// decline to read GitHub when that snapshot is unusable.
    request: Request,
    now: Instant,
}

impl LaneClock {
    /// Decide this fetch without awaiting: serve the daemon snapshot, skip,
    /// postpone a network read behind the floor, or read GitHub now.
    fn plan(&mut self, input: PlanInput<'_>) -> Plan {
        let PlanInput {
            branches,
            daemon,
            request,
            now,
        } = input;
        let same_branches = self.published.as_deref() == Some(branches);
        if let Some(read) = daemon {
            let newer = match (read.started, self.live_observed) {
                (_, None) => true,
                (Some(started), Some(live)) => started >= live,
                (None, Some(_)) => false,
            };
            if !newer && same_branches {
                // Fresh daemon data exists and the shown data is newer still,
                // except for branches the daemon has read again since.
                if self.merged_daemon == Some(read.written_ms) {
                    return Plan::Skip;
                }
                self.merged_daemon = Some(read.written_ms);
                let merged = match (&self.shown, self.live_observed) {
                    (Some(shown), Some(live)) => merge_newer(shown, &read, live),
                    _ => None,
                };
                let Some((data, observed)) = merged else {
                    return Plan::Skip;
                };
                self.pace_by(data.rate_limit);
                self.shown = Some((data.clone(), observed));
                return Plan::Publish(Fetched::new(data, observed));
            }
            let version = (read.written_ms, branches.to_vec());
            if self.served_daemon.as_ref() == Some(&version) {
                return Plan::Skip;
            }
            let fetched = if newer {
                let observed = read.stamp();
                self.pace_by(read.data.rate_limit);
                Fetched::new(read.data, observed)
            } else if let Some(shown) = &self.shown {
                // The branch set changed but the shown data is newer: keep it
                // for the branches it covers and take only the new branches
                // from the older snapshot, so a newer read never regresses.
                let (data, observed) = merge_older(shown, read, branches);
                Fetched::new(data, observed)
            } else {
                // Nothing shown to keep: wait for a direct read.
                return self.network(now, request);
            };
            self.served_daemon = Some(version);
            self.published = Some(branches.to_vec());
            self.shown = Some((fetched.data.clone(), fetched.observed));
            return Plan::Publish(fetched);
        }
        // A recheck that found nothing usable only reads GitHub when the
        // daemon data it had shown has aged out or the branch set changed.
        if request == Request::Recheck
            && !self.direct_owed
            && same_branches
            && self.served_daemon.is_none()
        {
            return Plan::Skip;
        }
        self.network(now, request)
    }

    /// Read GitHub now, or defer behind the rate floor. A read the user
    /// asked for (an explicit refresh, or the one after a write) waits only for
    /// [`GITHUB_MINIMUM`]; any other also keeps the budget's pace.
    fn network(&mut self, now: Instant, request: Request) -> Plan {
        let floor = if self.direct_owed || request == Request::Direct {
            GITHUB_MINIMUM
        } else {
            self.pace.unwrap_or(GITHUB_MINIMUM).max(GITHUB_MINIMUM)
        };
        if let Some(due) = self
            .live_started
            .map(|at| at + floor)
            .filter(|due| *due > now)
        {
            self.deferred = Some(due);
            return Plan::Defer;
        }
        self.live_started = Some(now);
        self.served_daemon = None;
        self.deferred = None;
        self.direct_owed = false;
        Plan::Network
    }

    /// Pace background reads by the budget a read just reported.
    fn pace_by(&mut self, rate: Option<wt_github::RateLimit>) {
        let floor = u64::try_from(GITHUB_MINIMUM.as_millis()).unwrap_or(u64::MAX);
        let now = epoch_ms();
        let budget = match rate {
            Some(rate) => self.budget.observe(wt_ops::events::rate_budget(rate), now),
            // A server that reports no budget has none to keep to.
            None => {
                self.budget = wt_events::BudgetWindows::default();
                None
            }
        };
        self.pace = Some(Duration::from_millis(wt_events::paced_interval_ms(
            floor, budget, now,
        )));
    }

    /// A direct read was refused for the rate limit: background reads wait
    /// for its reset when the reply named one, and otherwise as long as
    /// every other reader backs off from a limit it cannot time.
    fn rate_limited(&mut self, reset_at_ms: Option<u64>) {
        let floor = u64::try_from(GITHUB_MINIMUM.as_millis()).unwrap_or(u64::MAX);
        self.pace = Some(Duration::from_millis(wt_events::failure_wait_ms(
            floor,
            1,
            true,
            reset_at_ms,
            epoch_ms(),
        )));
    }

    /// A direct read that started at `started` succeeded.
    fn live_read(&mut self, started: Instant, branches: Vec<String>, data: &GithubData) {
        self.pace_by(data.rate_limit);
        self.live_observed = Some(started);
        self.published = Some(branches);
        self.shown = Some((data.clone(), Stamp::at(started)));
        // This read may have begun before a scoped read the daemon's file
        // already holds, and has just replaced what was merged from it.
        self.merged_daemon = None;
    }
}

/// The shown data for the branches it covers, plus `older`'s entries for the
/// branches it does not. Both inputs are complete reads of their branch sets.
/// The result is stamped with the earlier observation when it takes anything
/// from `older`, so it never claims to have seen a later write.
fn merge_older(
    (shown, shown_at): &(GithubData, Stamp),
    older: DaemonRead,
    branches: &[String],
) -> (GithubData, Stamp) {
    let mut data = shown.clone();
    data.prs.retain(|branch, _| branches.contains(branch));
    let added: Vec<&String> = branches
        .iter()
        .filter(|branch| !shown.covered_branches.contains(*branch))
        .collect();
    let observed = if added.is_empty() {
        *shown_at
    } else {
        // The combined data is only as recent as its older part, and has no
        // known time when either part has none.
        data.observed_at_ms = shown
            .observed_at_ms
            .zip(older.data.observed_at_ms)
            .map(|(shown, older)| shown.min(older));
        shown_at.earliest(older.stamp())
    };
    for branch in added {
        if let Some(pr) = older.data.prs.get(branch) {
            data.prs.insert(branch.clone(), pr.clone());
        }
    }
    // The shown read's repository-wide merge queue is the newer one.
    data.covered_branches = branches.iter().cloned().collect();
    (data, observed)
}

/// The shown data with the branches the daemon has read again since the
/// shown read began at `live`: each takes the daemon's newer answer, a
/// missing pull request included. Every other branch, and the merge queue,
/// stay as the shown read left them, and so does its stamp: nothing here is
/// older than that read, and nothing claims to be newer. `None` when the
/// daemon has nothing newer or it changes nothing.
fn merge_newer(
    (shown, shown_at): &(GithubData, Stamp),
    read: &DaemonRead,
    live: Instant,
) -> Option<(GithubData, Stamp)> {
    let mut data = shown.clone();
    for (branch, read_at) in &read.scoped {
        if *read_at < live || !shown.covered_branches.contains(branch) {
            continue;
        }
        match read.data.prs.get(branch) {
            Some(pr) => data.prs.insert(branch.clone(), pr.clone()),
            None => data.prs.remove(branch),
        };
    }
    if data.prs == shown.prs {
        return None;
    }
    data.rate_limit = read.data.rate_limit.or(shown.rate_limit);
    Some((data, *shown_at))
}

impl GithubLane {
    fn new(
        context: AppContext,
        git: LeafHandle<Vec<WorktreeSnapshot>>,
        metadata: LeafHandle<Metadata>,
        enabled: bool,
    ) -> Self {
        Self {
            context,
            git,
            metadata,
            enabled,
            clock: Mutex::default(),
            reads: Arc::default(),
            wake: Notify::new(),
        }
    }

    fn clock(&self) -> std::sync::MutexGuard<'_, LaneClock> {
        self.clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record_read(&self, times: ReadTimes) {
        *self
            .reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = times;
    }

    async fn fetch(
        &self,
        request: Request,
        cancel: &CancellationToken,
    ) -> Result<Option<Fetched<GithubData>>, GithubError> {
        // Recorded before any early return: a direct read stays owed until
        // one starts, however many fetches skip or defer first.
        let bypass = {
            let mut clock = self.clock();
            clock.direct_owed |= request == Request::Direct;
            clock.direct_owed
        };
        // The key set comes from Git. Restored or missing inventory cannot
        // name the branches to read; the inventory's next change re-arms this.
        let Some(inventory) = self.git.confirmed() else {
            return Ok(None);
        };
        // Each row's recorded other branches come from wt's state, under
        // the same rule: an unread state cannot name them.
        let Some(state) = self.metadata.confirmed() else {
            return Ok(None);
        };
        let branches = inventory_branches(&inventory, &state.0);
        if !self.enabled || branches.is_empty() {
            let mut clock = self.clock();
            if clock.published.as_ref() == Some(&branches) {
                return Ok(None);
            }
            clock.published = Some(branches);
            clock.served_daemon = None;
            clock.shown = None;
            return Ok(Some(Fetched::new(GithubData::default(), Stamp::now())));
        }
        let daemon = if bypass {
            None
        } else {
            crate::github_events_source::load(&self.context, &branches).await
        };
        let now = Instant::now();
        let plan = self.clock().plan(PlanInput {
            branches: &branches,
            daemon,
            request,
            now,
        });
        match plan {
            Plan::Skip => return Ok(None),
            Plan::Defer => {
                self.wake.notify_one();
                return Ok(None);
            }
            Plan::Publish(fetched) => return Ok(Some(fetched)),
            Plan::Network => {}
        }
        let config = &self.context.config;
        let ci = has_workflows(&config.paths.main_clone).await;
        let client = GithubClient::new(
            self.context.processes.clone(),
            config.paths.main_clone.clone(),
            GithubOptions::from_config(config, ci),
        );
        let started = Stamp::at(now);
        self.record_read(ReadTimes {
            started: Some(started),
            finished: None,
        });
        let result = client.fetch_worktrees(&branches, cancel).await;
        let elapsed = now.elapsed();
        self.record_read(ReadTimes {
            started: Some(started),
            finished: Some(Stamp::now()),
        });
        let failed = result.as_ref().err().map(ToString::to_string);
        let started_at = started_at(elapsed);
        if elapsed >= SLOW_READ {
            tracing::info!(
                branches = branches.len(),
                elapsed_ms = elapsed.as_millis() as u64,
                started_at,
                error = failed.as_deref(),
                "slow GitHub read"
            );
        } else {
            tracing::debug!(
                branches = branches.len(),
                elapsed_ms = elapsed.as_millis() as u64,
                started_at,
                error = failed.as_deref(),
                "GitHub read"
            );
        }
        let data = match result {
            Ok(data) => data,
            Err(error) => {
                if matches!(error, GithubError::RateLimit { .. }) {
                    self.clock().rate_limited(error.rate_limit_reset_ms());
                }
                return Err(error);
            }
        };
        self.clock().live_read(now, branches, &data);
        Ok(Some(Fetched::new(data, started)))
    }
}

/// Requests the network read the rate floor postponed, once it is due.
fn spawn_deferred_reads(
    scope: &TaskScope,
    lane: &Arc<GithubLane>,
    github: SourceHandle<GithubData>,
) {
    let lane = lane.clone();
    let cancel = scope.token();
    scope.spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = lane.wake.notified() => {}
            }
            // A read that ran meanwhile clears the deferral; never fire twice.
            loop {
                let deferred = lane.clock().deferred;
                let Some(due) = deferred else { break };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep_until(due) => {}
                }
                let mut clock = lane.clock();
                if clock.deferred.is_some_and(|due| due <= Instant::now()) {
                    clock.deferred = None;
                    drop(clock);
                    github.refresh();
                    break;
                }
            }
        }
    });
}

/// Rereads GitHub when the confirmed branch set changes (including when Git
/// and wt's state first go live) and on the slow backstop.
fn spawn_github_driver(
    scope: &TaskScope,
    git: LeafHandle<Vec<WorktreeSnapshot>>,
    metadata: LeafHandle<Metadata>,
    github: SourceHandle<GithubData>,
    backstop: Duration,
) {
    let cancel = scope.token();
    scope.spawn(async move {
        let mut updates = git.subscribe();
        let mut states = metadata.subscribe();
        updates.mark_changed();
        let mut previous: Option<Vec<String>> = None;
        let mut interval = tokio::time::interval_at(Instant::now() + backstop, backstop);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                changed = updates.changed() => {
                    if changed.is_err() { break; }
                    updates.borrow_and_update();
                }
                changed = states.changed() => {
                    if changed.is_err() { break; }
                    states.borrow_and_update();
                }
                _ = interval.tick() => { github.refresh(); continue; }
            }
            let (Some(inventory), Some(state)) = (git.confirmed(), metadata.confirmed()) else {
                continue;
            };
            let next = inventory_branches(&inventory, &state.0);
            if previous.as_ref() != Some(&next) {
                previous = Some(next);
                github.refresh();
            }
        }
    });
}

async fn has_workflows(root: &Path) -> bool {
    let mut entries = match tokio::fs::read_dir(root.join(".github/workflows")).await {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, "cannot inspect workflow files");
            }
            return false;
        }
    };
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                if matches!(
                    entry.path().extension().and_then(|s| s.to_str()),
                    Some("yml" | "yaml")
                ) {
                    return true;
                }
            }
            Ok(None) => return false,
            Err(error) => {
                tracing::warn!(%error, "cannot inspect workflow files");
                return false;
            }
        }
    }
}

/// When a read that has run for `elapsed` began, as a UTC time of day.
fn started_at(elapsed: Duration) -> String {
    let ms = epoch_ms().saturating_sub(elapsed.as_millis().min(u128::from(u64::MAX)) as u64);
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .and_then(|at| {
            at.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| ms.to_string())
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_millis().min(u128::from(u64::MAX)) as u64
        })
}

/// Records what each live GitHub read says of the worktrees' recorded other
/// branches (see `wt_ops::other_branches`). The record is what the board
/// shows and what removal checks under its lock, so the batched read's
/// answer has to reach it without waiting for someone to ask for a removal.
fn spawn_other_branch_recorder(
    scope: &TaskScope,
    context: &AppContext,
    git: LeafHandle<Vec<WorktreeSnapshot>>,
    metadata: LeafHandle<Metadata>,
    github: LeafHandle<GithubData>,
) {
    let cancel = scope.token();
    let context = context.clone();
    scope.spawn(async move {
        let mut updates = github.subscribe();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                changed = updates.changed() => if changed.is_err() { break; },
            }
            let snapshot = updates.borrow_and_update().clone();
            // A failed fetch keeps its last data, and restored data was
            // never read by this process: neither is an answer from now.
            let Some(data) = github
                .confirm(&snapshot)
                .filter(|_| matches!(snapshot.state(), wt_runtime::SourceState::Ready))
            else {
                continue;
            };
            let (Some(inventory), Some(state)) = (git.confirmed(), metadata.confirmed()) else {
                continue;
            };
            let rows: Vec<wt_vcs::WorktreeRecord> = inventory
                .iter()
                .map(|snapshot| &snapshot.worktree)
                .filter(|row| {
                    !row.is_main
                        && wt_core::stored_other_branches(&state.0["slugs"][row.target.slug()])
                            .iter()
                            .any(|other| data.covered_branches.contains(&other.branch))
                })
                .cloned()
                .collect();
            if rows.is_empty() {
                continue;
            }
            wt_ops::other_branches::observe(
                &context,
                &rows,
                &state.0,
                Some(&data),
                wt_ops::other_branches::Ask::Nothing,
            )
            .await;
        }
    });
}

/// Branches GitHub is asked about: every linked worktree's branch and the
/// other branches `state` records for it.
fn inventory_branches(inventory: &[WorktreeSnapshot], state: &serde_json::Value) -> Vec<String> {
    wt_ops::other_branches::github_branches(
        inventory.iter().map(|snapshot| &snapshot.worktree),
        state,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_runtime::leaf_channel;

    fn spec<T>(name: &'static str, domain: Domain) -> SourceSpec<T> {
        SourceSpec {
            name,
            domain,
            persistence: Persistence::Never("test fixture"),
            policy: RefreshPolicy::default(),
        }
    }

    fn worktree(slug: &str, branch: &str, is_main: bool) -> WorktreeSnapshot {
        let target = wt_core::local_worktree_target(slug, branch, format!("/tmp/{slug}"), "");
        WorktreeSnapshot {
            worktree: wt_vcs::WorktreeRecord {
                target,
                is_main,
                head_sha: None,
                detached: false,
                kind: wt_vcs::RepositoryKind::LinkedWorktree,
                locked: false,
                prunable: false,
                git_dir: None,
                common_dir: None,
            },
            status: None,
            error: None,
        }
    }

    /// The branch list comes from confirmed Git inventory. While Git has not
    /// been read live, the GitHub fetch skips instead of reading GitHub for a
    /// guessed (empty or restored) branch set.
    #[tokio::test]
    async fn github_fetch_skips_while_git_inventory_is_unconfirmed() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let (git, git_publisher) = leaf_channel(&scope, spec("git", Domain::Git));
        let (metadata, metadata_publisher) =
            leaf_channel(&scope, spec("metadata", Domain::Metadata));
        // Disabled GitHub still proves the gate without a network client.
        let lane = GithubLane::new(fixture.ctx.clone(), git, metadata, false);
        let cancel = CancellationToken::new();
        assert!(
            lane.fetch(Request::Refresh, &cancel)
                .await
                .unwrap()
                .is_none()
        );
        git_publisher.publish_live(
            vec![
                worktree("main", "main", true),
                worktree("one", "feature/one", false),
            ],
            Stamp::now(),
        );
        // The recorded other branches are part of the key set, so an unread
        // state holds the fetch as an unread inventory does.
        assert!(
            lane.fetch(Request::Refresh, &cancel)
                .await
                .unwrap()
                .is_none()
        );
        metadata_publisher.publish_live(
            (
                serde_json::json!({"slugs": {"one": {"otherBranches": {"feature/extra": {}}}}}),
                Default::default(),
            ),
            Stamp::now(),
        );
        let fetched = lane
            .fetch(Request::Refresh, &cancel)
            .await
            .unwrap()
            .expect("confirmed Git and state fetch");
        assert!(fetched.data.prs.is_empty());
        assert_eq!(
            *lane.clock().published.as_ref().unwrap(),
            ["feature/extra", "feature/one"]
        );
        // The same branch set does not republish.
        assert!(
            lane.fetch(Request::Refresh, &cancel)
                .await
                .unwrap()
                .is_none()
        );
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[test]
    fn inventory_branches_are_linked_sorted_unique_and_include_recorded_other_branches() {
        let inventory = [
            worktree("main", "main", true),
            worktree("two", "feature/two", false),
            worktree("one", "feature/one", false),
            worktree("again", "feature/one", false),
            worktree("detached", "", false),
        ];
        assert_eq!(
            inventory_branches(&inventory, &serde_json::Value::Null),
            ["feature/one", "feature/two"]
        );
        // A row's recorded other branches ride the same read; a recorded
        // merge is final, and the main clone's record is not a worktree's.
        let state = serde_json::json!({"slugs": {
            "two": {"otherBranches": {
                "feature/extra": {"state": "open", "number": 9},
                "feature/one": {"state": "unknown"},
                "feature/done": {"state": "merged", "number": 8},
            }},
            "main": {"otherBranches": {"feature/main-side": {}}},
        }});
        assert_eq!(
            inventory_branches(&inventory, &state),
            ["feature/extra", "feature/one", "feature/two"]
        );
    }

    fn branches() -> Vec<String> {
        vec!["feature".into()]
    }

    fn input(branches: &[String], daemon: Option<DaemonRead>, now: Instant) -> PlanInput<'_> {
        PlanInput {
            branches,
            daemon,
            request: Request::Refresh,
            now,
        }
    }

    fn daemon(started: Option<Instant>, written_ms: u64) -> DaemonRead {
        DaemonRead {
            data: GithubData::default(),
            started,
            scoped: Default::default(),
            written_ms,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn network_reads_respect_the_floor_and_defer_until_it_passes() {
        let branches = branches();
        let mut clock = LaneClock::default();
        let start = Instant::now();
        assert!(matches!(
            clock.plan(input(&branches, None, start)),
            Plan::Network
        ));
        assert_eq!(clock.live_started, Some(start));
        let early = start + Duration::from_secs(3);
        assert!(matches!(
            clock.plan(input(&branches, None, early)),
            Plan::Defer
        ));
        assert_eq!(clock.deferred, Some(start + GITHUB_MINIMUM));
        let due = start + GITHUB_MINIMUM;
        assert!(matches!(
            clock.plan(input(&branches, None, due)),
            Plan::Network
        ));
        assert_eq!(clock.deferred, None);
        assert_eq!(clock.live_started, Some(due));
    }

    /// A budget down to its reserve holds the backstop and other background
    /// reads for the paced interval, whether a direct read or the daemon's
    /// snapshot reported it. A read the user asked for keeps the ordinary
    /// floor.
    #[tokio::test(start_paused = true)]
    async fn a_low_budget_defers_background_reads_but_not_requested_ones() {
        let branches = branches();
        let low = |reset_in: Duration| GithubData {
            rate_limit: Some(wt_github::RateLimit {
                limit: 5_000,
                remaining: 800,
                cost: 5,
                reset_at_ms: epoch_ms() + reset_in.as_millis() as u64,
            }),
            ..GithubData::default()
        };
        let reset_in = Duration::from_secs(20 * 60);
        let start = Instant::now();
        let plan = |clock: &mut LaneClock, request, now| {
            clock.plan(PlanInput {
                request,
                ..input(&branches, None, now)
            })
        };
        let mut clock = LaneClock::default();
        assert!(matches!(
            plan(&mut clock, Request::Refresh, start),
            Plan::Network
        ));
        clock.live_read(start, branches.clone(), &low(reset_in));
        let later = start + Duration::from_secs(5 * 60);
        assert!(matches!(
            plan(&mut clock, Request::Refresh, later),
            Plan::Defer
        ));
        // The reset is 20 minutes off; the hold is capped short of it.
        assert_eq!(
            clock.deferred,
            Some(start + Duration::from_millis(wt_events::MAX_PACED_WAIT_MS))
        );
        assert!(matches!(
            plan(&mut clock, Request::Direct, later),
            Plan::Network
        ));
        // A healthy budget, or a server that reports none, changes nothing.
        clock.live_read(later, branches.clone(), &GithubData::default());
        assert!(matches!(
            plan(&mut clock, Request::Refresh, later + GITHUB_MINIMUM),
            Plan::Network
        ));

        // The daemon's snapshot carries the budget its read saw.
        let mut clock = LaneClock {
            live_started: Some(start),
            ..LaneClock::default()
        };
        let mut read = daemon(Some(start), 1_000);
        read.data = low(reset_in);
        assert!(matches!(
            clock.plan(input(&branches, Some(read), start)),
            Plan::Publish(_)
        ));
        assert!(matches!(
            plan(&mut clock, Request::Refresh, later),
            Plan::Defer
        ));

        // A refused read waits for the reset its reply named. One that named
        // none still backs off, instead of reading again on the next tick.
        let mut clock = LaneClock {
            live_started: Some(start),
            ..LaneClock::default()
        };
        clock.rate_limited(Some(epoch_ms() + reset_in.as_millis() as u64));
        assert!(matches!(
            plan(&mut clock, Request::Refresh, later),
            Plan::Defer
        ));
        assert!(clock.deferred.unwrap() >= start + reset_in);
        clock.rate_limited(None);
        let soon = start + Duration::from_secs(60);
        assert!(matches!(
            plan(&mut clock, Request::Refresh, soon),
            Plan::Defer
        ));
        assert_eq!(clock.deferred, Some(start + Duration::from_secs(5 * 60)));
        assert!(matches!(
            plan(&mut clock, Request::Direct, soon),
            Plan::Network
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_daemon_snapshot_is_served_once_per_file_version() {
        let branches = branches();
        let mut clock = LaneClock::default();
        let started = Instant::now();
        let now = started + Duration::from_millis(5);
        assert!(matches!(
            clock.plan(input(&branches, Some(daemon(Some(started), 1_000)), now)),
            Plan::Publish(_)
        ));
        assert_eq!(clock.published.as_deref(), Some(&branches[..]));
        assert!(matches!(
            clock.plan(input(&branches, Some(daemon(Some(started), 1_000)), now)),
            Plan::Skip
        ));
        let restarted = started + Duration::from_secs(1);
        assert!(matches!(
            clock.plan(input(&branches, Some(daemon(Some(restarted), 2_000)), now)),
            Plan::Publish(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn daemon_data_older_than_a_live_read_is_skipped_unless_branches_changed() {
        let branches = branches();
        let mut clock = LaneClock::default();
        let before = Instant::now();
        let live = before + Duration::from_secs(1);
        clock.live_read(live, branches.clone(), &GithubData::default());
        // Started before the live read, or never recorded its start.
        assert!(matches!(
            clock.plan(input(&branches, Some(daemon(Some(before), 1_000)), live)),
            Plan::Skip
        ));
        assert!(matches!(
            clock.plan(input(&branches, Some(daemon(None, 1_000)), live)),
            Plan::Skip
        ));
        // A new branch has no data yet: the daemon supplies it.
        let more = vec!["another".to_owned(), "feature".to_owned()];
        assert!(matches!(
            clock.plan(input(&more, Some(daemon(Some(before), 1_000)), live)),
            Plan::Publish(_)
        ));
    }

    /// A PR marked ready, read live after the write, then a worktree is
    /// created and the daemon's older snapshot (still showing a draft) is the
    /// only data for the new branch. The new branch comes from the daemon;
    /// the newer live read keeps the existing branch, and the merged stamp
    /// cannot confirm anything the older snapshot might have missed.
    #[tokio::test(start_paused = true)]
    async fn an_older_daemon_snapshot_after_a_branch_change_never_regresses_a_live_read() {
        use crate::github_actions::tests::pr;
        let branches = branches();
        let mut clock = LaneClock::default();
        let before = Instant::now();
        let live = before + Duration::from_secs(1);
        let mut ready = pr("feature");
        ready.is_draft = false;
        let mut live_data = GithubData::default();
        live_data.prs.insert("feature".into(), ready);
        live_data.covered_branches = branches.iter().cloned().collect();
        clock.live_read(live, branches.clone(), &live_data);

        let more = vec!["another".to_owned(), "feature".to_owned()];
        let mut older = daemon(Some(before), 1_000);
        older.data.prs.insert("feature".into(), pr("feature"));
        let mut other = pr("another");
        other.number = 8;
        older.data.prs.insert("another".into(), other);
        older.data.covered_branches = more.iter().cloned().collect();
        let Plan::Publish(fetched) = clock.plan(input(&more, Some(older), live)) else {
            panic!("the new branch's data is published");
        };
        assert!(!fetched.data.prs["feature"].is_draft, "live read regressed");
        assert_eq!(fetched.data.prs["another"].number, 8);
        assert_eq!(
            fetched.data.covered_branches,
            more.iter().cloned().collect()
        );
        assert_eq!(fetched.observed, Stamp::at(before));
        assert_eq!(clock.published.as_deref(), Some(&more[..]));

        // A removed branch keeps the live data and its stamp.
        let mut clock = LaneClock::default();
        let mut both = live_data.clone();
        both.prs.insert("another".into(), pr("another"));
        both.covered_branches = more.iter().cloned().collect();
        clock.live_read(live, more.clone(), &both);
        let Plan::Publish(fetched) =
            clock.plan(input(&branches, Some(daemon(Some(before), 1_000)), live))
        else {
            panic!("the smaller branch set is published");
        };
        assert!(!fetched.data.prs["feature"].is_draft);
        assert!(!fetched.data.prs.contains_key("another"));
        assert_eq!(fetched.observed, Stamp::at(live));
    }

    /// After a live read, the daemon's snapshot is older as a whole, but a
    /// branch whose check finished since has been read again on its own.
    /// That branch takes the daemon's newer answer; every other branch and
    /// the stamp stay the live read's, so the result confirms nothing the
    /// live read did not, and no branch goes back to an older answer.
    #[tokio::test(start_paused = true)]
    async fn branches_the_daemon_read_again_after_a_live_read_are_merged_without_a_newer_stamp() {
        use crate::github_actions::tests::pr;
        let branches = vec!["feature".to_owned(), "other".to_owned()];
        let mut clock = LaneClock::default();
        let swept = Instant::now();
        let live = swept + Duration::from_secs(5);
        let mut live_data = GithubData::default();
        let mut feature = pr("feature");
        feature.is_draft = false;
        live_data.prs.insert("feature".into(), feature);
        let mut other = pr("other");
        other.is_draft = false;
        live_data.prs.insert("other".into(), other);
        live_data.covered_branches = branches.iter().cloned().collect();
        clock.live_read(live, branches.clone(), &live_data);

        // The daemon's sweep predates the live read and still shows both as
        // drafts. Its scoped read of `feature` began after the live read
        // and found a different head.
        let snapshot = |written_ms, feature_read: Option<Instant>, head: Option<&str>| {
            let mut read = daemon(Some(swept), written_ms);
            if let Some(head) = head {
                let mut feature = pr("feature");
                feature.head_ref_oid = Some(head.into());
                read.data.prs.insert("feature".into(), feature);
            }
            read.data.prs.insert("other".into(), pr("other"));
            read.data.covered_branches = branches.iter().cloned().collect();
            read.scoped = feature_read
                .map(|at| ("feature".to_owned(), at))
                .into_iter()
                .collect();
            read
        };
        let after = live + Duration::from_secs(20);
        let Plan::Publish(fetched) = clock.plan(input(
            &branches,
            Some(snapshot(1_000, Some(after), Some("cafe"))),
            after,
        )) else {
            panic!("the branch read again is published");
        };
        assert_eq!(
            fetched.data.prs["feature"].head_ref_oid.as_deref(),
            Some("cafe")
        );
        assert!(
            !fetched.data.prs["other"].is_draft,
            "the live read regressed"
        );
        assert_eq!(fetched.observed, Stamp::at(live));
        // The same file is not merged twice.
        assert!(matches!(
            clock.plan(input(
                &branches,
                Some(snapshot(1_000, Some(after), Some("cafe"))),
                after
            )),
            Plan::Skip
        ));
        // A direct read that began before the scoped read finishes after
        // it and replaces the shown data with its older answer. The same
        // file is then merged again.
        clock.live_read(after - Duration::from_secs(1), branches.clone(), &live_data);
        let Plan::Publish(fetched) = clock.plan(input(
            &branches,
            Some(snapshot(1_000, Some(after), Some("cafe"))),
            after,
        )) else {
            panic!("the scoped read is merged over the slower direct read");
        };
        assert_eq!(
            fetched.data.prs["feature"].head_ref_oid.as_deref(),
            Some("cafe")
        );
        assert_eq!(fetched.observed, Stamp::at(after - Duration::from_secs(1)));
        // A scoped read that found no pull request removes the branch's.
        let Plan::Publish(fetched) = clock.plan(input(
            &branches,
            Some(snapshot(2_000, Some(after), None)),
            after,
        )) else {
            panic!("the closed branch is published");
        };
        assert!(!fetched.data.prs.contains_key("feature"));
        assert!(fetched.data.prs.contains_key("other"));
        assert_eq!(fetched.observed, Stamp::at(after - Duration::from_secs(1)));

        // A scoped read that began before the live read is older than what
        // is shown, and a snapshot with none changes nothing.
        let mut clock = LaneClock::default();
        clock.live_read(live, branches.clone(), &live_data);
        let before = live - Duration::from_secs(1);
        assert!(matches!(
            clock.plan(input(
                &branches,
                Some(snapshot(3_000, Some(before), Some("cafe"))),
                after
            )),
            Plan::Skip
        ));
        assert!(matches!(
            clock.plan(input(
                &branches,
                Some(snapshot(4_000, None, Some("cafe"))),
                after
            )),
            Plan::Skip
        ));
    }

    /// The pace follows the window with the least left, also when a later
    /// read reports only the roomier one.
    #[tokio::test(start_paused = true)]
    async fn the_pace_keeps_to_the_window_that_runs_out_first() {
        let branches = branches();
        let rate = |remaining, reset_in: Duration| GithubData {
            rate_limit: Some(wt_github::RateLimit {
                limit: 5_000,
                remaining,
                cost: 5,
                reset_at_ms: epoch_ms() + reset_in.as_millis() as u64,
            }),
            ..GithubData::default()
        };
        let start = Instant::now();
        let mut clock = LaneClock {
            live_started: Some(start),
            ..LaneClock::default()
        };
        clock.live_read(
            start,
            branches.clone(),
            &rate(800, Duration::from_secs(20 * 60)),
        );
        clock.live_read(
            start,
            branches.clone(),
            &rate(4_900, Duration::from_secs(40 * 60)),
        );
        let later = start + Duration::from_secs(5 * 60);
        assert!(matches!(
            clock.plan(input(&branches, None, later)),
            Plan::Defer
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_recheck_without_usable_daemon_data_reads_only_when_needed() {
        let branches = branches();
        let now = Instant::now();
        let plan = |clock: &mut LaneClock, request| {
            clock.plan(PlanInput {
                request,
                ..input(&branches, None, now)
            })
        };
        // Nothing from the daemon was shown for this branch set: skip.
        let mut clock = LaneClock {
            published: Some(branches.clone()),
            ..LaneClock::default()
        };
        assert!(matches!(plan(&mut clock, Request::Recheck), Plan::Skip));
        // An owed direct read happens regardless, and is then no longer owed.
        clock.direct_owed = true;
        assert!(matches!(plan(&mut clock, Request::Recheck), Plan::Network));
        assert!(!clock.direct_owed);
        // Daemon data had been shown and has aged out: read GitHub.
        let mut clock = LaneClock {
            published: Some(branches.clone()),
            served_daemon: Some((1_000, branches.clone())),
            ..LaneClock::default()
        };
        assert!(matches!(plan(&mut clock, Request::Recheck), Plan::Network));
        assert_eq!(clock.served_daemon, None);
    }

    /// The backstop tick and the events watcher's reconcile tick are
    /// phase-locked (600 s is a multiple of 15 s), so they share one debounce
    /// window. With no usable daemon snapshot the shared fetch must still
    /// read GitHub, or nothing ever rereads it.
    #[tokio::test(start_paused = true)]
    async fn a_backstop_coalesced_with_a_recheck_still_reads_github() {
        let scope = TaskScope::new();
        let plans = Arc::new(Mutex::new(Vec::new()));
        let mut clock = LaneClock {
            published: Some(branches()),
            ..LaneClock::default()
        };
        let source = start_requested_source(
            &scope,
            SourceSpec::<GithubData> {
                policy: RefreshPolicy {
                    debounce: Duration::from_millis(100),
                    minimum_interval: DAEMON_MINIMUM,
                },
                ..spec("github", Domain::Github)
            },
            {
                let plans = plans.clone();
                move |request, _| {
                    let plan = clock.plan(PlanInput {
                        request,
                        ..input(&branches(), None, Instant::now())
                    });
                    plans.lock().unwrap().push(matches!(plan, Plan::Network));
                    async { Ok::<_, GithubError>(None) }
                }
            },
        );
        // The backstop, then the watcher's recheck inside its debounce.
        source.refresh();
        source.request(Request::Recheck);
        tokio::time::sleep(Duration::from_secs(1)).await;
        // A recheck alone, past the rate floor, still declines to read.
        tokio::time::sleep(GITHUB_MINIMUM).await;
        source.request(Request::Recheck);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(*plans.lock().unwrap(), [true, false]);
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
