//! Shared conformance checks. Every persisted source type and every mutation
//! kind runs these from its owning crate's tests, on paused tokio time, so a
//! new source or mutation cannot opt out of the layer's guarantees.
use std::fmt::Debug;
use std::time::Duration;

use serde::{Serialize, de::DeserializeOwned};
use tokio::time::Instant;

use crate::{
    CacheStore, DiskCache, Mutation, Pending, PendingPhase, Persistence, RefreshPolicy, SourceSpec,
    SourceState, Stamp, TaskScope, leaf_channel, start_source,
};
use wt_core::Domain;

/// A persisted source type survives a restart as restored, non-confirmable
/// display data; a live fetch replaces it and becomes confirmable; a failed
/// refresh keeps the last good data; a restore never lands after live data.
///
/// `name` and `domain` must match the production spec; `sample` is a typical
/// value of the source's data.
pub async fn persisted_source<T>(name: &'static str, domain: Domain, sample: T)
where
    T: Serialize + DeserializeOwned + Clone + PartialEq + Debug + Send + Sync + 'static,
{
    let directory = tempfile::tempdir().expect("temporary cache directory");
    let store = CacheStore::new(directory.path().to_path_buf(), "conformance");

    // First process: fetch live, which saves the cache.
    {
        let scope = TaskScope::new();
        let value = sample.clone();
        let source = start_source(&scope, disk_spec(name, domain, &store), move |_| {
            let value = value.clone();
            async move { Ok::<_, String>(value) }
        });
        assert!(
            source.confirmed().is_none(),
            "{name}: empty source confirms nothing"
        );
        source.refresh();
        let mut updates = source.subscribe();
        updates
            .wait_for(|s| *s.state() == SourceState::Ready)
            .await
            .expect("live fetch");
        assert_eq!(
            source.confirmed().as_deref(),
            Some(&sample),
            "{name}: live data confirms"
        );
        wait_for_file(&store, name).await;
        scope
            .shutdown(Duration::from_secs(2))
            .await
            .expect("shutdown");
    }

    // Second process: the cache is shown before the first fetch, as restored.
    {
        let scope = TaskScope::new();
        let fetches = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = start_source(&scope, disk_spec(name, domain, &store), {
            let fetches = fetches.clone();
            move |_| {
                fetches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Err::<T, _>("offline") }
            }
        });
        let mut updates = source.subscribe();
        updates
            .wait_for(|s| s.data().is_some())
            .await
            .expect("restored data");
        let restored = source.snapshot();
        assert_eq!(
            fetches.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{name}: shown before any fetch"
        );
        assert_eq!(restored.data().map(|d| (**d).clone()), Some(sample.clone()));
        assert!(restored.is_restored(), "{name}: marked as restored");
        assert!(
            source.confirmed().is_none(),
            "{name}: restored data can never authorize an action"
        );
        // A failed refresh keeps the last good (restored) data, still stale.
        source.refresh();
        updates
            .wait_for(|s| matches!(s.state(), SourceState::Failed(_)))
            .await
            .expect("failed refresh");
        let failed = source.snapshot();
        assert_eq!(failed.data().map(|d| (**d).clone()), Some(sample.clone()));
        assert!(failed.is_restored());
        assert!(source.confirmed().is_none());
        scope
            .shutdown(Duration::from_secs(2))
            .await
            .expect("shutdown");
    }

    // Third process: live data published first is never replaced by a
    // late restore.
    {
        let scope = TaskScope::new();
        let (source, publisher) = leaf_channel(&scope, disk_spec(name, domain, &store));
        publisher.publish_live(sample.clone(), Stamp::now());
        // The restore reads on a real blocking thread; give it real time to
        // finish (a small file takes well under this) before asserting.
        settle_real_time(Duration::from_millis(300)).await;
        assert!(
            !source.snapshot().is_restored(),
            "{name}: live data stays live"
        );
        assert!(source.confirmed().is_some());
        scope
            .shutdown(Duration::from_secs(2))
            .await
            .expect("shutdown");
    }

    // A cache from another configuration, build or repository is discarded.
    let other = CacheStore::new(directory.path().to_path_buf(), "another scope");
    let scope = TaskScope::new();
    let source = start_source(&scope, disk_spec(name, domain, &other), |_| async {
        Err::<T, _>("offline")
    });
    // Deletion is the observable end of the rejected restore.
    let path = directory.path().join(format!("{name}.json"));
    for _ in 0..2000 {
        if !path.exists() {
            break;
        }
        settle_real_time(Duration::from_millis(1)).await;
    }
    assert!(!path.exists(), "{name}: foreign cache deleted");
    settle_real_time(Duration::from_millis(20)).await;
    assert!(
        source.snapshot().data().is_none(),
        "{name}: foreign cache ignored"
    );
    scope
        .shutdown(Duration::from_secs(2))
        .await
        .expect("shutdown");
}

fn disk_spec<T>(name: &'static str, domain: Domain, store: &CacheStore) -> SourceSpec<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    SourceSpec {
        name,
        domain,
        persistence: Persistence::Disk(DiskCache::new(store)),
        policy: RefreshPolicy::default(),
    }
}

