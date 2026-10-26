//! The events daemon's GitHub snapshot, read as one input of the GitHub leaf.
//! A daemon read carries the moment the daemon's GraphQL request started, so
//! rereading the same file never makes old data look fresher, and a snapshot
//! whose request began before a local write can never confirm that write.
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use notify::{RecursiveMode, Watcher};
use tokio::time::Instant;
use wt_events::GithubSnapshot;
use wt_github::GithubData;
use wt_runtime::{SourceHandle, Stamp, TaskScope};

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

/// A stale, foreign-build or incomplete snapshot is disposable. Live fetches
/// are the fallback, and retain their own last-good data on a fetch error.
pub async fn load(context: &AppContext, branches: &[String]) -> Option<DaemonRead> {
    context.config.github.events.as_ref()?;
    read(
        context.config.paths.cache_root.join("events"),
        branches.to_vec(),
        wt_ops::events::repository_identity(&context.config),
    )
    .await
}

async fn read(directory: PathBuf, branches: Vec<String>, repository: String) -> Option<DaemonRead> {
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
        Ok::<_, anyhow::Error>(validate(
            snapshot,
            &branches,
            (now_ms(), local_now),
            wt_app_core::build_info::build_id(),
            &repository,
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

/// `now` is the same moment on the wall clock (ms) and the process clock.
fn validate(
    snapshot: GithubSnapshot,
    branches: &[String],
    (now, local_now): (u64, Instant),
    build: &str,
    repository: &str,
) -> Option<DaemonRead> {
    let age = now.checked_sub(snapshot.updated_at)?;
    if age > FRESH.as_millis() as u64
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
    }))
    .ok()?;
    data.prs.retain(|branch, _| branches.contains(branch));
    data.covered_branches = branches.iter().cloned().collect();
    // A start after the write is inconsistent; such data cannot be ordered.
    let started = snapshot
        .fetch_started_at_ms
        .filter(|started| *started <= snapshot.updated_at)
        .and_then(|started| local_now.checked_sub(Duration::from_millis(now - started)));
    Some(DaemonRead {
        data,
        started,
        written_ms: snapshot.updated_at,
    })
}

/// Ask the GitHub leaf to recheck the daemon snapshot when it changes, and on
/// a slow backstop. `file_event` marks these requests so a recheck that finds
/// nothing usable does not become a network read by itself.
pub fn watch(
    scope: &TaskScope,
    context: &AppContext,
    github: SourceHandle<GithubData>,
    file_event: Arc<AtomicBool>,
) {
    if context.config.github.events.is_none() {
        return;
    }
    let directory = context.config.paths.cache_root.join("events");
    let cancellation = scope.token();
    scope.spawn(async move {
        let notify = github.clone();
        let notify_hint = file_event.clone();
        let watcher = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory)?;
            let mut watcher = wt_app_core::watcher::new(
                move |event: notify::Result<notify::Event>| match event {
                    Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => {}
                    Ok(event)
                        if !event.paths.is_empty()
                            && !event.paths.iter().any(|path| snapshot_event(path)) => {}
                    _ => {
                        notify_hint.store(true, Ordering::Release);
                        notify.refresh();
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
                    file_event.store(true, Ordering::Release);
                    github.refresh();
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
