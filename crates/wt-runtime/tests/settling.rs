//! The settlement window is process-global, so these checks run in their own
//! test binary.
use std::time::Duration;

use wt_core::Domain;
use wt_runtime::{Persistence, RefreshPolicy, SourceSpec, Stamp, TaskScope};

#[tokio::test(start_paused = true)]
async fn identical_refetches_carry_stamps_only_while_a_write_settles() {
    let scope = TaskScope::new();
    let (source, publisher) = wt_runtime::leaf_channel(
        &scope,
        SourceSpec {
            name: "rows",
            domain: Domain::Git,
            persistence: Persistence::Never("test fixture"),
            policy: RefreshPolicy::default(),
        },
    );
    publisher.publish_live(1u8, Stamp::now());
    let first = source.snapshot();
    tokio::time::advance(Duration::from_millis(5)).await;
    publisher.publish_live(1u8, Stamp::now());
    let refetch = source.snapshot();
    publisher.publish_live(2u8, Stamp::now());
    let changed = source.snapshot();

    assert!(!wt_runtime::settling());
    assert!(
        first.same_view(&refetch),
        "an idle identical refetch changes nothing visible"
    );
    assert!(!first.same_view(&changed));
    wt_runtime::hold_for_settlement(Duration::from_secs(30));
    assert!(wt_runtime::settling());
    assert!(
        !first.same_view(&refetch),
        "while settling, the newer stamp must get through"
    );
    scope.shutdown(Duration::from_secs(1)).await.unwrap();
}
