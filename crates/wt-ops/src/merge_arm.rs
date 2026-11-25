//! The durable record that wt armed a pull request's merge, which
//! `pr.merge_dropped` later compares GitHub against. Every path that arms a
//! merge (`wt merge`, the TUI's arm and ship, the background retry) records
//! it here, and every path that cancels one forgets it here.

use wt_app_core::database::Database;
use wt_core::{MergeArm, MergeMechanism};
use wt_github::{GhActionResult, PrMergeTarget};

/// Record an arm GitHub accepted for `slug`'s pull request. A result that
/// is not a confirmed success records nothing: an arm that may not exist is
/// not one whose loss is news.
pub async fn armed(
    database: &Database,
    slug: &str,
    target: &PrMergeTarget,
    result: &GhActionResult,
) {
    if !result.ok {
        return;
    }
    let arm = MergeArm {
        pr: target.number,
        head: (!target.head_ref_oid.is_empty()).then(|| target.head_ref_oid.clone()),
        armed_at_ms: now_ms(),
        mechanism: if result.queued {
            MergeMechanism::Queue
        } else {
            MergeMechanism::AutoMerge
        },
    };
    let owner = slug.to_owned();
    if let Err(error) = database
        .call(move |store| Ok(store.set_slug_merge_arm(&owner, &arm)?))
        .await
    {
        tracing::warn!(slug, pr = target.number, %error, "cannot record the armed merge; a dropped merge will not be reported");
    }
}

/// Forget `slug`'s armed merge: the person or an agent cancelled it.
pub async fn cancelled(database: &Database, slug: &str) {
    let owner = slug.to_owned();
    if let Err(error) = database
        .call(move |store| Ok(store.clear_slug_merge_arm(&owner, None)?))
        .await
    {
        tracing::warn!(slug, %error, "cannot forget the cancelled merge");
    }
}

fn now_ms() -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .unwrap_or(i64::MAX)
}
