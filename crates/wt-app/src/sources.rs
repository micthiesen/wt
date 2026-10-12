//! Independent I/O lanes composed into a prepared board. Only explicit refresh
//! requests cross from presentation back into fetch scheduling.

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
use wt_github::{GithubClient, GithubData, GithubError, GithubOptions, PrChecks, PrReview};
use wt_runtime::{
    DiskCache, Domain, Fetched, LeafHandle, Persistence, RefreshPolicy, SourceHandle,
    SourceSnapshot, SourceSpec, SourceState, Stamp, TaskScope, derived_channel,
    start_skippable_source,
};
use wt_tui::Board;
use wt_vcs::WorktreeSnapshot;

use crate::context::AppContext;

/// Network reads of GitHub, measured from the prior read's start.
const GITHUB_MINIMUM: Duration = Duration::from_secs(10);
/// Rereads of the events daemon's local snapshot file.
const DAEMON_MINIMUM: Duration = Duration::from_millis(100);

pub struct BoardSources {
    pub board: SourceHandle<Board>,
    pub local: SourceHandle<Board>,
    pub git: LeafHandle<Vec<WorktreeSnapshot>>,
    pub metadata: LeafHandle<crate::local_source::Metadata>,
    /// Raw PR data as GitHub (or the events daemon) reported it. There is no
    /// optimistic overlay: pending writes are shown by the TUI's own engine.
    pub github: LeafHandle<GithubData>,
    pub github_refresh: GithubRefresh,
    pub github_actions: crate::github_actions::GithubActions,
    pub github_pickers: Arc<crate::github_pickers::GithubPickers>,
    pub naming: crate::naming_source::NamingCommands,
    pub sessions: crate::session_source::SessionSources,
    pub automations: crate::automation_source::AutomationCommands,
    pub history: crate::history_source::HistoryCommands,
    pub perf: crate::perf_source::PerfCommands,
    pub hard_refresh: crate::hard_refresh::HardRefreshCommands,
    pub review_requests: crate::review_requests::ReviewRequests,
}

impl BoardSources {
    /// After an accepted GitHub write: the read that may confirm it must be a
    /// direct fetch stamped here, not the events daemon's older snapshot.
    pub fn refresh_github_live(&self) {
        self.github_refresh.refresh_live();
    }
}

/// Refresh requests for the GitHub leaf, including the one-shot request to
/// skip the events daemon's snapshot.
#[derive(Clone)]
pub struct GithubRefresh {
    github: SourceHandle<GithubData>,
    bypass_daemon: Arc<AtomicBool>,
}

impl GithubRefresh {
    pub fn refresh(&self) {
        self.github.refresh();
    }

    pub fn refresh_live(&self) {
        self.bypass_daemon.store(true, Ordering::Release);
        self.github.refresh();
    }

    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        Self {
            github: SourceHandle::constant(SourceSnapshot::default()),
            bypass_daemon: Arc::default(),
        }
    }
}

