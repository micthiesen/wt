//! The GitHub leaf's scheduling: which branches to read, when to serve the
//! events daemon's snapshot instead of reading GitHub, and the network rate
//! floor with its deferred reads.

use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};
use tokio_util::sync::CancellationToken;
use wt_core::Domain;
use wt_github::{GithubClient, GithubData, GithubError, GithubOptions};
use wt_runtime::{
    DiskCache, Fetched, LeafHandle, Persistence, RefreshPolicy, SourceHandle, SourceSpec, Stamp,
    TaskScope, start_skippable_source,
};
use wt_vcs::WorktreeSnapshot;

use crate::{context::AppContext, github_events_source::DaemonRead};

/// Network reads of GitHub, measured from the prior read's start.
const GITHUB_MINIMUM: Duration = Duration::from_secs(10);
/// Rereads of the events daemon's local snapshot file.
const DAEMON_MINIMUM: Duration = Duration::from_millis(100);

/// Refresh requests for the GitHub leaf, including the one-shot request to
/// skip the events daemon's snapshot.
#[derive(Clone)]
pub struct GithubRefresh {
    github: LeafHandle<GithubData>,
    bypass_daemon: Arc<AtomicBool>,
}

impl GithubRefresh {
    /// The latest live GitHub read, for settling this host's own writes.
    pub fn confirmed(&self) -> Option<wt_runtime::Confirmed<GithubData>> {
        self.github.confirmed()
    }

    pub fn refresh(&self) {
        self.github.refresh();
    }

    /// After an accepted GitHub write: the read that may confirm it must be a
    /// direct fetch stamped here, not the events daemon's older snapshot.
    pub fn refresh_live(&self) {
        self.bypass_daemon.store(true, Ordering::Release);
        self.github.refresh();
    }

    #[cfg(test)]
    pub(crate) fn detached() -> Self {
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
            bypass_daemon: Arc::default(),
        }
    }
}

/// The one GitHub leaf. Each fetch chooses between the events daemon's
/// snapshot and a direct read; both publish the moment their GitHub request
/// started, so neither can claim to have seen a later write.
pub(crate) fn start(
    scope: &TaskScope,
    context: &AppContext,
    git: LeafHandle<Vec<WorktreeSnapshot>>,
    enabled: bool,
    backstop: Duration,
) -> (LeafHandle<GithubData>, GithubRefresh) {
    let lane = Arc::new(GithubLane::new(context.clone(), git.clone(), enabled));
    let github = start_skippable_source(
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
            move |cancel| {
                let lane = lane.clone();
                async move { lane.fetch(&cancel).await }
            }
        },
    );
    let refresh = GithubRefresh {
        github: github.clone(),
        bypass_daemon: lane.bypass_daemon.clone(),
    };
    spawn_deferred_reads(scope, &lane, github.view());
    spawn_github_driver(scope, git, github.view(), backstop);
    if enabled {
        crate::github_events_source::watch(scope, context, github.view(), lane.file_event.clone());
    }
    (github, refresh)
}

struct GithubLane {
    context: AppContext,
    git: LeafHandle<Vec<WorktreeSnapshot>>,
    enabled: bool,
    bypass_daemon: Arc<AtomicBool>,
    /// Set by the events-dir watcher: the request only rechecks the daemon
    /// snapshot and is not by itself a reason to read GitHub.
    file_event: Arc<AtomicBool>,
    clock: Mutex<LaneClock>,
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
    /// A network read the floor postponed is due then.
    deferred: Option<Instant>,
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
    file_event: bool,
    bypass: bool,
    now: Instant,
}

