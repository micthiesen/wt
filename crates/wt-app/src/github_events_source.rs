//! Daemon snapshots update GitHub presentation independently of the network
//! fetch floor. Cache invalidation must not become another GitHub request.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use notify::{RecursiveMode, Watcher};
use wt_events::GithubSnapshot;
use wt_github::GithubData;
use wt_runtime::{
    RefreshPolicy, SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel,
    start_source,
};
use wt_tui::Board;

use crate::context::AppContext;

const FRESH: Duration = Duration::from_secs(90);
const RECONCILE: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
struct Cached {
    data: Arc<GithubData>,
    covered: BTreeSet<String>,
    observed_at: tokio::time::Instant,
}

/// A stale, foreign-build or incomplete cache is disposable. Live fetches are
/// the fallback, and retain their own last-good data on an actual fetch error.
pub async fn load(context: &AppContext, branches: &[String]) -> Option<GithubData> {
    context.config.github.events.as_ref()?;
    read(
        context.config.paths.cache_root.join("events"),
        branches.to_vec(),
    )
    .await
    .map(|cached| cached.data.as_ref().clone())
}

async fn read(directory: PathBuf, branches: Vec<String>) -> Option<Cached> {
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
        Ok::<_, anyhow::Error>(validate(snapshot, &branches, now_ms(), env!("WT_BUILD_ID")))
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

fn validate(
    snapshot: GithubSnapshot,
    branches: &[String],
    now: u64,
    build: &str,
) -> Option<Cached> {
    let age = now.checked_sub(snapshot.updated_at)?;
    if age > FRESH.as_millis() as u64 || snapshot.writer_sha.as_deref() != Some(build) {
        return None;
    }
    let covered: BTreeSet<_> = snapshot.branches.into_iter().collect();
    if branches.iter().any(|branch| !covered.contains(branch)) {
        return None;
    }
    let mut data: GithubData = serde_json::from_value(serde_json::json!({
        "prs": snapshot.prs, "mergeQueue": snapshot.merge_queue,
    }))
    .ok()?;
    data.prs.retain(|branch, _| branches.contains(branch));
    Some(Cached {
        data: Arc::new(data),
        // Payload was narrowed to the requested scope. Advertising the file's
        // larger coverage here would blank newly added rows before their read.
        covered: branches.iter().cloned().collect(),
        observed_at: tokio::time::Instant::now().checked_sub(Duration::from_millis(age))?,
    })
}

pub fn overlay(
    scope: &TaskScope,
    context: &AppContext,
    local: SourceHandle<Board>,
    live: SourceHandle<GithubData>,
) -> SourceHandle<GithubData> {
    if context.config.github.events.is_none() {
        return live;
    }
    let directory = context.config.paths.cache_root.join("events");
    let cache = start_source(
        scope,
        RefreshPolicy {
            debounce: Duration::from_millis(25),
            minimum_interval: Duration::from_millis(100),
        },
        {
            let directory = directory.clone();
            let local = local.clone();
            move |_cancel| {
                let directory = directory.clone();
                let branches = crate::sources::branches(&local.snapshot());
                async move { Ok::<_, std::convert::Infallible>(read(directory, branches).await) }
            }
        },
    );
    watch(scope, directory, cache.clone());
    project(scope, local, live, cache)
}

fn project(
    scope: &TaskScope,
    local: SourceHandle<Board>,
    live: SourceHandle<GithubData>,
    cache: SourceHandle<Option<Cached>>,
) -> SourceHandle<GithubData> {
    let (source, mut publisher) = source_channel();
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut local_updates = local.subscribe();
        let mut live_updates = live.subscribe();
        let mut cache_updates = cache.subscribe();
        local_updates.mark_changed();
        live_updates.mark_changed();
        cache_updates.mark_changed();
        let mut branches = Vec::new();
        let mut served_cache = false;
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    live.refresh(); cache.refresh(); continue;
                }
                changed = local_updates.changed() => {
                    if changed.is_err() { break; }
                    let next = crate::sources::branches(&local_updates.borrow_and_update());
                    if next != branches { branches = next; cache.refresh(); }
                }
                changed = live_updates.changed() => {
                    if changed.is_err() { break; }
                    live_updates.borrow_and_update();
                }
                changed = cache_updates.changed() => {
                    if changed.is_err() { break; }
                    cache_updates.borrow_and_update();
                }
            }
            let live_snapshot = live_updates.borrow().clone();
            let cached = cache_updates
                .borrow()
                .data
                .as_ref()
                .and_then(|cached| cached.as_ref().clone());
            let usable = cached.as_ref().filter(|cached| {
                cached.observed_at.elapsed() <= FRESH
                    && branches
                        .iter()
                        .all(|branch| cached.covered.contains(branch))
            });
            // Live requests use this same cache first and fetch over the
            // network only when it is unavailable. A request that began before
            // a new daemon snapshot must not overwrite it merely by finishing
            // later. The fresh, complete daemon snapshot is authoritative.
            let use_cache = usable.is_some();
            if use_cache {
                let cached = usable.unwrap();
                publisher.publish(SourceSnapshot {
                    data: Some(cached.data.clone()),
                    state: SourceState::Ready,
                    updated_at: Some(cached.observed_at),
                    revision: 0,
                });
            } else {
                // Once a previously displayed snapshot expires, ask the live
                // lane for replacement even if no new webhook arrives.
                if served_cache && usable.is_none() {
                    live.refresh();
                }
                publisher.publish(live_snapshot);
            }
            served_cache = use_cache;
        }
    });
    source
}

