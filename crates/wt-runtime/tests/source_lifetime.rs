use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use wt_runtime::{
    Domain, Persistence, RefreshPolicy, SourceSpec, SourceState, TaskScope, start_source,
};

fn spec<T>(policy: RefreshPolicy) -> SourceSpec<T> {
    SourceSpec {
        name: "test",
        domain: Domain::Git,
        persistence: Persistence::Never("test fixture"),
        policy,
    }
}

#[tokio::test]
async fn projected_snapshots_do_not_request_fetches_and_explicit_requests_coalesce() {
    let scope = TaskScope::new();
    let (source, mut publisher) = wt_runtime::derived_channel(&scope, "test");
    for revision in 1..=20 {
        publisher
            .publish(wt_runtime::SourceSnapshot::default().with_data(Some(Arc::new(revision))));
    }
    assert_eq!(source.snapshot().revision(), 20);
    assert_eq!(source.snapshot().data().map(|d| **d), Some(20));
    assert!(
        tokio::time::timeout(Duration::ZERO, publisher.requested())
            .await
            .is_err()
    );
    for _ in 0..100 {
        assert!(source.refresh());
    }
    assert_eq!(publisher.requested().await, Some(()));
    assert!(
        tokio::time::timeout(Duration::ZERO, publisher.requested())
            .await
            .is_err()
    );
    drop(publisher);
    assert!(!source.refresh());
}

#[tokio::test(start_paused = true)]
async fn idle_sources_do_no_work_and_failures_keep_successful_data() {
    let scope = TaskScope::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let source = start_source(&scope, spec(RefreshPolicy::default()), {
        let calls = calls.clone();
        move |_| {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            async move { if index == 0 { Ok(42) } else { Err("offline") } }
        }
    });
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    source.refresh();
    let mut updates = source.subscribe();
    updates
        .wait_for(|s| *s.state() == SourceState::Ready)
        .await
        .unwrap();
    let first = source.snapshot();
    source.refresh();
    updates
        .wait_for(|s| matches!(s.state(), SourceState::Failed(_)))
        .await
        .unwrap();
    let failed = source.snapshot();
    assert_eq!(failed.data().map(|d| **d), Some(42));
    assert_eq!(
        failed.domains()[&Domain::Git].observed(),
        first.domains()[&Domain::Git].observed()
    );
    scope.shutdown(Duration::from_secs(1)).await.unwrap();
    assert!(!source.refresh());
}

