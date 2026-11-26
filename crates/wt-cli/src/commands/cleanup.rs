use std::{collections::BTreeSet, io::IsTerminal, path::PathBuf};

use anyhow::Result;
use clap::Args;
use wt_lifecycle::RemoveOptions;

use wt_app_core::context::AppContext;
use wt_core::cleanup_review::KeptRow;
use wt_ops::{commands::resolve::run_git, lifecycle_ops};

#[derive(Debug, Clone, Args, Default)]
pub struct CleanupArgs {
    /// Skip confirmation (required when not interactive).
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Destroy the SST stage of every candidate (default: per worktree, iff its stage is live).
    #[arg(long, conflicts_with = "no_destroy_stage")]
    pub destroy_stage: bool,
    /// Keep the SST stage of every candidate.
    #[arg(long, conflicts_with = "destroy_stage")]
    pub no_destroy_stage: bool,
    /// Run removals synchronously (background is the default).
    #[arg(long)]
    pub foreground: bool,
    /// Dispatch removals as background jobs (the default).
    #[arg(long, conflicts_with = "foreground")]
    pub background: bool,
}

pub async fn run(ctx: &AppContext, args: &CleanupArgs) -> Result<i32> {
    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    wt_ops::origin::refresh(ctx, &ctx.cancellation).await?;
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
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let mut candidates = Vec::new();
    for mut plan in plans.rows {
        let slug = plan.row.target.slug();
        let stored = &state["slugs"][slug];
        let kept = KeptRow {
            landed: plan.landed,
            dropped: wt_core::parse_work_status(&stored["work"])
                .is_some_and(|work| work.state == wt_core::WorkState::Dropped),
            pr_state: plan
                .removed_snapshot
                .extra
                .get("prState")
                .and_then(serde_json::Value::as_str),
        };
        let base = stored["baseBranch"]
            .as_str()
            .unwrap_or(&ctx.config.branch.base);
        match keeping(slug, kept, &plan.hazards, base) {
            Kept::Listed(line) => println!("{line}"),
            Kept::Unlisted => {}
            Kept::No => {
                plan.destroy_stage =
                    !args.no_destroy_stage && (args.destroy_stage || plan.destroy_stage);
                candidates.push(plan);
            }
        }
    }
    if candidates.is_empty() {
        println!("Nothing is ready to clean.");
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
        if !wt_app_core::prompt::confirm(
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
    if args.background || !args.foreground {
        let requests: Vec<_> = candidates
            .into_iter()
            .map(|plan| {
                (
                    plan.row,
                    wt_ops::commands::_destroy::DestroyOptions {
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
            .zip(wt_ops::commands::_destroy::start_removals(ctx, &requests).await)
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
            let dev_port = wt_ops::host_cleanup::stored_dev_port(ctx, plan.row.target.slug()).await;
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
                        for warning in wt_ops::host_cleanup::after_remove(
                            ctx,
                            plan.row.target.slug(),
                            dev_port,
                        )
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

/// What the review does with one worktree.
#[derive(Debug, PartialEq, Eq)]
enum Kept {
    /// Kept, and a person could be surprised it is: say why.
    Listed(String),
    /// Kept because its work is unfinished or unstarted: not cleanup's
    /// business, so nothing is said.
    Unlisted,
    /// A candidate for removal.
    No,
}

/// The board's cleanup review lists the same rows for the same reasons
/// (`wt_core::cleanup_review`). Nothing here loosens what is removed: a row
/// is a candidate only when its work landed and no hazard holds it.
fn keeping(slug: &str, kept: KeptRow<'_>, hazards: &[String], base: &str) -> Kept {
    // Removing a row is refused while it owes something, so that is the
    // reason given: the advice to remove a branch that will never land must
    // work when followed.
    let owed = hazards
        .iter()
        .find(|hazard| hazard.starts_with(wt_core::OWED_HAZARD));
    let reason = if let (false, Some(owed)) = (kept.landed, owed) {
        owed.clone()
    } else if !kept.landed {
        kept.unlanded_reason(base, &format!("wt rm {slug}"))
    } else if hazards.is_empty() {
        return Kept::No;
    } else {
        hazards.join(", ")
    };
    if kept.surprising() {
        Kept::Listed(format!("Keeping {slug}: {reason}"))
    } else {
        Kept::Unlisted
    }
}

#[cfg(test)]
mod tests {
    use super::{CleanupArgs, Kept, KeptRow, keeping};
    use clap::Args;

    #[test]
    fn the_review_lists_only_kept_rows_a_person_expects_gone() {
        let row = |landed, dropped, pr_state| KeptRow {
            landed,
            dropped,
            pr_state,
        };
        let held = ["still owes: check: open /health (in staging)".to_owned()];
        // Unfinished or unstarted work is kept without a word, hazards or not.
        assert_eq!(
            keeping("one", row(false, false, None), &[], "main"),
            Kept::Unlisted
        );
        assert_eq!(
            keeping("one", row(false, false, Some("OPEN")), &held, "main"),
            Kept::Unlisted
        );
        // Landed: removed when nothing holds it, listed with why otherwise.
        assert_eq!(
            keeping("one", row(true, false, None), &[], "main"),
            Kept::No
        );
        assert_eq!(
            keeping("one", row(true, false, Some("MERGED")), &held, "main"),
            Kept::Listed("Keeping one: still owes: check: open /health (in staging)".into())
        );
        // A branch that will never land is never a candidate; the line says
        // how to remove it on purpose.
        assert_eq!(
            keeping("one", row(false, true, None), &[], "main"),
            Kept::Listed(
                "Keeping one: dropped, but its commits are not on main: remove it with wt rm one"
                    .into()
            )
        );
        assert_eq!(
            keeping("one", row(false, false, Some("CLOSED")), &[], "develop"),
            Kept::Listed(
                "Keeping one: its PR was closed unmerged, and its commits are not on develop: remove it with wt rm one"
                    .into()
            )
        );
        // While it still owes something `wt rm` would be refused: that is
        // what holds it, and what the line says.
        assert_eq!(
            keeping("one", row(false, true, None), &held, "main"),
            Kept::Listed("Keeping one: still owes: check: open /health (in staging)".into())
        );
        // Commits made since a merged pull request may still land.
        assert_eq!(
            keeping("one", row(false, false, Some("MERGED")), &[], "main"),
            Kept::Listed("Keeping one: landing of its own work is not proven".into())
        );
    }

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

    #[test]
    fn clean_accepts_background_alias_and_rejects_conflicting_mode() {
        use clap::Parser;

        #[derive(Parser)]
        #[command(name = "wt")]
        struct Cli {
            #[command(subcommand)]
            command: TestCommand,
        }
        #[derive(clap::Subcommand)]
        enum TestCommand {
            #[command(name = "clean")]
            Cleanup(CleanupArgs),
        }

        let parsed = Cli::try_parse_from(["wt", "clean", "--background"]).unwrap();
        let TestCommand::Cleanup(args) = parsed.command;
        assert!(args.background);
        assert!(!args.foreground);
        assert!(Cli::try_parse_from(["wt", "clean", "--background", "--foreground"]).is_err());
        let parsed = Cli::try_parse_from(["wt", "clean"]).unwrap();
        let TestCommand::Cleanup(args) = parsed.command;
        assert!(!args.background && !args.foreground);
    }
}
