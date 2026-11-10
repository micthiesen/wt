use anyhow::Result;
use clap::Args;
use wt_core::{WorktreeRef, parse_worktree_ledger_key, worktree_target_key};

use wt_app_core::context::AppContext;
use wt_ops::commands::resolve::resolve_named_worktree;

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
        if super::archive::set_archived(ctx, args.target.clone(), false).await? {
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
    match super::archive::set_archived(ctx, worktree_target_key(&target), false).await {
        Ok(true) => println!("✓ restored {}", target.slug()),
        Ok(false) => println!("{} is already active", target.slug()),
        Err(error) => {
            eprintln!("{error:#}");
            return Ok(1);
        }
    }
    Ok(0)
}
