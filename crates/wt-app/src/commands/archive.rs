use anyhow::Result;
use clap::Args;
use wt_core::worktree_target_key;
use wt_lifecycle::{LifecycleService, ServiceConfig};

use crate::{commands::resolve::resolve_named_worktree, context::AppContext};

#[derive(Debug, Clone, Args)]
pub struct ArchiveArgs {
    #[arg(value_name = "SLUG_OR_BRANCH")]
    pub target: String,
}

pub async fn run(ctx: &AppContext, args: &ArchiveArgs) -> Result<i32> {
    if args.target.starts_with("@remote/") {
        eprintln!(
            "{} is a remote ledger key; archive it through `wt remote <host> ...`",
            args.target
        );
        return Ok(2);
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