fn watch(scope: &TaskScope, directory: PathBuf, cache: SourceHandle<Option<Cached>>) {
    let cancellation = scope.token();
    scope.spawn(async move {
        let notify = cache.clone();
        let watcher = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory)?;
            let mut watcher = notify::recommended_watcher(
                move |event: notify::Result<notify::Event>| match event {
                    Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => {}
                    Ok(event)
                        if !event.paths.is_empty()
                            && !event.paths.iter().any(|path| snapshot_event(path)) => {}
                    _ => {
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
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + RECONCILE, RECONCILE);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { biased;
                _ = cancellation.cancelled() => break,
                _ = interval.tick() => { cache.refresh(); }
            }
        }
        drop(watcher);
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

    fn snapshot() -> GithubSnapshot {
        GithubSnapshot {
            updated_at: 100_000,
            branches: vec!["feature".into()],
            writer_sha: Some("current".into()),
            prs: serde_json::json!({}),
            merge_queue: serde_json::json!({}),
            ..GithubSnapshot::default()
        }
    }

    #[test]
    fn cache_requires_fresh_time_current_build_full_coverage_and_valid_payloads() {
        let branches = vec!["feature".into()];
        assert!(validate(snapshot(), &branches, 101_000, "current").is_some());
        assert!(
            validate(snapshot(), &[], 101_000, "current")
                .unwrap()
                .covered
                .is_empty()
        );
        assert!(validate(snapshot(), &branches, 190_001, "current").is_none());
        assert!(validate(snapshot(), &branches, 99_000, "current").is_none());
        assert!(validate(snapshot(), &branches, 101_000, "other").is_none());
        assert!(validate(snapshot(), &["missing".into()], 101_000, "current").is_none());
        let mut malformed = snapshot();
        malformed.prs = serde_json::json!({"feature":{"number":42}});
        assert!(validate(malformed, &branches, 101_000, "current").is_none());
        assert!(snapshot_event(Path::new("/events/.github.json.123.tmp")));
        assert!(!snapshot_event(Path::new("/events/state.json")));
    }

    #[tokio::test]
    async fn new_daemon_snapshot_updates_while_network_fetch_is_busy_without_refetching() {
        let scope = TaskScope::new();
        let (local, local_publisher) = source_channel();
        let (live, mut live_publisher) = source_channel();
        let (cache, cache_publisher) = source_channel();
        local_publisher.publish(SourceSnapshot {
            data: Some(Arc::new(Board {
                rows: vec![wt_tui::BoardRow {
                    branch: "feature".into(),
                    ..Default::default()
                }],
                ..Default::default()
            })),
            state: SourceState::Ready,
            updated_at: None,
            revision: 0,
        });
        let source = project(&scope, local, live, cache);
        live_publisher.publish(SourceSnapshot {
            data: None,
            state: SourceState::Refreshing,
            updated_at: None,
            revision: 0,
        });
        let cached = validate(snapshot(), &["feature".into()], 100_000, "current").unwrap();
        cache_publisher.publish(SourceSnapshot {
            data: Some(Arc::new(Some(cached))),
            state: SourceState::Ready,
            updated_at: Some(tokio::time::Instant::now()),
            revision: 0,
        });
        let mut updates = source.subscribe();
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|s| s.state == SourceState::Ready && s.data.is_some()),
        )
        .await
        .unwrap()
        .unwrap();
        // An older request can finish after a webhook has already supplied
        // newer data. Completion time must not make its stale result win.
        let mut stale = GithubData::default();
        stale.merge_queue.insert(
            "stale".into(),
            wt_github::MergeQueueEntry {
                head_ref_name: "stale".into(),
                position: 1,
                state: wt_github::MergeQueueState::Queued,
                enqueued_at: String::new(),
                estimated_time_to_merge: None,
            },
        );
        updates.borrow_and_update();
        live_publisher.publish(SourceSnapshot {
            data: Some(Arc::new(stale)),
            state: SourceState::Ready,
            updated_at: Some(tokio::time::Instant::now()),
            revision: 0,
        });
        tokio::time::timeout(Duration::from_secs(2), updates.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(
            updates
                .borrow()
                .data
                .as_ref()
                .unwrap()
                .merge_queue
                .is_empty()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(25), live_publisher.requested())
                .await
                .is_err()
        );
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
