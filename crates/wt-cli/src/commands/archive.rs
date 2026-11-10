use anyhow::Result;
use clap::Args;
use wt_core::{WorktreeRef, parse_worktree_ledger_key, worktree_target_key};

use wt_app_core::context::AppContext;
use wt_ops::commands::resolve::resolve_named_worktree;

#[derive(Debug, Clone, Args)]
pub struct ArchiveArgs {
    #[arg(value_name = "SLUG_OR_BRANCH")]
    pub target: String,
}

pub async fn run(ctx: &AppContext, args: &ArchiveArgs) -> Result<i32> {
    if args.target.starts_with("@remote/") {
        let Some(WorktreeRef::Remote { host, slug }) = parse_worktree_ledger_key(&args.target)
        else {
            eprintln!("invalid remote worktree key {:?}", args.target);
            return Ok(2);
        };
        if configured_remote(&ctx.config.remotes, &host).is_none() {
            eprintln!(
                "remote host {host:?} is not configured; refusing to modify its local archive ledger"
            );
            return Ok(2);
        }
        if set_archived(ctx, args.target.clone(), true).await? {
            println!("✓ archived remote {host}/{slug}");
        } else {
            println!("remote {host}/{slug} is already archived");
        }
        return Ok(0);
    }
    let target = match resolve_named_worktree(ctx, &args.target).await {
        Ok(record) if !record.is_main => record.target,
        Ok(_) => {
            eprintln!("the configured main clone cannot be archived");
            return Ok(1);
        }
        Err(error) => {
            eprintln!("{error}");
            return Ok(1);
        }
    };
    match set_archived(ctx, worktree_target_key(&target), true).await {
        Ok(true) => println!("✓ archived {}", target.slug()),
        Ok(false) => println!("{} is already archived", target.slug()),
        Err(error) => {
            eprintln!("{error:#}");
            return Ok(1);
        }
    }
    Ok(0)
}

/// Archive or restore `key` the way the TUI's `a` does: refused while the
/// row's operation lock is held, with the marker, the Archived fold and the
/// restored row's return to the Inbox in one store transaction. Returns
/// `false` when the row is already in the requested state.
pub(crate) async fn set_archived(ctx: &AppContext, key: String, archived: bool) -> Result<bool> {
    if wt_core::is_remote_worktree_ledger_key(&key) {
        // The host's toggle needs a live board to vouch for a remote row; a
        // ledger key has no local operation lock to probe.
        return ctx
            .database
            .call(move |store| {
                Ok(matches!(
                    store.toggle_archived_seen(
                        &key,
                        Some(!archived),
                        wt_sources::board_layout::ARCHIVED,
                    )?,
                    wt_store::SeenToggle::Set(_)
                ))
            })
            .await;
    }
    let toggle = wt_tui::UiAction::ToggleArchive {
        key,
        seen: Some(wt_tui::Seen(!archived)),
    };
    match wt_host::controller_actions::execute(ctx, toggle, None, None).await {
        Ok(_) => Ok(true),
        Err(error) if wt_app_core::host_errors::error_reply(&error).stale => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn configured_remote<'a>(
    remotes: &'a [wt_config::RemoteConfig],
    key: &str,
) -> Option<&'a wt_config::RemoteConfig> {
    remotes.iter().find(|remote| remote.key() == key)
}

#[cfg(test)]
mod tests {
    use super::configured_remote;
    use wt_config::RemoteConfig;

    #[tokio::test]
    async fn archive_and_restore_match_the_tui_toggle() {
        use crate::commands::restore;
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        let row = wt_ops::commands::resolve::resolve_named_worktree(ctx, "one")
            .await
            .unwrap();
        let key = wt_core::worktree_target_key(&row.target);
        let filed = key.clone();
        ctx.database
            .call(move |store| Ok(store.move_worktrees_to_section(&[filed], Some("Batch"))?))
            .await
            .unwrap();
        let archived = || {
            let key = key.clone();
            let database = ctx.database.clone();
            async move {
                database
                    .call(move |store| Ok(store.read_archived_keys()?.contains(&key)))
                    .await
                    .unwrap()
            }
        };
        let archive = super::ArchiveArgs {
            target: "one".into(),
        };
        let restore = restore::RestoreArgs {
            target: "one".into(),
        };

        // A restack or removal holds the row's operation lock.
        let lock =
            wt_platform::lock::FileLock::try_acquire(&ctx.config.paths.lock_dir, "one", "restack")
                .await
                .unwrap()
                .unwrap();
        assert_eq!(super::run(ctx, &archive).await.unwrap(), 1);
        assert!(
            !archived().await,
            "archived a row with an operation running"
        );
        drop(lock);

        assert_eq!(super::run(ctx, &archive).await.unwrap(), 0);
        assert!(archived().await);
        assert!(
            ctx.database
                .call(|store| Ok(store.is_section_folded(wt_sources::board_layout::ARCHIVED)?))
                .await
                .unwrap(),
            "archiving folds the Archived section"
        );
        assert_eq!(super::run(ctx, &archive).await.unwrap(), 0);
        assert!(archived().await, "a repeated archive is not a toggle");

        assert_eq!(restore::run(ctx, &restore).await.unwrap(), 0);
        assert!(!archived().await);
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert!(
            state["slugs"][&key]["section"].is_null(),
            "a restored row returns to the Inbox: {}",
            state["slugs"][&key]
        );
        assert_eq!(restore::run(ctx, &restore).await.unwrap(), 0);
        assert!(!archived().await, "a repeated restore is not a toggle");
        fixture.close().await.unwrap();
    }

    #[test]
    fn configured_remote_identity_includes_config_when_ssh_host_is_shared() {
        let first = RemoteConfig {
            host: "devbox".into(),
            label: "one".into(),
            wt_path: "~/one".into(),
            config: Some("/etc/wt-one.toml".into()),
            ..Default::default()
        };
        let second = RemoteConfig {
            host: "devbox".into(),
            label: "two".into(),
            wt_path: "~/two".into(),
            config: Some("/etc/wt-two.toml".into()),
            ..Default::default()
        };
        let remotes = [first, second];
        let first_key = remotes[0].key();
        let second_key = remotes[1].key();
        assert_ne!(first_key, second_key);
        assert_eq!(
            configured_remote(&remotes, &first_key).unwrap().label,
            "one"
        );
        assert_eq!(
            configured_remote(&remotes, &second_key).unwrap().label,
            "two"
        );
        assert!(configured_remote(&remotes, "devbox").is_none());
    }
}