impl LaneClock {
    /// Decide this fetch without awaiting: serve the daemon snapshot, skip,
    /// postpone a network read behind the floor, or read GitHub now.
    fn plan(&mut self, input: PlanInput<'_>) -> Plan {
        let PlanInput {
            branches,
            daemon,
            file_event,
            bypass,
            now,
        } = input;
        let same_branches = self.published.as_deref() == Some(branches);
        if let Some(read) = daemon {
            let newer = match (read.started, self.live_observed) {
                (_, None) => true,
                (Some(started), Some(live)) => started >= live,
                (None, Some(_)) => false,
            };
            // A new branch has no data yet; complete daemon coverage of it is
            // better than waiting, even when other PRs were read more recently.
            if !newer && same_branches {
                // Fresh daemon data exists and the shown data is newer still.
                return Plan::Skip;
            }
            let version = (read.written_ms, branches.to_vec());
            if self.served_daemon.as_ref() == Some(&version) {
                return Plan::Skip;
            }
            self.served_daemon = Some(version);
            self.published = Some(branches.to_vec());
            let observed = read.stamp();
            return Plan::Publish(Fetched::new(read.data, observed));
        }
        // A recheck that found nothing usable only reads GitHub when the
        // daemon data it had shown has aged out or the branch set changed.
        if file_event && !bypass && same_branches && self.served_daemon.is_none() {
            return Plan::Skip;
        }
        if let Some(due) = self
            .live_started
            .map(|at| at + GITHUB_MINIMUM)
            .filter(|due| *due > now)
        {
            self.deferred = Some(due);
            return Plan::Defer;
        }
        self.live_started = Some(now);
        self.served_daemon = None;
        self.deferred = None;
        Plan::Network
    }
}

impl GithubLane {
    fn new(context: AppContext, git: LeafHandle<Vec<WorktreeSnapshot>>, enabled: bool) -> Self {
        Self {
            context,
            git,
            enabled,
            bypass_daemon: Arc::default(),
            file_event: Arc::default(),
            clock: Mutex::default(),
            wake: Notify::new(),
        }
    }