#[tokio::test(start_paused = true)]
async fn bursts_have_one_trailing_refresh_and_never_overlap() {
    let scope = TaskScope::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let source = start_source(&scope, spec(RefreshPolicy::default()), {
        let calls = calls.clone();
        move |_| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok::<_, String>(n)
            }
        }
    });
    source.refresh();
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for _ in 0..10_000 {
        assert!(source.refresh());
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    scope.shutdown(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn continuous_invalidations_do_not_starve_a_scheduled_refresh() {
    let scope = TaskScope::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let source = start_source(
        &scope,
        spec(RefreshPolicy {
            minimum_interval: Duration::from_secs(10),
            debounce: Duration::from_secs(1),
        }),
        {
            let calls = calls.clone();
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok::<_, String>(()) }
            }
        },
    );
    source.refresh();
    tokio::task::yield_now().await;
    for _ in 0..50 {
        tokio::time::advance(Duration::from_millis(100)).await;
        source.refresh();
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    scope.shutdown(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn shutdown_waits_for_cancellation_cleanup() {
    let scope = TaskScope::new();
    let cleaned = Arc::new(AtomicUsize::new(0));
    let source = start_source(&scope, spec(RefreshPolicy::default()), {
        let cleaned = cleaned.clone();
        move |cancel| {
            let cleaned = cleaned.clone();
            async move {
                cancel.cancelled().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
                cleaned.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>("cancelled")
            }
        }
    });
    source.refresh();
    tokio::task::yield_now().await;
    scope.shutdown(Duration::from_secs(1)).await.unwrap();
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    assert_eq!(scope.active_tasks(), 0);
}

#[tokio::test(start_paused = true)]
async fn factory_and_future_panics_are_visible_and_the_source_can_recover() {
    let scope = TaskScope::new();
    let mut calls = 0;
    let source = start_source(&scope, spec(RefreshPolicy::default()), move |_| {
        calls += 1;
        let call = calls;
        if call == 2 {
            panic!("factory failure");
        }
        async move {
            if call == 3 {
                panic!("future failure");
            }
            Ok::<_, String>(call)
        }
    });
    let mut updates = source.subscribe();
    source.refresh();
    updates
        .wait_for(|s| *s.state() == SourceState::Ready)
        .await
        .unwrap();
    let first = source.snapshot();
    for expected in ["factory failure", "future failure"] {
        let revision = source.snapshot().revision();
        assert!(source.refresh());
        updates
            .wait_for(|s| s.revision() > revision && matches!(s.state(), SourceState::Failed(_)))
            .await
            .unwrap();
        let failed = source.snapshot();
        assert_eq!(failed.data().map(|d| **d), Some(1));
        assert_eq!(
            failed.domains()[&Domain::Git].observed(),
            first.domains()[&Domain::Git].observed()
        );
        assert_eq!(
            *failed.state(),
            SourceState::Failed(format!("background source panicked: {expected}").into())
        );
    }
    assert!(source.refresh());
    updates
        .wait_for(|s| *s.state() == SourceState::Ready)
        .await
        .unwrap();
    assert_eq!(source.snapshot().data().map(|d| **d), Some(4));
    scope.shutdown(Duration::from_secs(1)).await.unwrap();
}

// Real time: the restore runs on a blocking thread that paused time does not
// wait for.
#[tokio::test]
async fn a_state_change_before_the_cache_read_does_not_discard_the_restore() {
    let directory = tempfile::tempdir().unwrap();
    let store = wt_runtime::CacheStore::new(directory.path().to_path_buf(), "scope");
    {
        let scope = TaskScope::new();
        let (_, publisher) = wt_runtime::leaf_channel(
            &scope,
            SourceSpec {
                name: "rows",
                domain: Domain::Git,
                persistence: Persistence::Disk(wt_runtime::DiskCache::new(&store)),
                policy: RefreshPolicy::default(),
            },
        );
        publisher.publish_live(vec![1u8, 2], wt_runtime::Stamp::now());
        let path = directory.path().join("rows.json");
        while !path.exists() {
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(1));
        }
        scope.shutdown(Duration::from_secs(2)).await.unwrap();
    }
    let scope = TaskScope::new();
    let (source, publisher) = wt_runtime::leaf_channel::<Vec<u8>>(
        &scope,
        SourceSpec {
            name: "rows",
            domain: Domain::Git,
            persistence: Persistence::Disk(wt_runtime::DiskCache::new(&store)),
            policy: RefreshPolicy::default(),
        },
    );
    // The first fetch starts (and fails) before the blocking read returns.
    publisher.set_state(SourceState::Failed("offline".into()));
    let mut updates = source.subscribe();
    tokio::time::timeout(
        Duration::from_secs(5),
        updates.wait_for(|s| s.data().is_some()),
    )
    .await
    .expect("restore still lands")
    .unwrap();
    let snapshot = source.snapshot();
    assert!(snapshot.is_restored());
    assert!(matches!(snapshot.state(), SourceState::Failed(_)));
    assert!(source.confirmed().is_none());
    scope.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn only_a_slow_fetch_reads_as_refreshing_and_a_skip_publishes_nothing() {
    let scope = TaskScope::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let source = wt_runtime::start_skippable_source(&scope, spec(RefreshPolicy::default()), {
        let calls = calls.clone();
        move |_| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                match call {
                    // Quick fetch: one publish, never refreshing.
                    0 => Ok::<_, String>(Some(wt_runtime::Fetched::new(
                        1u8,
                        wt_runtime::Stamp::now(),
                    ))),
                    // Quick skip: nothing published at all.
                    1 => Ok(None),
                    // Slow fetch: announced as refreshing, then live.
                    _ => {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        Ok(Some(wt_runtime::Fetched::new(
                            2u8,
                            wt_runtime::Stamp::now(),
                        )))
                    }
                }
            }
        }
    });
    let mut updates = source.subscribe();
    source.refresh();
    updates.wait_for(|s| s.data().is_some()).await.unwrap();
    assert_eq!(
        source.snapshot().revision(),
        1,
        "a quick fetch publishes once"
    );
    source.refresh();
    while calls.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(500)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        source.snapshot().revision(),
        1,
        "a quick skip publishes nothing"
    );
    source.refresh();
    updates
        .wait_for(|s| *s.state() == SourceState::Refreshing)
        .await
        .unwrap();
    updates
        .wait_for(|s| s.data().map(|d| **d) == Some(2))
        .await
        .unwrap();
    assert_eq!(*source.snapshot().state(), SourceState::Ready);
    scope.shutdown(Duration::from_secs(1)).await.unwrap();
}