pub fn start(scope: &TaskScope, context: &AppContext) -> BoardSources {
    let local_sources = crate::local_source::start(scope, context);
    let history = crate::history_source::start(
        scope,
        context,
        local_sources.metadata.view(),
        local_sources.git.view(),
    );
    let sessions = crate::session_source::start(scope, context, local_sources.git.view());
    let session_activity = crate::session_activity::start(scope, context, &sessions).activity;
    let local = local_sources.board;
    // This exact pipeline runs on every host. The controller combines its
    // completed snapshots; transport never reimplements a feature source.
    let enabled = std::env::var("WT_GITHUB").as_deref() != Ok("off");
    let (github, github_refresh) = start_github(
        scope,
        context,
        local_sources.git.clone(),
        enabled,
        crate::origin::backstop(context),
    );
    let github_actions =
        crate::github_actions::GithubActions::start(scope, context, github_refresh.clone());
    let github_pickers = Arc::new(crate::github_pickers::GithubPickers::start(context));
    let activity = crate::activity_source::start(
        scope,
        context.config.paths.cache_root.clone(),
        context.config.paths.app_log_dir.clone(),
    );
    let board = project(scope, local.clone(), github.view(), activity.view());
    let dev = crate::dev_source::overlay(scope, context, local.clone(), board);
    let origin = crate::origin::overlay(scope, context, dev.board, local.clone());
    let issues = crate::issue_source::start(scope, context, origin.board);
    let attention_sources = crate::attention_source::AttentionSources {
        metadata: local_sources.metadata.view(),
        github: github.view(),
        issue_statuses: issues.statuses.view(),
        dev_status: dev.status.as_ref().map(|status| status.view()),
        manager_sessions: sessions.discoveries.view(),
    };
    let board = crate::action_source::overlay(
        scope,
        context,
        issues.board,
        crate::action_source::RefreshTargets {
            git: local_sources.git.clone(),
            github: github.clone(),
            dev: dev.status,
            origin: origin.origin.clone(),
            issues: issues.statuses,
        },
    );
    let git_facts = crate::git_presentation::start(
        scope,
        context,
        local_sources.git.view(),
        local_sources.metadata.view(),
        github.view(),
    );
    let board = crate::git_presentation::overlay(
        scope,
        board,
        git_facts,
        local_sources.git.view(),
        local_sources.metadata.view(),
        github.view(),
        context.config.branch.base.clone(),
    );
    let naming = crate::naming_source::start(
        scope,
        context,
        local_sources.git.view(),
        local_sources.metadata.view(),
        board,
    );
    let board =
        crate::session_board::overlay(scope, naming.board, &sessions, session_activity.clone());
    let board = crate::history_source::overlay(scope, board, history.snapshot);
    let (review_requests, review_snapshot) =
        crate::review_requests::ReviewRequests::start(scope, context);
    let board = crate::review_requests::overlay(scope, board, review_snapshot);
    let board = crate::attention_source::overlay(scope, context, board, attention_sources);
    let board =
        crate::work_presentation::overlay(scope, context.config.clone(), board, github.view());
    let automations = crate::automation_source::start(
        scope,
        context,
        crate::automation_source::AutomationSources {
            board,
            git: local_sources.git.clone(),
            metadata: local_sources.metadata.clone(),
            github: github.clone(),
            sessions: sessions.discoveries.clone(),
            inventory: sessions.inventory.clone(),
            activity: session_activity.clone(),
            edits: local_sources.edits,
        },
    );
    let perf = crate::perf_source::start(scope, context, automations.board);
    let hard_refresh = crate::hard_refresh::HardRefreshCommands::new(
        perf.board.clone(),
        github_refresh.clone(),
        origin.origin.as_ref().map(|origin| origin.view()),
        naming.commands.clone(),
        github_pickers.clone(),
    );
    BoardSources {
        board: perf.board,
        local,
        git: local_sources.git,
        metadata: local_sources.metadata,
        github,
        github_refresh,
        github_actions,
        github_pickers,
        naming: naming.commands,
        sessions,
        automations: automations.commands,
        history: history.commands,
        perf: perf.commands,
        hard_refresh,
        review_requests,
    }
}