    fn clock(&self) -> std::sync::MutexGuard<'_, LaneClock> {
        self.clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn fetch(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<Fetched<GithubData>>, GithubError> {
        let file_event = self.file_event.swap(false, Ordering::AcqRel);
        // The key set comes from Git. Restored or missing inventory cannot
        // name the branches to read; the inventory's next change re-arms this.
        let Some(inventory) = self.git.confirmed() else {
            return Ok(None);
        };
        let branches = inventory_branches(&inventory);
        if !self.enabled || branches.is_empty() {
            let mut clock = self.clock();
            if clock.published.as_ref() == Some(&branches) {
                return Ok(None);
            }
            clock.published = Some(branches);
            clock.served_daemon = None;
            return Ok(Some(Fetched::new(GithubData::default(), Stamp::now())));
        }
        let bypass = self.bypass_daemon.load(Ordering::Acquire);
        let daemon = if bypass {
            None
        } else {
            crate::github_events_source::load(&self.context, &branches).await
        };
        let now = Instant::now();
        let plan = self.clock().plan(PlanInput {
            branches: &branches,
            daemon,
            file_event,
            bypass,
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
        self.bypass_daemon.store(false, Ordering::Release);
        let config = &self.context.config;
        let ci = has_workflows(&config.paths.main_clone).await;
        let client = GithubClient::new(
            self.context.processes.clone(),
            config.paths.main_clone.clone(),
            GithubOptions::from_config(config, ci),
        );
        let data = client.fetch_worktrees(&branches, cancel).await?;
        let mut clock = self.clock();
        clock.live_observed = Some(now);
        clock.published = Some(branches);
        Ok(Some(Fetched::new(data, Stamp::at(now))))
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
/// first goes live) and on the slow backstop.
fn spawn_github_driver(
    scope: &TaskScope,
    git: LeafHandle<Vec<WorktreeSnapshot>>,
    github: SourceHandle<GithubData>,
    backstop: Duration,
) {
    let cancel = scope.token();
    scope.spawn(async move {
        let mut updates = git.subscribe();
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
                    let snapshot = updates.borrow_and_update().clone();
                    let Some(confirmed) = git.confirm(&snapshot) else { continue };
                    let next = inventory_branches(&confirmed);
                    if previous.as_ref() != Some(&next) {
                        previous = Some(next);
                        github.refresh();
                    }
                }
                _ = interval.tick() => { github.refresh(); }
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

/// Branches GitHub is asked about: every linked worktree's branch.
fn inventory_branches(inventory: &[WorktreeSnapshot]) -> Vec<String> {
    let mut branches: Vec<_> = inventory
        .iter()
        .filter(|snapshot| !snapshot.worktree.is_main)
        .map(|snapshot| snapshot.worktree.target.branch.clone())
        .filter(|branch| !branch.is_empty())
        .collect();
    branches.sort();
    branches.dedup();
    branches
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
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let (git, git_publisher) = leaf_channel(&scope, spec("git", Domain::Git));
        // Disabled GitHub still proves the gate without a network client.
        let lane = GithubLane::new(fixture.ctx.clone(), git, false);
        let cancel = CancellationToken::new();
        assert!(lane.fetch(&cancel).await.unwrap().is_none());
        git_publisher.publish_live(
            vec![
                worktree("main", "main", true),
                worktree("one", "feature/one", false),
            ],
            Stamp::now(),
        );
        let fetched = lane
            .fetch(&cancel)
            .await
            .unwrap()
            .expect("confirmed Git fetches");
        assert!(fetched.data.prs.is_empty());
        assert_eq!(*lane.clock().published.as_ref().unwrap(), ["feature/one"]);
        // The same branch set does not republish.
        assert!(lane.fetch(&cancel).await.unwrap().is_none());
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[test]
    fn inventory_branches_are_linked_sorted_and_unique() {
        let inventory = [
            worktree("main", "main", true),
            worktree("two", "feature/two", false),
            worktree("one", "feature/one", false),
            worktree("again", "feature/one", false),
            worktree("detached", "", false),
        ];
        assert_eq!(
            inventory_branches(&inventory),
            ["feature/one", "feature/two"]
        );
    }

    fn branches() -> Vec<String> {
        vec!["feature".into()]
    }

    fn input(branches: &[String], daemon: Option<DaemonRead>, now: Instant) -> PlanInput<'_> {
        PlanInput {
            branches,
            daemon,
            file_event: false,
            bypass: false,
            now,
        }
    }

    fn daemon(started: Option<Instant>, written_ms: u64) -> DaemonRead {
        DaemonRead {
            data: GithubData::default(),
            started,
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
        clock.live_observed = Some(live);
        clock.published = Some(branches.clone());
        // Started before the live read, or never recorded its start.
        assert!(matches!(
            clock.plan(input(&branches, Some(daemon(Some(before), 1_000)), live)),
            Plan::Skip
        ));
        assert!(matches!(
            clock.plan(input(&branches, Some(daemon(None, 1_000)), live)),
            Plan::Skip
        ));
        // A new branch has no data yet: the daemon's full coverage is served.
        let more = vec!["another".to_owned(), "feature".to_owned()];
        assert!(matches!(
            clock.plan(input(&more, Some(daemon(Some(before), 1_000)), live)),
            Plan::Publish(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_file_event_without_usable_daemon_data_reads_only_when_needed() {
        let branches = branches();
        let now = Instant::now();
        let recheck = |clock: &mut LaneClock, bypass: bool| {
            clock.plan(PlanInput {
                file_event: true,
                bypass,
                ..input(&branches, None, now)
            })
        };
        // Nothing from the daemon was shown for this branch set: skip.
        let mut clock = LaneClock {
            published: Some(branches.clone()),
            ..LaneClock::default()
        };
        assert!(matches!(recheck(&mut clock, false), Plan::Skip));
        // A bypass request reads regardless.
        assert!(matches!(recheck(&mut clock, true), Plan::Network));
        // Daemon data had been shown and has aged out: read GitHub.
        let mut clock = LaneClock {
            published: Some(branches.clone()),
            served_daemon: Some((1_000, branches.clone())),
            ..LaneClock::default()
        };
        assert!(matches!(recheck(&mut clock, false), Plan::Network));
        assert_eq!(clock.served_daemon, None);
    }
}
