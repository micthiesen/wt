//! The events daemon's GitHub snapshot, read as one input of the GitHub leaf.
//! A daemon read carries the moment the daemon's GraphQL request started, so
//! rereading the same file never makes old data look fresher, and a snapshot
//! whose request began before a local write can never confirm that write.
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use notify::{RecursiveMode, Watcher};
use tokio::time::Instant;
use wt_events::GithubSnapshot;
use wt_github::GithubData;
use wt_runtime::{Request, SourceHandle, Stamp, TaskScope};

use wt_app_core::context::AppContext;

/// Daemon data older than this is not used; the GitHub leaf reads directly.
pub const FRESH: Duration = Duration::from_secs(90);
/// Rechecks the snapshot when a file event was missed or the shown data aged out.
const RECONCILE: Duration = Duration::from_secs(15);

/// A validated daemon snapshot narrowed to the requested branches.
#[derive(Clone, Debug)]
pub struct DaemonRead {
    pub data: GithubData,
    /// When the daemon's GitHub read started, on this process clock. `None`
    /// when the writer did not record it; such data never orders against
    /// local writes.
    pub started: Option<Instant>,
    /// Branches the daemon read again after `started`, with when that read
    /// of each began on this process clock. Only these are newer than
    /// `started`; a branch here without an entry in `data` had no pull
    /// request when it was read.
    pub scoped: BTreeMap<String, Instant>,
    /// When the daemon wrote the snapshot, in wall-clock milliseconds as
    /// recorded in the file. Identifies the file version: unlike a moment
    /// converted onto the process clock, it is identical on every reread.
    pub written_ms: u64,
}

impl DaemonRead {
    /// The observation stamp the GitHub leaf publishes for this data.
    pub fn stamp(&self) -> Stamp {
        self.started.map_or_else(Stamp::unordered, Stamp::at)
    }
}

/// How long past the daemon's planned next read a display still waits for
/// it: the delivery that prompts the read and the read itself take time.
const LATE: Duration = Duration::from_secs(30);

/// Why a reader that only displays pull requests may show a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    /// Within [`FRESH`]: what the board itself would use.
    Fresh,
    /// Older, and the daemon's budget keeps its next read until then
    /// (wall-clock epoch ms).
    Paced { next_fetch_at_ms: u64 },
    /// The daemon's next read is overdue and the budget it saw is down to
    /// its reserve, in a window still open: a read here would spend it.
    BudgetLow,
}

/// A snapshot a display may show in place of a GitHub read. Never evidence
/// for a write: only [`Held::Fresh`] data is what [`load`] would return.
#[derive(Clone, Debug)]
pub struct DisplayRead {
    pub read: DaemonRead,
    pub age_ms: u64,
    pub held: Held,
}

impl DisplayRead {
    /// What a command says under data older than the board would use.
    pub fn note(&self) -> Option<String> {
        let minutes = (self.age_ms.saturating_add(30_000) / 60_000).max(1);
        match self.held {
            Held::Fresh => None,
            Held::Paced { .. } => Some(format!(
                "GitHub reads are paced, using data {minutes} min old"
            )),
            Held::BudgetLow => Some(format!("GitHub budget low, using data {minutes} min old")),
        }
    }
}

/// A stale, foreign-build or incomplete snapshot is disposable. Live fetches
/// are the fallback, and retain their own last-good data on a fetch error.
pub async fn load(context: &AppContext, branches: &[String]) -> Option<DaemonRead> {
    read(context, branches, false).await.map(|shown| shown.read)
}

/// [`load`] for a command that only displays pull requests: it also takes
/// a snapshot the daemon's budget is holding, so a low budget is not spent
/// by every command that prints a table.
pub async fn load_for_display(context: &AppContext, branches: &[String]) -> Option<DisplayRead> {
    read(context, branches, true).await
}

