use anyhow::Result;
use clap::Args;
use wt_core::{WorktreeRef, parse_worktree_ledger_key, worktree_target_key};
use wt_lifecycle::{LifecycleService, ServiceConfig};

use crate::{commands::resolve::resolve_named_worktree, context::AppContext};

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
        let key = args.target.clone();
        let changed = ctx
            .database
            .call(move |store| Ok(store.set_archived(&key, true)?))
            .await?;
        if changed {
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
    let service = LifecycleService::new(
        ServiceConfig::from_config(&ctx.config),
        (*ctx.repository).clone(),
        ctx.processes.clone(),
    );
    let key = worktree_target_key(&target);
    if service.archive(&key, true, &ctx.cancellation).await? {
        println!("✓ archived {}", target.slug());
    } else {
        println!("{} is already archived", target.slug());
    }
    Ok(0)
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

    #[test]
    fn configured_remote_identity_includes_config_when_ssh_host_is_shared() {
        let first = RemoteConfig {
            host: "devbox".into(),
            label: "one".into(),
            wt_path: "~/one".into(),
            config: Some("/etc/wt-one.toml".into()),
        };
        let second = RemoteConfig {
            host: "devbox".into(),
            label: "two".into(),
            wt_path: "~/two".into(),
            config: Some("/etc/wt-two.toml".into()),
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
