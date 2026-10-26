//! Commands for the host-local removed-history view.

use anyhow::{Context, Result, bail};
use wt_lifecycle::{CreateOptions, LifecycleError};
use wt_tui::UiReply;

use wt_app_core::context::AppContext;

pub async fn restore(
    ctx: &AppContext,
    key: &str,
    expected_removed_at: &str,
    expected_branch: &str,
) -> Result<UiReply> {
    let slug = local_slug(key)?;
    let service = crate::lifecycle_ops::service(ctx)?;
    let result = service
        .create_from_removed(
            &slug,
            expected_branch,
            expected_removed_at,
            CreateOptions {
                fetch_origin: true,
                run_install: true,
                ..CreateOptions::default()
            },
            &ctx.cancellation,
        )
        .await
        .map_err(restore_refusal)?;
    Ok(UiReply {
        message: wt_core::sanitize_terminal_text(&format!(
            "Restored {} at {}",
            result.target.slug(),
            result.target.path
        )),
        select_when_visible: Some(wt_core::worktree_target_key(&result.target)),
        ..UiReply::default()
    })
}

pub async fn toggle_automations_paused(ctx: &AppContext, key: &str) -> Result<UiReply> {
    let slug = local_slug(key)?;
    let changed = ctx
        .database
        .call(move |store| Ok(store.toggle_removed_automations_paused(&slug)?))
        .await?;
    let Some(paused) = changed else {
        bail!("removed history entry is no longer available");
    };
    Ok(UiReply {
        message: if paused {
            "Automations paused for this removed worktree".into()
        } else {
            "Automations resumed for this removed worktree".into()
        },
        ..UiReply::default()
    })
}

fn local_slug(key: &str) -> Result<String> {
    match wt_core::parse_worktree_ledger_key(key).context("invalid removed-worktree identity")? {
        wt_core::WorktreeRef::Local { slug } => Ok(slug),
        wt_core::WorktreeRef::Remote { .. } => {
            bail!("remote history action was not routed to its host")
        }
    }
}

/// The removal the user confirmed is no longer the current one: report it as
/// stale so the menu reopens from the board. Other refusals stay failures.
fn restore_refusal(error: LifecycleError) -> anyhow::Error {
    match error {
        LifecycleError::Refused(message) if message == wt_lifecycle::REMOVED_CHANGED => {
            wt_app_core::host_errors::stale(message)
        }
        error => error.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_store::RemovedWorktree;

    #[test]
    fn a_changed_removal_is_stale_and_other_refusals_are_not() {
        let stale = restore_refusal(LifecycleError::Refused(
            wt_lifecycle::REMOVED_CHANGED.into(),
        ));
        assert!(wt_app_core::host_errors::error_reply(&stale).stale);
        let other = restore_refusal(LifecycleError::Refused("branch is checked out".into()));
        assert!(!wt_app_core::host_errors::error_reply(&other).stale);
    }

    #[tokio::test]
    async fn pause_toggle_refreshes_the_removed_record() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let entry = RemovedWorktree {
            slug: "archived".into(),
            branch: "feature/archived".into(),
            removed_at: "2026-10-09T00:00:00Z".into(),
            work: None,
            automations_paused: None,
            extra: serde_json::Map::from_iter([("futureField".into(), serde_json::json!("kept"))]),
        };
        fixture
            .ctx
            .database
            .call({
                let entry = entry.clone();
                move |store| {
                    store.record_removed_worktrees(&[entry], 1_791_540_000_000)?;
                    Ok(())
                }
            })
            .await
            .unwrap();

        let paused = toggle_automations_paused(&fixture.ctx, "archived")
            .await
            .unwrap();
        assert!(paused.message.contains("paused"));
        let records = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_removed_worktrees()?))
            .await
            .unwrap();
        assert_eq!(records[0].automations_paused, Some(true));
        assert_eq!(records[0].extra["futureField"], "kept");
        fixture.close().await.unwrap();
    }
}
