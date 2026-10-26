use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;
use wt_lifecycle::RemoveOptions;

use crate::{commands::resolve::resolve_named_worktree, context::AppContext, lifecycle_ops};

#[derive(Debug, Clone, Args, Default)]
pub struct RemoveArgs {
    #[arg(value_name = "SLUG")]
    pub slug: Option<String>,
    #[arg(short = 'y', long)]
    pub yes: bool,
    #[arg(long)]
    pub force: bool,
    #[arg(long, conflicts_with = "no_destroy_stage")]
    pub destroy_stage: bool,
    #[arg(long, conflicts_with = "destroy_stage")]
    pub no_destroy_stage: bool,
    #[arg(long, conflicts_with = "keep_branch")]
    pub delete_branch: bool,
    #[arg(long, conflicts_with = "delete_branch")]
    pub keep_branch: bool,
    #[arg(short = 'b', long)]
    pub background: bool,
}

pub async fn run(ctx: &AppContext, args: &RemoveArgs) -> Result<i32> {
    let record = if let Some(slug) = &args.slug {
        match resolve_named_worktree(ctx, slug).await {
            Ok(record) => record,
            Err(error) => {
                eprintln!("{error}");
                return Ok(1);
            }
        }
    } else {
        let rows = ctx
            .repository
            .inventory(&ctx.cancellation)
            .await?
            .into_iter()
            .filter(|record| !record.is_main)
            .collect::<Vec<_>>();
        if rows.is_empty() {
            println!("No worktrees to remove.");
            return Ok(0);
        }
        if !std::io::stdin().is_terminal() {
            eprintln!("Picking a worktree requires a TTY.");
            return Ok(2);
        }
        let choices = rows
            .iter()
            .map(|row| format!("{}  {}", row.target.slug(), row.target.branch))
            .collect::<Vec<_>>();
        let Some(index) =
            crate::prompt::pick(&choices, "Remove which worktree?", &ctx.cancellation).await?
        else {
            return Ok(0);
        };
        rows[index].clone()
    };
    if record.is_main {
        eprintln!("cannot remove the configured main clone");
        return Ok(1);
    }

    let plans = lifecycle_ops::plan(ctx, vec![record.clone()]).await?;
    if let Some(warning) = plans.warning {
        eprintln!("warning: {warning}");
    }
    let plan = plans
        .rows
        .into_iter()
        .next()
        .context("worktree disappeared")?;

    let mut destroy_stage = args.destroy_stage;
    if args.no_destroy_stage {
        destroy_stage = false;
    } else if !args.destroy_stage && plan.destroy_stage {
        if args.yes {
            destroy_stage = true;
        } else if std::io::stdin().is_terminal() {
            destroy_stage = crate::prompt::confirm(
                &format!(
                    "Stage {} looks deployed. Run `pnpm sst remove`? [Y/n] ",
                    record.target.stage
                ),
                true,
                &ctx.cancellation,
            )
            .await?;
        } else {
            println!(
                "Skipping sst remove for {} (non-interactive; pass --destroy-stage to run it)",
                record.target.stage
            );
        }
    }
    if args.background {
        let job = super::_destroy::start_remove(
            ctx,
            &record,
            super::_destroy::DestroyOptions {
                force: args.force,
                delete_branch: !args.keep_branch,
                landed: plan.landed,
                destroy_stage,
                expected_revision: Some(plan.revision),
                removed_snapshot: Some(plan.removed_snapshot.clone()),
            },
        )
        .await?;
        println!("✓ removal queued for {} (job {job})", record.target.slug());
        return Ok(0);
    }
    let service = lifecycle_ops::service(ctx)?;
    let dev_port = crate::host_cleanup::stored_dev_port(ctx, record.target.slug()).await;
    let result = service
        .remove_with_revision(
            &record.target,
            RemoveOptions {
                force: args.force,
                delete_branch: !args.keep_branch,
                landed: plan.landed,
                destroy_stage,
                removed_snapshot: Some(plan.removed_snapshot),
            },
            &plan.revision,
            &ctx.cancellation,
        )
        .await;
    match result {
        Ok(result) => {
            println!("✓ removed {}", record.target.slug());
            if result.removed {
                for warning in
                    crate::host_cleanup::after_remove(ctx, record.target.slug(), dev_port).await
                {
                    eprintln!("warning: {warning}");
                }
            }
            if result.destroyed_stage {
                println!("✓ destroyed stage {}", record.target.stage);
            }
            if result.deleted_branch {
                println!("✓ deleted branch {}", record.target.branch);
            }
            for warning in result.warnings {
                eprintln!("warning: {warning}");
            }
            Ok(0)
        }
        Err(error) => {
            eprintln!("Failed: {error}");
            if !args.force {
                eprintln!(
                    "  retry with `wt rm {} --force` only if discarding work is intended",
                    record.target.slug()
                );
            }
            Ok(1)
        }
    }
}

pub(crate) fn is_our_stage_deployed(path: &Path, stage: &str, prefix: &str) -> bool {
    matches!(wt_sst::observe_local_deployment(path, prefix),
        wt_sst::DeploymentObservation::Deployed {stage: pinned} if pinned == stage)
}

#[cfg(test)]
mod tests {
    use super::is_our_stage_deployed;
    use wt_sst::safe_pinned_stage;

    #[test]
    fn stage_safety_requires_owned_pin_and_matching_outputs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".sst")).unwrap();
        std::fs::write(dir.path().join(".sst/stage"), "personal-eng-4-a1b2\n").unwrap();
        assert_eq!(
            safe_pinned_stage(dir.path(), "personal-").unwrap(),
            "personal-eng-4-a1b2"
        );
        std::fs::write(
            dir.path().join(".sst/outputs.json"),
            r#"{"url":"https://personal-eng-4-a1b2.example"}"#,
        )
        .unwrap();
        assert!(is_our_stage_deployed(
            dir.path(),
            "personal-eng-4-a1b2",
            "personal-"
        ));
        assert!(!is_our_stage_deployed(
            dir.path(),
            "production",
            "personal-"
        ));
        std::fs::write(
            dir.path().join(".sst/outputs.json"),
            "broken personal-eng-4-a1b2",
        )
        .unwrap();
        assert!(!is_our_stage_deployed(
            dir.path(),
            "personal-eng-4-a1b2",
            "personal-"
        ));
    }
}
