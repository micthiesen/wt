use anyhow::Result;
use clap::Args;
use wt_core::{WorktreeRef, parse_worktree_ledger_key, worktree_target_key};
use wt_lifecycle::{LifecycleService, ServiceConfig};

use crate::{commands::resolve::resolve_named_worktree, context::AppContext};

#[derive(Debug, Clone, Args)]
pub struct RestoreArgs {
    #[arg(value_name = "SLUG_OR_BRANCH")]
    pub target: String,
}

pub async fn run(ctx: &AppContext, args: &RestoreArgs) -> Result<i32> {
    if args.target.starts_with("@remote/") {
        let Some(WorktreeRef::Remote { host, slug }) = parse_worktree_ledger_key(&args.target)
        else {
            eprintln!("invalid remote worktree key {:?}", args.target);
            return Ok(2);
        };
        if super::archive::configured_remote(&ctx.config.remotes, &host).is_none() {
            eprintln!(
                "remote host {host:?} is not configured; refusing to modify its local archive ledger"
            );
            return Ok(2);
        }
        let key = args.target.clone();
        let changed = ctx
            .database
            .call(move |store| Ok(store.set_archived(&key, false)?))
            .await?;
        if changed {
            println!("✓ restored remote {host}/{slug}");
        } else {
            println!("remote {host}/{slug} is already active");
        }
        return Ok(0);
    }
    let target = match resolve_named_worktree(ctx, &args.target).await {
        Ok(record) if !record.is_main => record.target,
        Ok(_) => {
            eprintln!("the configured main clone cannot be restored");
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
    if service.restore(&key, &ctx.cancellation).await? {
        println!("✓ restored {}", target.slug());
    } else {
        println!("{} is already active", target.slug());
    }
    Ok(0)
}
