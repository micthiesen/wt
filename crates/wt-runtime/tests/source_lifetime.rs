use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use wt_runtime::{RefreshPolicy, SourceState, TaskScope, start_source};

#[tokio::test]
async fn projected_snapshots_do_not_request_fetches_and_explicit_requests_coalesce() {
    let (source, mut publisher) = wt_runtime::source_channel();
    for revision in 1..=20 {
        publisher.publish(wt_runtime::SourceSnapshot {
            data: Some(Arc::new(revision)),
            state: SourceState::Ready,
            updated_at: None,
            revision: 0,
        });
    }
    assert_eq!(source.snapshot().revision, 20);
    assert_eq!(source.snapshot().data.as_deref(), Some(&20));
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
    let source = start_source(&scope, RefreshPolicy::default(), {
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
        .wait_for(|s| s.state == SourceState::Ready)
        .await
        .unwrap();
    let first = source.snapshot();
    source.refresh();
    updates
        .wait_for(|s| matches!(s.state, SourceState::Failed(_)))
        .await
        .unwrap();
    let failed = source.snapshot();
    assert_eq!(failed.data.as_deref(), Some(&42));
    assert_eq!(failed.updated_at, first.updated_at);
    scope.shutdown(Duration::from_secs(1)).await.unwrap();
    assert!(!source.refresh());
}

#[tokio::test(start_paused = true)]
async fn bursts_have_one_trailing_refresh_and_never_overlap() {
    let scope = TaskScope::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let source = start_source(&scope, RefreshPolicy::default(), {
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
        RefreshPolicy {
            minimum_interval: Duration::from_secs(10),
            debounce: Duration::from_secs(1),
        },
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
    let source = start_source(&scope, RefreshPolicy::default(), {
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
    let source = start_source(&scope, RefreshPolicy::default(), move |_| {
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
        .wait_for(|s| s.state == SourceState::Ready)
        .await
        .unwrap();
    let first = source.snapshot();
    for expected in ["factory failure", "future failure"] {
        let revision = source.snapshot().revision;
        assert!(source.refresh());
        updates
            .wait_for(|s| s.revision > revision && matches!(s.state, SourceState::Failed(_)))
            .await
            .unwrap();
        let failed = source.snapshot();
        assert_eq!(failed.data.as_deref(), Some(&1));
        assert_eq!(failed.updated_at, first.updated_at);
        assert_eq!(
            failed.state,
            SourceState::Failed(format!("background source panicked: {expected}").into())
        );
    }
    assert!(source.refresh());
    updates
        .wait_for(|s| s.state == SourceState::Ready)
        .await
        .unwrap();
    assert_eq!(source.snapshot().data.as_deref(), Some(&4));
    scope.shutdown(Duration::from_secs(1)).await.unwrap();
}
