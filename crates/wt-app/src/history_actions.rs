//! Commands for the host-local removed-history view.

use anyhow::{Context, Result, bail};
use wt_lifecycle::CreateOptions;
use wt_store::RemovedWorktree;
use wt_tui::{ConfirmAction, UiModal, UiReply};

use crate::context::AppContext;

pub async fn prepare_restore(ctx: &AppContext, key: &str) -> Result<UiReply> {
    let entry = removed_entry(ctx, key)
        .await?
        .context("removed history entry is no longer available")?;
    Ok(UiReply {
        modal: Some(UiModal::Confirm {
            action: ConfirmAction::RestoreRemoved {
                key: entry.slug.clone(),
                removed_at: entry.removed_at.clone(),
                branch: entry.branch.clone(),
            },
            title: format!("Restore {}?", entry.slug),
            lines: vec![
                format!(
                    "Create a worktree for {}",
                    wt_core::sanitize_terminal_text(&entry.branch)
                ),
                "The current local/origin branch is used when it still exists; otherwise it is recreated from the configured base.".into(),
            ],
            cancel_key: None,
        }),
        ..UiReply::default()
    })
}

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
        .await?;
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

async fn removed_entry(ctx: &AppContext, key: &str) -> Result<Option<RemovedWorktree>> {
    let slug = local_slug(key)?;
    ctx.database
        .call(move |store| {
            Ok(store
                .read_removed_worktrees()?
                .into_iter()
                .find(|entry| entry.slug == slug))
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use wt_store::RemovedWorktree;

    #[tokio::test]
    async fn restore_confirmation_captures_ledger_identity_and_pause_toggle_refreshes_record() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let entry = RemovedWorktree {
            slug: "archived".into(),
            branch: "feature/archived".into(),
            removed_at: "2026-10-09T00:00:00Z".into(),
            work: None,
            automations_paused: None,
            extra: Map::from_iter([("futureField".into(), serde_json::json!("kept"))]),
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

        let prepared = prepare_restore(&fixture.ctx, "archived").await.unwrap();
        let Some(UiModal::Confirm { action, .. }) = prepared.modal else {
            panic!("restore must request confirmation");
        };
        assert_eq!(
            action,
            ConfirmAction::RestoreRemoved {
                key: "archived".into(),
                removed_at: entry.removed_at.clone(),
                branch: entry.branch.clone(),
            }
        );

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
