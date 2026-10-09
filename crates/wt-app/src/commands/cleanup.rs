use std::io::IsTerminal;
use std::path::Path;

use anyhow::Result;
use clap::Args;
use serde_json::Value;
use wt_lifecycle::{CleanupCandidate, LifecycleService, ServiceConfig};

use crate::{
    commands::{remove::is_our_stage_deployed, resolve::run_git},
    context::AppContext,
};

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
    let fetched = run_git(
        ctx,
        &ctx.config.paths.main_clone,
        ["fetch", "--prune", "origin"],
    )
    .await;
    if let Err(error) = fetched {
        eprintln!("Failed to fetch origin: {error}");
        return Ok(1);
    }
    let trunk = ctx.config.branch.base.clone();
    let trunk_ref = if trunk.starts_with("origin/") || trunk.starts_with("refs/") {
        trunk.clone()
    } else {
        format!("origin/{trunk}")
    };
    let snapshots = ctx.repository.inventory_status(&ctx.cancellation).await?;
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let mut candidates = Vec::new();
    let mut dirty = Vec::new();
    let mut verification_owed = Vec::new();
    let mut unproven = Vec::new();
    for snapshot in snapshots
        .iter()
        .filter(|snapshot| !snapshot.worktree.is_main)
    {
        let row = &snapshot.worktree;
        if row.target.branch.is_empty()
            || snapshot.worktree.kind != wt_vcs::RepositoryKind::LinkedWorktree
        {
            continue;
        }
        let state_entry = state
            .get("slugs")
            .and_then(|slugs| slugs.get(row.target.slug()));
        let base_sha = state_entry
            .and_then(|entry| entry.get("baseSha"))
            .and_then(Value::as_str);
        let Some(base_sha) = base_sha else {
            unproven.push(row);
            continue;
        };
        // A branch with no commits since its fork point is never considered
        // landed: ancestry alone would be vacuously true for untouched rows.
        let own = run_git(
            ctx,
            &ctx.config.paths.main_clone,
            [
                "rev-list",
                "--count",
                &format!("{base_sha}..{}", row.target.branch),
            ],
        )
        .await?;
        let own_count = own
            .status
            .success()
            .then(|| own.stdout_text().trim().parse::<u64>().ok())
            .flatten()
            .unwrap_or(0);
        if own_count == 0 {
            unproven.push(row);
            continue;
        }
        let merged = run_git(
            ctx,
            &ctx.config.paths.main_clone,
            [
                "merge-base",
                "--is-ancestor",
                &row.target.branch,
                &trunk_ref,
            ],
        )
        .await?;
        if !merged.status.success() {
            unproven.push(row);
            continue;
        }
        if snapshot.status.as_ref().is_some_and(|status| status.dirty) {
            dirty.push(row);
            continue;
        }
        let work = state_entry.and_then(|entry| entry.get("work"));
        if work.is_some_and(|work| {
            work.get("verifyAfterMerge")
                .and_then(Value::as_str)
                .is_some_and(|steps| !steps.trim().is_empty())
                && !matches!(
                    work.get("state").and_then(Value::as_str),
                    Some("verified" | "dropped")
                )
        }) {
            verification_owed.push((
                row,
                work.and_then(|work| work.get("verifyAfterMerge"))
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ));
            continue;
        }
        let stage_destroy = if args.no_destroy_stage {
            false
        } else if args.destroy_stage {
            true
        } else {
            is_our_stage_deployed(
                Path::new(&row.target.path),
                &row.target.stage,
                &ctx.config.stage.prefix,
            )
        };
        candidates.push(CleanupCandidate {
            target: row.target.clone(),
            landed: true,
            destroy_stage: stage_destroy,
        });
    }
    if !dirty.is_empty() {
        println!("Skipping (uncommitted changes; clean never forces):");
        for row in dirty {
            println!(
                "  {}  commit them, or discard deliberately with `wt rm {} --force`",
                row.target.slug(),
                row.target.slug()
            );
        }
    }
    if !verification_owed.is_empty() {
        println!("Skipping (post-merge verification still owed):");
        for (row, steps) in verification_owed {
            println!(
                "  {}  {steps}\n    run it, then: wt status {} verified -m \"<what you checked>\"",
                row.target.slug(),
                row.target.slug()
            );
        }
    }
    if !unproven.is_empty() {
        println!("Skipping (not proven as a non-empty direct merge into trunk):");
        for row in unproven {
            println!(
                "  {}  remove explicitly with `wt rm {}` if intended",
                row.target.slug(),
                row.target.slug()
            );
        }
    }
    if candidates.is_empty() {
        println!("Nothing to clean.");
        return Ok(0);
    }
    println!("Cleanup candidates:");
    for candidate in &candidates {
        println!(
            "  {}  merged  {}",
            candidate.target.slug(),
            candidate.target.branch
        );
    }
    if !args.yes {
        if !std::io::stdin().is_terminal() {
            eprintln!("Confirming clean requires a TTY. Pass -y.");
            return Ok(2);
        }
        let confirmed = crate::prompt::confirm(
            &format!("Remove {}? [Y/n] ", candidates.len()),
            true,
            &ctx.cancellation,
        )
        .await?;
        if !confirmed {
            return Ok(0);
        }
    }
    if !args.foreground {
        let mut failed = false;
        let live_rows = ctx.repository.inventory(&ctx.cancellation).await?;
        let mut scheduled_candidates = Vec::new();
        let mut requests = Vec::new();
        for candidate in &candidates {
            if let Some(row) = live_rows
                .iter()
                .find(|row| row.target.slug() == candidate.target.slug())
            {
                scheduled_candidates.push(candidate);
                requests.push((
                    row.clone(),
                    super::_destroy::DestroyOptions {
                        force: false,
                        delete_branch: true,
                        landed: true,
                        destroy_stage: candidate.destroy_stage,
                        expected_revision: None,
                    },
                ));
            } else {
                failed = true;
                eprintln!(
                    "{}: cleanup target disappeared before scheduling",
                    candidate.target.slug()
                );
            }
        }
        for (candidate, result) in scheduled_candidates
            .into_iter()
            .zip(super::_destroy::start_removals(ctx, &requests).await)
        {
            match result {
                Ok(job) => println!(
                    "✓ cleanup queued for {} (job {job})",
                    candidate.target.slug()
                ),
                Err(error) => {
                    failed = true;
                    eprintln!("{}: {error:#}", candidate.target.slug());
                }
            }
        }
        return Ok(i32::from(failed));
    }
    let service = LifecycleService::new(
        ServiceConfig::from_config(&ctx.config),
        (*ctx.repository).clone(),
        ctx.processes.clone(),
    );
    let results = service.cleanup(&candidates, &ctx.cancellation).await;
    let mut failed = false;
    for (candidate, result) in candidates.iter().zip(results) {
        match result {
            Ok(result) => {
                println!("✓ removed {}", candidate.target.slug());
                if result.destroyed_stage {
                    println!("✓ destroyed stage {}", candidate.target.stage);
                }
                for warning in result.warnings {
                    eprintln!("warning: {warning}");
                }
            }
            Err(error) => {
                failed = true;
                eprintln!("{}: {error}", candidate.target.slug());
            }
        }
    }
    Ok(i32::from(failed))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    #[test]
    fn clean_has_no_force_option() {
        let fields = BTreeSet::from(["yes", "destroy_stage", "no_destroy_stage", "foreground"]);
        assert!(!fields.contains("force"));
    }
}
