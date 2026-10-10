use std::{collections::BTreeSet, io::IsTerminal, path::PathBuf};

use anyhow::Result;
use clap::Args;
use wt_lifecycle::RemoveOptions;

use crate::{commands::resolve::run_git, context::AppContext, lifecycle_ops};

#[derive(Debug, Clone, Args, Default)]
pub struct CleanupArgs {
    #[arg(short = 'y', long)]
    pub yes: bool,
    #[arg(long, conflicts_with = "no_destroy_stage")]
    pub destroy_stage: bool,
    #[arg(long, conflicts_with = "destroy_stage")]
    pub no_destroy_stage: bool,
    #[arg(long)]
    pub foreground: bool,
}

pub async fn run(ctx: &AppContext, args: &CleanupArgs) -> Result<i32> {
    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    crate::origin::refresh(ctx, &ctx.cancellation).await?;
    // Linked worktrees share refs; Rift clones must each refresh their own refs.
    let mut fetch_roots = BTreeSet::new();
    for row in &rows {
        if row.kind == wt_vcs::RepositoryKind::RiftClone {
            fetch_roots.insert(PathBuf::from(&row.target.path));
        }
    }
    for root in fetch_roots {
        run_git(ctx, root, ["fetch", "--prune", "origin"])
            .await?
            .checked("git fetch")?;
    }
    let plans = lifecycle_ops::plan(ctx, rows).await?;
    if let Some(warning) = plans.warning {
        eprintln!("warning: {warning}");
    }
    let mut candidates = Vec::new();
    for mut plan in plans.rows {
        if !plan.landed {
            println!(
                "Keeping {}: landing of its own work is not proven",
                plan.row.target.slug()
            );
            continue;
        }
        if !plan.hazards.is_empty() {
            println!(
                "Keeping {}: {}",
                plan.row.target.slug(),
                plan.hazards.join(", ")
            );
            continue;
        }
        plan.destroy_stage = !args.no_destroy_stage && (args.destroy_stage || plan.destroy_stage);
        candidates.push(plan);
    }
    if candidates.is_empty() {
        println!("Nothing to clean.");
        return Ok(0);
    }
    println!("Cleanup candidates:");
    for plan in &candidates {
        println!(
            "  {}  merged  {}",
            plan.row.target.slug(),
            plan.row.target.branch
        );
    }
    if !args.yes {
        if !std::io::stdin().is_terminal() {
            eprintln!("Confirming clean requires a TTY. Pass -y.");
            return Ok(2);
        }
        if !crate::prompt::confirm(
            &format!("Remove {}? [Y/n] ", candidates.len()),
            true,
            &ctx.cancellation,
        )
        .await?
        {
            return Ok(0);
        }
    }
    let mut failed = false;
    if !args.foreground {
        let requests: Vec<_> = candidates
            .into_iter()
            .map(|plan| {
                (
                    plan.row,
                    super::_destroy::DestroyOptions {
                        force: false,
                        delete_branch: true,
                        landed: true,
                        destroy_stage: plan.destroy_stage,
                        expected_revision: Some(plan.revision),
                        removed_snapshot: Some(plan.removed_snapshot),
                    },
                )
            })
            .collect();
        for ((row, _), result) in requests
            .iter()
            .zip(super::_destroy::start_removals(ctx, &requests).await)
        {
            match result {
                Ok(job) => println!("✓ cleanup queued for {} (job {job})", row.target.slug()),
                Err(error) => {
                    failed = true;
                    eprintln!("{}: {error:#}", row.target.slug());
                }
            }
        }
    } else {
        let service = lifecycle_ops::service(ctx)?;
        for plan in candidates {
            let dev_port = crate::host_cleanup::stored_dev_port(ctx, plan.row.target.slug()).await;
            let result = service
                .remove_with_revision(
                    &plan.row.target,
                    RemoveOptions {
                        force: false,
                        delete_branch: true,
                        landed: true,
                        destroy_stage: plan.destroy_stage,
                        removed_snapshot: Some(plan.removed_snapshot),
                    },
                    &plan.revision,
                    &ctx.cancellation,
                )
                .await;
            match result {
                Ok(result) => {
                    println!("✓ removed {}", plan.row.target.slug());
                    if result.removed {
                        for warning in
                            crate::host_cleanup::after_remove(ctx, plan.row.target.slug(), dev_port)
                                .await
                        {
                            eprintln!("warning: {warning}");
                        }
                    }
                    if result.destroyed_stage {
                        println!("✓ destroyed stage {}", plan.row.target.stage);
                    }
                    for warning in result.warnings {
                        eprintln!("warning: {warning}");
                    }
                }
                Err(error) => {
                    failed = true;
                    eprintln!("{}: {error}", plan.row.target.slug());
                }
            }
        }
    }
    Ok(i32::from(failed))
}

#[cfg(test)]
mod tests {
    use super::CleanupArgs;
    use clap::Args;

    #[test]
    fn clean_rejects_force_and_conflicting_stage_flags() {
        let command = || CleanupArgs::augment_args(clap::Command::new("clean"));
        assert!(
            command()
                .try_get_matches_from(["clean", "--force"])
                .is_err()
        );
        assert!(
            command()
                .try_get_matches_from(["clean", "--destroy-stage", "--no-destroy-stage"])
                .is_err()
        );
        assert!(
            command()
                .try_get_matches_from(["clean", "--yes", "--foreground"])
                .is_ok()
        );
    }
}