/// The one GitHub leaf. Each fetch chooses between the events daemon's
/// snapshot and a direct read; both publish the moment their GitHub request
/// started, so neither can claim to have seen a later write.
fn start_github(
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
        github: github.view(),
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
    served_daemon: Option<(Instant, Vec<String>)>,
    /// The branch set the published data covers.
    published: Option<Vec<String>>,
    /// A network read the floor postponed is due then.
    deferred: Option<Instant>,
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
        match self.plan(&branches, daemon, file_event, bypass, now) {
            Plan::Skip => return Ok(None),
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

    /// Decide this fetch without awaiting: serve the daemon snapshot, skip,
    /// postpone a network read behind the floor, or read GitHub now.
    fn plan(
        &self,
        branches: &[String],
        daemon: Option<crate::github_events_source::DaemonRead>,
        file_event: bool,
        bypass: bool,
        now: Instant,
    ) -> Plan {
        let mut clock = self.clock();
        let same_branches = clock.published.as_deref() == Some(branches);
        if let Some(read) = daemon {
            let newer = match (read.started, clock.live_observed) {
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
            let version = (read.written, branches.to_vec());
            if clock.served_daemon.as_ref() == Some(&version) {
                return Plan::Skip;
            }
            clock.served_daemon = Some(version);
            clock.published = Some(branches.to_vec());
            let observed = read.stamp();
            return Plan::Publish(Fetched::new(read.data, observed));
        }
        // A recheck that found nothing usable only reads GitHub when the
        // daemon data it had shown has aged out or the branch set changed.
        if file_event && !bypass && same_branches && clock.served_daemon.is_none() {
            return Plan::Skip;
        }
        if let Some(due) = clock
            .live_started
            .map(|at| at + GITHUB_MINIMUM)
            .filter(|due| *due > now)
        {
            clock.deferred = Some(due);
            self.wake.notify_one();
            return Plan::Skip;
        }
        clock.live_started = Some(now);
        clock.served_daemon = None;
        clock.deferred = None;
        Plan::Network
    }
}

enum Plan {
    Skip,
    Publish(Fetched<GithubData>),
    Network,
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

fn project(
    scope: &TaskScope,
    local: SourceHandle<Board>,
    github: SourceHandle<GithubData>,
    activity: SourceHandle<crate::activity_source::ActivitySnapshot>,
) -> SourceHandle<Board> {
    let (source, mut publisher) = derived_channel(scope, "github-board");
    let cancel = scope.token();
    scope.spawn(async move {
        let mut local_updates = local.subscribe();
        let mut github_updates = github.subscribe();
        let mut activity_updates = activity.subscribe();
        local_updates.mark_changed();
        github_updates.mark_changed();
        activity_updates.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    local.refresh();
                    activity.refresh();
                    // The GitHub leaf skips until Git inventory is confirmed.
                    github.refresh();
                    continue;
                }
                changed = local_updates.changed() => {
                    if changed.is_err() { break; }
                    local_updates.borrow_and_update();
                }
                changed = github_updates.changed() => {
                    if changed.is_err() { break; }
                    github_updates.borrow_and_update();
                }
                changed = activity_updates.changed() => {
                    if changed.is_err() { break; }
                    activity_updates.borrow_and_update();
                }
            }
            let local_snapshot = local_updates.borrow().clone();
            let github_snapshot = github_updates.borrow().clone();
            let activity_snapshot = activity_updates.borrow().clone();
            // The worktree-era guard stats directories. Keep it off the input
            // and async executor threads; all rows are prepared in one job.
            let result = tokio::task::spawn_blocking(move || {
                compose(local_snapshot, github_snapshot, activity_snapshot)
            })
            .await;
            match result {
                Ok(snapshot) if !cancel.is_cancelled() => publisher.publish(snapshot),
                Ok(_) => break,
                Err(error) => {
                    tracing::error!(%error, "board projection failed");
                    publisher.publish(local.snapshot().with_state(SourceState::Failed(
                        format!("board projection: {error}").into(),
                    )));
                }
            }
        }
    });
    source
}