async fn read(context: &AppContext, branches: &[String], display: bool) -> Option<DisplayRead> {
    context.config.github.events.as_ref()?;
    let directory = context.config.paths.cache_root.join("events");
    let branches = branches.to_vec();
    let repository = wt_ops::events::repository_identity(&context.config);
    let result = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        const LIMIT: u64 = 16 * 1024 * 1024;
        let file = match std::fs::File::open(directory.join("github.json")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(anyhow::Error::from(error)),
        };
        let mut bytes = Vec::new();
        file.take(LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT {
            anyhow::bail!("GitHub snapshot exceeds 16 MiB");
        }
        let snapshot = serde_json::from_slice(&bytes)?;
        // Read the process clock first so the wall-clock age it is offset by
        // can only place the daemon's moments earlier, never later.
        let local_now = Instant::now();
        Ok::<_, anyhow::Error>(admit(
            snapshot,
            &branches,
            (now_ms(), local_now),
            wt_app_core::build_info::build_id(),
            &repository,
            display,
        ))
    })
    .await;
    match result {
        Ok(Ok(cached)) => cached,
        error => {
            tracing::warn!(
                ?error,
                "cannot use GitHub events snapshot; falling back to GitHub"
            );
            None
        }
    }
}

/// The snapshot's age and why it may still be shown at `now`, if it may.
fn held(snapshot: &GithubSnapshot, now: u64) -> Option<(u64, Held)> {
    // A write after a read of a few branches leaves the rest as old as the
    // last read of every branch, and the snapshot is as old as that.
    let read_at = snapshot
        .swept_at_ms
        .map_or(snapshot.updated_at, |swept| swept.min(snapshot.updated_at));
    now.checked_sub(snapshot.updated_at)?;
    let age = now.checked_sub(read_at)?;
    if age <= FRESH.as_millis() as u64 {
        return Some((age, Held::Fresh));
    }
    // The daemon never plans further ahead than its pacing cap.
    let planned = snapshot
        .next_fetch_at_ms
        .filter(|next| next.saturating_sub(snapshot.updated_at) <= wt_events::MAX_PACED_WAIT_MS);
    if let Some(next_fetch_at_ms) =
        planned.filter(|next| now <= next.saturating_add(LATE.as_millis() as u64))
    {
        return Some((age, Held::Paced { next_fetch_at_ms }));
    }
    snapshot
        .rate_limit
        .is_some_and(|budget| wt_events::budget_low(budget, now))
        .then_some((age, Held::BudgetLow))
}

/// `now` is the same moment on the wall clock (ms) and the process clock.
/// Only a `display` reader is given a snapshot older than [`FRESH`].
fn admit(
    snapshot: GithubSnapshot,
    branches: &[String],
    (now, local_now): (u64, Instant),
    build: &str,
    repository: &str,
    display: bool,
) -> Option<DisplayRead> {
    let (age_ms, held) = held(&snapshot, now)?;
    if (!display && held != Held::Fresh)
        || snapshot.writer_sha.as_deref() != Some(build)
        || snapshot.repository.as_deref() != Some(repository)
    {
        return None;
    }
    if branches
        .iter()
        .any(|branch| !snapshot.branches.contains(branch))
    {
        return None;
    }
    let mut data: GithubData = serde_json::from_value(serde_json::json!({
        "prs": snapshot.prs, "mergeQueue": snapshot.merge_queue,
        "rateLimit": snapshot.rate_limit,
    }))
    .ok()?;
    data.prs.retain(|branch, _| branches.contains(branch));
    data.covered_branches = branches.iter().cloned().collect();
    // A start after the write is inconsistent; such data cannot be ordered.
    let fetch_started = snapshot
        .fetch_started_at_ms
        .filter(|started| *started <= snapshot.updated_at);
    data.observed_at_ms = fetch_started;
    let on_process_clock =
        |at_ms: u64| local_now.checked_sub(Duration::from_millis(now.checked_sub(at_ms)?));
    let started = fetch_started.and_then(on_process_clock);
    // A scoped read began after the read of every branch and before this
    // write. An entry that says otherwise orders nothing.
    let scoped = fetch_started
        .map(|swept| {
            snapshot
                .branch_reads
                .iter()
                .filter(|(branch, read_at)| {
                    (swept..=snapshot.updated_at).contains(*read_at) && branches.contains(*branch)
                })
                .filter_map(|(branch, read_at)| Some((branch.clone(), on_process_clock(*read_at)?)))
                .collect()
        })
        .unwrap_or_default();
    Some(DisplayRead {
        read: DaemonRead {
            data,
            started,
            scoped,
            written_ms: snapshot.updated_at,
        },
        age_ms,
        held,
    })
}