/// Let blocking-pool work progress for `duration` of real time while the
/// test runtime's clock may be paused.
async fn settle_real_time(duration: Duration) {
    let until = std::time::Instant::now() + duration;
    while std::time::Instant::now() < until {
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    tokio::task::yield_now().await;
}

async fn wait_for_file(store: &CacheStore, name: &str) {
    let path = store.directory().join(format!("{name}.json"));
    for _ in 0..1000 {
        if path.exists() {
            return;
        }
        // The write runs on a blocking thread, outside paused time.
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("{name}: cache file was never written");
}

/// A mutation kind's optimistic behavior.
///
/// `before` is confirmed data without the change; `after` is confirmed data a
/// fetch would return once the change is applied. The checks: projecting
/// shows the change; a stale fetch or one that started before the write was
/// accepted never clears or reverts it; it stays until observed, failure or
/// the deadline; and a fetch that started afterwards and shows it settles it.
pub async fn mutation<T, M>(mutation: M, before: T, after: T)
where
    T: Clone + Debug,
    M: Mutation<T> + Debug,
{
    mutation_with_projection(mutation, before, after, |mutation, view| {
        mutation.observed(view)
    })
    .await;
}

/// The same settlement checks for progress projections, whose visible pending
/// state deliberately differs from the final result (for example "removing").
/// The predicate checks presentation only; settlement still uses `observed`.
pub async fn mutation_with_projection<T, M>(
    mutation: M,
    before: T,
    after: T,
    projected: impl Fn(&M, &T) -> bool,
) where
    T: Clone + Debug,
    M: Mutation<T> + Debug,
{
    let label = mutation.label();
    assert!(
        !mutation.domains().is_empty(),
        "{label}: declares its domains"
    );
    assert!(
        !mutation.observed(&before),
        "{label}: `before` must not show the change"
    );
    assert!(
        mutation.observed(&after),
        "{label}: `after` must show the change"
    );
    assert!(
        !projected(&mutation, &before),
        "{label}: no projection before mutation"
    );
    let mut view = before.clone();
    mutation.apply(&mut view);
    assert!(
        projected(&mutation, &view),
        "{label}: projection shows the change"
    );
    let mut again = view.clone();
    mutation.apply(&mut again);
    assert!(
        projected(&mutation, &again),
        "{label}: projection is idempotent"
    );

    let mut pending = Pending::<T, M>::default();
    let stale = Stamp::now();
    tokio::time::advance(Duration::from_millis(5)).await;
    pending.begin(1, mutation.clone(), Instant::now());
    // In flight: nothing settles it, even data that already shows it.
    pending.settle(&after, |_, _| Some(Stamp::now()), Instant::now());
    assert_eq!(pending.len(), 1, "{label}: in-flight waits for the reply");
    assert_eq!(pending.views()[0].phase, PendingPhase::InFlight);
    tokio::time::advance(Duration::from_millis(5)).await;
    let accepted = Stamp::now();
    pending.accept(1, accepted, Instant::now());

    // A fetch that started before the write, even one that happens to show
    // the change (an older value equal to the target), cannot settle it.
    pending.settle(&after, |_, _| Some(stale), Instant::now());
    assert_eq!(pending.len(), 1, "{label}: an older fetch cannot settle it");
    // Fresh fetches that still show the old value do not revert it.
    for _ in 0..10 {
        tokio::time::advance(Duration::from_secs(1)).await;
        let expired = pending.settle(&before, |_, _| Some(Stamp::now()), Instant::now());
        assert!(expired.is_empty(), "{label}: no revert before the deadline");
        assert!(
            projected(&mutation, &pending.project(&before)),
            "{label}: still shown"
        );
    }
    // A fetch that started after acceptance and shows it settles it.
    pending.settle(&after, |_, _| Some(Stamp::now()), Instant::now());
    assert!(pending.is_empty(), "{label}: settled by a confirming fetch");

    // Failure stops showing it at once.
    pending.begin(2, mutation.clone(), Instant::now());
    pending.remove(2);
    assert!(
        !projected(&mutation, &pending.project(&before)),
        "{label}: failure rolls back"
    );

    // The deadline is bounded and reports the unobserved change.
    pending.begin(3, mutation.clone(), Instant::now());
    pending.accept(3, Stamp::now(), Instant::now());
    let bound = mutation
        .deadlines()
        .map_or(Duration::from_secs(30), |bounds| bounds.accepted);
    tokio::time::advance(bound + Duration::from_secs(1)).await;
    let expired = pending.settle(&before, |_, _| Some(Stamp::now()), Instant::now());
    assert_eq!(expired.len(), 1, "{label}: deadline is bounded");
    assert!(pending.is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Target;
    use std::collections::BTreeMap;

    #[tokio::test(start_paused = true)]
    async fn a_persisted_source_meets_the_contract() {
        persisted_source(
            "conformance-rows",
            Domain::Git,
            vec!["a".to_owned(), "b".to_owned()],
        )
        .await;
    }

    #[derive(Clone, Debug)]
    struct Flag(bool);

    impl Mutation<BTreeMap<String, bool>> for Flag {
        fn key(&self) -> String {
            "flag".into()
        }
        fn target(&self) -> Target {
            Target::row(None, "row")
        }
        fn label(&self) -> String {
            "flagging".into()
        }
        fn domains(&self) -> &'static [Domain] {
            &[Domain::Metadata]
        }
        fn apply(&self, view: &mut BTreeMap<String, bool>) {
            view.insert("row".into(), self.0);
        }
        fn observed(&self, confirmed: &BTreeMap<String, bool>) -> bool {
            confirmed.get("row") == Some(&self.0)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_mutation_meets_the_contract() {
        let before: BTreeMap<_, _> = [("row".to_owned(), false)].into();
        let after: BTreeMap<_, _> = [("row".to_owned(), true)].into();
        mutation(Flag(true), before, after).await;
    }
}