fn compose(
    local: SourceSnapshot<Board>,
    github: SourceSnapshot<GithubData>,
    activity: SourceSnapshot<crate::activity_source::ActivitySnapshot>,
) -> SourceSnapshot<Board> {
    let Some(board) = local.data() else {
        return local;
    };
    let mut board = board.as_ref().clone();
    if let Some(feed) = activity.data() {
        board.activity.extend(feed.activity.iter().cloned());
        board.attention.extend(feed.attention.iter().cloned());
    }
    if let SourceState::Failed(error) = activity.state() {
        let at_ms = crate::activity_source::epoch_ms();
        let text = wt_core::sanitize_terminal_text(error);
        board.activity.push(wt_tui::ActivityLine {
            at_ms,
            level: "ERROR".into(),
            channel: "activity".into(),
            source: "activity feed".into(),
            text: text.clone(),
        });
        board.attention.push(wt_tui::AttentionLine {
            at_ms,
            source: "activity feed".into(),
            text,
        });
    }
    if let Some(data) = github.data() {
        for row in &mut board.rows {
            let Some(pr) = (if wt_core::is_remote_worktree_ledger_key(&row.key) {
                data.prs.get(&row.branch)
            } else {
                wt_github::pick_pr_for_worktree(Some(&row.branch), Path::new(&row.path), &data.prs)
            }) else {
                continue;
            };
            let clean = wt_core::sanitize_terminal_text;
            row.pr_url = Some(pr.url.clone());
            let state = if pr.is_draft { "draft" } else { &pr.state };
            row.badge = format!("{}  #{} {}", row.badge, pr.number, clean(state))
                .trim()
                .to_owned();
            if pr.state == "OPEN" {
                row.needs_attention |= pr.checks == PrChecks::Fail
                    || pr.review == PrReview::ChangesRequested
                    || pr.unresolved_threads > 0;
            }
        }
    }
    crate::activity_source::bound_feeds(&mut board);
    let mut state = local.state().clone();
    if let SourceState::Failed(error) = github.state() {
        // A remote failure stays visible while local updates and last-good
        // remote badges continue. Refreshing local data cannot mask it.
        if !matches!(state, SourceState::Failed(_)) {
            state = SourceState::Failed(
                format!("GitHub: {}", wt_core::sanitize_terminal_text(error)).into(),
            );
        }
    } else if *github.state() == SourceState::Refreshing && state == SourceState::Ready {
        state = SourceState::Refreshing;
    }
    local
        .with_data(Some(Arc::new(board)))
        .with_state(state)
        .with_input(&github)
        .with_input(&activity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_runtime::{DerivedPublisher, LeafPublisher, leaf_channel};
    use wt_tui::BoardRow;

    fn spec<T>(name: &'static str, domain: Domain) -> SourceSpec<T> {
        SourceSpec {
            name,
            domain,
            persistence: Persistence::Never("test fixture"),
            policy: RefreshPolicy::default(),
        }
    }

    fn publish_local(publisher: &DerivedPublisher<Board>, title: &str, branch: &str) {
        publisher.publish(SourceSnapshot::test_live(
            Some(Board {
                name: "test".into(),
                rows: vec![BoardRow {
                    key: "row".into(),
                    title: title.into(),
                    branch: branch.into(),
                    path: "/tmp/nonexistent-wt-source-fixture".into(),
                    ..BoardRow::default()
                }],
                ..Board::default()
            }),
            SourceState::Ready,
            Domain::Git,
        ));
    }

    async fn wait_for(
        source: &SourceHandle<Board>,
        predicate: impl Fn(&SourceSnapshot<Board>) -> bool,
    ) {
        tokio::time::timeout(
            Duration::from_secs(3),
            source.subscribe().wait_for(predicate),
        )
        .await
        .unwrap()
        .unwrap();
    }

    fn rows(snapshot: &SourceSnapshot<Board>) -> &[BoardRow] {
        snapshot.data().map_or(&[], |board| &board.rows)
    }

    struct Inputs {
        scope: TaskScope,
        local: DerivedPublisher<Board>,
        github: LeafPublisher<GithubData>,
        activity: LeafPublisher<crate::activity_source::ActivitySnapshot>,
        board: SourceHandle<Board>,
    }

    fn inputs(before: impl FnOnce(&DerivedPublisher<Board>)) -> Inputs {
        let scope = TaskScope::new();
        let (local, local_publisher) = derived_channel(&scope, "local");
        let (github, github_publisher) = leaf_channel(&scope, spec("github", Domain::Github));
        let (activity, activity_publisher) =
            leaf_channel(&scope, spec("activity", Domain::Activity));
        before(&local_publisher);
        let board = project(&scope, local, github.view(), activity.view());
        Inputs {
            scope,
            local: local_publisher,
            github: github_publisher,
            activity: activity_publisher,
            board,
        }
    }

    #[tokio::test]
    async fn projection_consumes_data_published_before_it_subscribes() {
        let inputs = inputs(|local| publish_local(local, "already available", "feature"));
        wait_for(&inputs.board, |snapshot| {
            rows(snapshot)
                .first()
                .is_some_and(|row| row.title == "already available")
        })
        .await;
        inputs.scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn local_updates_stay_live_during_remote_work_and_retain_remote_errors_and_data() {
        let mut inputs = inputs(|_| {});
        // An explicit refresh reaches every input; the GitHub leaf itself
        // decides whether it can fetch yet.
        inputs.board.refresh();
        assert_eq!(inputs.local.requested().await, Some(()));
        assert_eq!(inputs.github.requested().await, Some(()));
        assert_eq!(inputs.activity.requested().await, Some(()));
        publish_local(&inputs.local, "initial", "feature");
        wait_for(&inputs.board, |s| {
            rows(s).first().is_some_and(|row| row.title == "initial")
        })
        .await;
        inputs.github.set_state(SourceState::Refreshing);
        publish_local(&inputs.local, "renamed while fetching", "feature");
        wait_for(&inputs.board, |s| {
            rows(s)
                .first()
                .is_some_and(|row| row.title == "renamed while fetching")
                && *s.state() == SourceState::Refreshing
        })
        .await;
        let pr = serde_json::from_value(serde_json::json!({
            "number": 42, "url": "https://github.com/o/r/pull/42", "headRefName": "feature",
            "baseRefName": "main", "title": "A PR", "isDraft": false, "state": "OPEN",
            "checks": "fail", "failedChecks": ["tests"], "review": "pending", "reviewRequests": 0,
            "requestedReviewers": [], "suggestedReviewers": [], "comments": [],
            "unresolvedThreads": 0, "unresolvedThreadsTotal": 0
        }))
        .unwrap();
        inputs.github.publish_live(
            GithubData {
                prs: [("feature".into(), pr)].into(),
                ..GithubData::default()
            },
            Stamp::now(),
        );
        wait_for(&inputs.board, |s| {
            rows(s).first().is_some_and(|row| row.badge.contains("#42"))
                && s.domains().contains_key(&Domain::Github)
        })
        .await;
        inputs
            .github
            .set_state(SourceState::Failed("offline".into()));
        publish_local(&inputs.local, "renamed while offline", "feature");
        wait_for(&inputs.board, |s| {
            matches!(s.state(), SourceState::Failed(_))
                && rows(s)
                    .first()
                    .is_some_and(|row| row.title == "renamed while offline")
        })
        .await;
        let snapshot = inputs.board.snapshot();
        let row = &rows(&snapshot)[0];
        assert!(row.badge.contains("#42"));
        assert!(row.needs_attention);
        inputs.activity.publish_live(
            crate::activity_source::ActivitySnapshot {
                activity: vec![wt_tui::ActivityLine {
                    at_ms: 1,
                    level: "INFO".into(),
                    channel: "attention".into(),
                    source: "manager".into(),
                    text: "manager report".into(),
                }],
                attention: vec![wt_tui::AttentionLine {
                    at_ms: 1,
                    source: "manager".into(),
                    text: "manager report".into(),
                }],
            },
            Stamp::now(),
        );
        wait_for(&inputs.board, |s| {
            s.data()
                .is_some_and(|b| b.activity.iter().any(|line| line.text == "manager report"))
        })
        .await;
        // Data changes never request fetches.
        assert!(
            tokio::time::timeout(Duration::from_millis(5), inputs.github.requested())
                .await
                .is_err()
        );
        publish_local(&inputs.local, "new branch", "another");
        wait_for(&inputs.board, |s| {
            rows(s).first().is_some_and(|row| row.branch == "another")
        })
        .await;
        assert!(!rows(&inputs.board.snapshot())[0].badge.contains("#42"));
        inputs.scope.shutdown(Duration::from_secs(1)).await.unwrap();
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

    /// E2: the branch list comes from confirmed Git inventory. While Git has
    /// not been read live, the GitHub fetch skips instead of reading GitHub
    /// for a guessed (empty or restored) branch set.
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
}