/// Ask the GitHub leaf to recheck the daemon snapshot when it changes, and on
/// a slow backstop. These are `Request::Recheck`, so a recheck that finds
/// nothing usable does not become a network read by itself.
pub fn watch(scope: &TaskScope, context: &AppContext, github: SourceHandle<GithubData>) {
    if context.config.github.events.is_none() {
        return;
    }
    let directory = context.config.paths.cache_root.join("events");
    let cancellation = scope.token();
    scope.spawn(async move {
        let notify = github.clone();
        let watcher = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory)?;
            let mut watcher = wt_app_core::watcher::new(
                move |event: notify::Result<notify::Event>| match event {
                    Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => {}
                    Ok(event)
                        if !event.paths.is_empty()
                            && !event.paths.iter().any(|path| snapshot_event(path)) => {}
                    _ => {
                        notify.request(Request::Recheck);
                    }
                },
            )?;
            watcher.watch(&directory, RecursiveMode::NonRecursive)?;
            Ok::<_, anyhow::Error>(watcher)
        })
        .await;
        if !matches!(watcher, Ok(Ok(_))) {
            tracing::warn!(
                ?watcher,
                "GitHub snapshot watcher unavailable; using backstop"
            );
        }
        let mut interval = tokio::time::interval_at(Instant::now() + RECONCILE, RECONCILE);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { biased;
                _ = cancellation.cancelled() => break,
                _ = interval.tick() => {
                    github.request(Request::Recheck);
                }
            }
        }
        wt_app_core::watcher_cleanup::drop_watcher(watcher).await;
    });
}

fn snapshot_event(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.trim_start_matches('.').starts_with("github."))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_core::Domain;
    use wt_runtime::{Persistence, RefreshPolicy, SourceSpec, leaf_channel};

    fn at(now: u64) -> (u64, Instant) {
        (now, Instant::now())
    }

    /// The board's rule.
    fn validate(
        snapshot: GithubSnapshot,
        branches: &[String],
        now: (u64, Instant),
        build: &str,
        repository: &str,
    ) -> Option<DaemonRead> {
        admit(snapshot, branches, now, build, repository, false).map(|shown| shown.read)
    }

    fn snapshot() -> GithubSnapshot {
        GithubSnapshot {
            updated_at: 100_000,
            fetch_started_at_ms: Some(98_000),
            branches: vec!["feature".into()],
            writer_sha: Some("current".into()),
            repository: Some("repo".into()),
            prs: serde_json::json!({}),
            merge_queue: serde_json::json!({}),
            ..GithubSnapshot::default()
        }
    }

    #[test]
    fn cache_requires_fresh_time_current_build_full_coverage_and_valid_payloads() {
        let branches = vec!["feature".into()];
        assert!(validate(snapshot(), &branches, at(101_000), "current", "repo").is_some());
        assert!(validate(snapshot(), &[], at(101_000), "current", "repo").is_some());
        assert!(validate(snapshot(), &branches, at(190_001), "current", "repo").is_none());
        assert!(validate(snapshot(), &branches, at(99_000), "current", "repo").is_none());
        assert!(validate(snapshot(), &branches, at(101_000), "other", "repo").is_none());
        // Another repository's or configuration's daemon is never read.
        assert!(validate(snapshot(), &branches, at(101_000), "current", "elsewhere").is_none());
        let mut unscoped = snapshot();
        unscoped.repository = None;
        assert!(validate(unscoped, &branches, at(101_000), "current", "repo").is_none());
        assert!(
            validate(
                snapshot(),
                &["missing".into()],
                at(101_000),
                "current",
                "repo"
            )
            .is_none()
        );
        let mut malformed = snapshot();
        malformed.prs = serde_json::json!({"feature":{"number":42}});
        assert!(validate(malformed, &branches, at(101_000), "current", "repo").is_none());
        assert!(snapshot_event(Path::new("/events/.github.json.123.tmp")));
        assert!(!snapshot_event(Path::new("/events/state.json")));
    }

    /// A write after a read of one branch does not make the other branches
    /// fresh: the snapshot is as old as its last read of every branch, and
    /// keeps that read's start as its stamp. The branch read again is named
    /// with its own, later start.
    #[test]
    fn a_scoped_write_keeps_the_age_and_stamp_of_the_last_read_of_every_branch() {
        let branches = vec!["feature".to_owned()];
        let scoped = |swept_at_ms, updated_at, reads: &[(&str, u64)]| GithubSnapshot {
            updated_at,
            swept_at_ms: Some(swept_at_ms),
            branch_reads: reads
                .iter()
                .map(|(branch, at)| ((*branch).to_owned(), *at))
                .collect(),
            ..snapshot()
        };
        let now = at(180_000);
        let local_now = now.1;
        let read = validate(
            scoped(100_000, 179_000, &[("feature", 170_000), ("gone", 170_000)]),
            &branches,
            now,
            "current",
            "repo",
        )
        .unwrap();
        assert_eq!(read.data.observed_at_ms, Some(98_000));
        assert_eq!(
            read.started,
            local_now.checked_sub(Duration::from_millis(82_000))
        );
        assert_eq!(read.written_ms, 179_000);
        // Only a branch that was asked for is named.
        assert_eq!(
            read.scoped,
            BTreeMap::from([(
                "feature".to_owned(),
                local_now
                    .checked_sub(Duration::from_millis(10_000))
                    .unwrap()
            )])
        );
        // Written a second ago, but its other branches were read 95 seconds
        // ago: the board reads GitHub instead.
        assert!(
            validate(
                scoped(100_000, 194_000, &[("feature", 190_000)]),
                &branches,
                at(195_000),
                "current",
                "repo"
            )
            .is_none()
        );
        // A scoped read cannot have begun before the read of every branch
        // or after the write.
        let odd = validate(
            scoped(100_000, 179_000, &[("feature", 97_999)]),
            &branches,
            now,
            "current",
            "repo",
        )
        .unwrap();
        assert!(odd.scoped.is_empty());
        let odd = validate(
            scoped(100_000, 179_000, &[("feature", 179_001)]),
            &branches,
            now,
            "current",
            "repo",
        )
        .unwrap();
        assert!(odd.scoped.is_empty());
    }

    /// A display takes a snapshot the board would not: one the daemon's
    /// pace still holds, or one whose budget is down to the reserve. Past
    /// both it reads GitHub like any other reader.
    #[test]
    fn a_display_shows_a_snapshot_the_budget_is_holding_and_says_how_old() {
        const MINUTE: u64 = 60_000;
        let branches = vec!["feature".to_owned()];
        let display = |snapshot: GithubSnapshot, now| {
            admit(snapshot, &branches, at(now), "current", "repo", true)
        };
        let budget = |remaining| {
            Some(wt_events::RateBudget {
                limit: 5_000,
                remaining,
                cost: 5,
                reset_at_ms: 100_000 + 40 * MINUTE,
            })
        };
        let fresh = display(snapshot(), 101_000).unwrap();
        assert_eq!(
            (fresh.held, fresh.age_ms, fresh.note()),
            (Held::Fresh, 1_000, None)
        );
        assert_eq!(fresh.read.data.observed_at_ms, Some(98_000));
        // No plan and a healthy budget: stale is stale.
        assert!(display(snapshot(), 100_000 + 2 * MINUTE).is_none());

        let paced = GithubSnapshot {
            next_fetch_at_ms: Some(98_000 + 10 * MINUTE),
            rate_limit: budget(1_400),
            ..snapshot()
        };
        let shown = display(paced.clone(), 100_000 + 8 * MINUTE).unwrap();
        assert_eq!(
            shown.held,
            Held::Paced {
                next_fetch_at_ms: 98_000 + 10 * MINUTE
            }
        );
        assert_eq!(
            shown.note().as_deref(),
            Some("GitHub reads are paced, using data 8 min old")
        );
        // The board keeps its own rule and its own paced reads.
        assert!(
            validate(
                paced.clone(),
                &branches,
                at(100_000 + 8 * MINUTE),
                "current",
                "repo"
            )
            .is_none()
        );
        // Still nothing for another build, repository or branch set.
        assert!(
            admit(
                paced.clone(),
                &branches,
                at(100_000 + 8 * MINUTE),
                "other",
                "repo",
                true
            )
            .is_none()
        );
        assert!(
            admit(
                paced.clone(),
                &["missing".into()],
                at(100_000 + 8 * MINUTE),
                "current",
                "repo",
                true
            )
            .is_none()
        );
        // The planned read is overdue and the budget is healthy: read GitHub.
        assert!(display(paced.clone(), 100_000 + 11 * MINUTE).is_none());
        // A plan further out than the daemon ever makes is not believed.
        let absurd = GithubSnapshot {
            next_fetch_at_ms: Some(100_000 + 60 * MINUTE),
            ..paced
        };
        assert!(display(absurd, 100_000 + 8 * MINUTE).is_none());

        let low = GithubSnapshot {
            next_fetch_at_ms: Some(98_000 + 15 * MINUTE),
            rate_limit: budget(900),
            ..snapshot()
        };
        let shown = display(low.clone(), 100_000 + 25 * MINUTE).unwrap();
        assert_eq!(shown.held, Held::BudgetLow);
        assert_eq!(
            shown.note().as_deref(),
            Some("GitHub budget low, using data 25 min old")
        );
        // Once the window has reset the stored budget says nothing.
        assert!(display(low, 100_000 + 41 * MINUTE).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_daemon_fetch_start_orders_its_data_and_a_missing_start_never_orders() {
        // Fix the process clock origin before the moments this test names.
        let _ = Stamp::now();
        tokio::time::advance(Duration::from_secs(10)).await;
        let read = validate(
            snapshot(),
            &["feature".into()],
            at(101_000),
            "current",
            "repo",
        )
        .unwrap();
        assert_eq!(
            read.stamp(),
            Stamp::at(Instant::now() - Duration::from_secs(3))
        );
        // Started 3s ago; the version is the file's own write time.
        assert_eq!(read.written_ms, 100_000);
        // A reread a fraction of a millisecond later names the same version.
        tokio::time::advance(Duration::from_micros(300)).await;
        let reread = validate(
            snapshot(),
            &["feature".into()],
            at(101_000),
            "current",
            "repo",
        )
        .unwrap();
        assert_eq!(reread.written_ms, read.written_ms);
        let legacy = GithubSnapshot {
            fetch_started_at_ms: None,
            ..snapshot()
        };
        let read = validate(legacy, &["feature".into()], at(101_000), "current", "repo").unwrap();
        assert!(!read.stamp().is_ordered());
    }

    /// Work between reading the clocks and converting the payload never moves
    /// the daemon's request start later, where it could postdate an accept.
    #[tokio::test(start_paused = true)]
    async fn a_slow_validation_never_places_the_daemon_start_later() {
        let _ = Stamp::now();
        tokio::time::advance(Duration::from_secs(10)).await;
        let read_at = Instant::now();
        tokio::time::advance(Duration::from_secs(1)).await;
        let read = validate(
            snapshot(),
            &["feature".into()],
            (101_000, read_at),
            "current",
            "repo",
        )
        .unwrap();
        assert_eq!(read.stamp(), Stamp::at(read_at - Duration::from_secs(3)));
    }

    /// A daemon read that finished after a write was accepted, but whose
    /// GitHub request started before it, cannot confirm that write.
    #[tokio::test(start_paused = true)]
    async fn a_daemon_snapshot_started_before_an_accept_does_not_observe_it() {
        use crate::github_actions::tests::{
            ledger_with_accepted_enable, open_pr_data, with_auto_merge,
        };
        let _ = Stamp::now();
        tokio::time::advance(Duration::from_secs(10)).await;
        let scope = TaskScope::new();
        let (github, publisher) = leaf_channel(
            &scope,
            SourceSpec {
                name: "github",
                domain: Domain::Github,
                persistence: Persistence::Never("test"),
                policy: RefreshPolicy::default(),
            },
        );
        // The daemon's request started 2s ago and its file was written 100ms ago.
        let mut daemon = snapshot();
        daemon.fetch_started_at_ms = Some(100_000);
        daemon.updated_at = 101_900;
        daemon.prs = serde_json::to_value(with_auto_merge(open_pr_data("feature")).prs).unwrap();
        let read = validate(daemon, &["feature".into()], at(102_000), "current", "repo").unwrap();
        // The arm was accepted 1s ago: after the daemon request started.
        let accepted = Stamp::at(Instant::now() - Duration::from_secs(1));
        let mut ledger = ledger_with_accepted_enable("feature", 7, accepted);
        publisher.publish_live(read.data.clone(), read.stamp());
        let confirmed = github.confirmed().unwrap();
        assert!(confirmed.prs["feature"].auto_merge.is_some());
        ledger.settle(
            &confirmed,
            |_, _| Some(confirmed.observed()),
            Instant::now(),
        );
        assert_eq!(
            ledger.len(),
            1,
            "a read that began before the write cannot settle it"
        );
        // A direct read that starts after the write settles it.
        publisher.publish_live(read.data, Stamp::now());
        let confirmed = github.confirmed().unwrap();
        ledger.settle(
            &confirmed,
            |_, _| Some(confirmed.observed()),
            Instant::now(),
        );
        assert!(ledger.is_empty());
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
