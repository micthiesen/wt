use std::{collections::BTreeSet, io::IsTerminal, time::Duration};

use anyhow::{Context, Result};
use clap::Args;
use wt_config::SstConfig;
use wt_platform::process::CommandSpec;
use wt_sst::{SstService, SstStage, StageInventory, human_size};

use crate::context::AppContext;

#[derive(Debug, Clone, Args, Default)]
pub struct StagesArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub clean: bool,
    #[arg(short = 'y', long)]
    pub yes: bool,
}

pub async fn run(ctx: &AppContext, args: &StagesArgs) -> Result<i32> {
    let Some(sst) = ctx.config.sst.clone() else {
        eprintln!("SST is not configured. Add [deploy.sst] to your wt configuration.");
        return Ok(1);
    };
    let service = service(ctx, sst);
    let stages = service
        .list(&ctx.cancellation)
        .await
        .context("list SST stages")?;
    let worktrees = ctx
        .repository
        .inventory(&ctx.cancellation)
        .await
        .context("read worktree inventory")?;
    let live_names = live_stage_names(&worktrees);
    let inventory = service
        .categorize(stages, &live_names, &ctx.cancellation)
        .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&inventory)?);
    } else {
        print_inventory(&inventory);
    }
    if !args.clean {
        return Ok(0);
    }
    if inventory.orphaned.is_empty() {
        if !inventory.unknown.is_empty() {
            emit(
                args.json,
                format!(
                    "No safe cleanup candidates; {} stage state file(s) could not be verified.",
                    inventory.unknown.len()
                ),
            );
            return Ok(1);
        }
        emit(args.json, "No orphaned SST stages to clean.".into());
        return Ok(0);
    }
    if !args.yes {
        if !std::io::stdin().is_terminal() {
            eprintln!("Use -y with --clean in non-interactive mode.");
            return Ok(2);
        }
        if !crate::prompt::confirm(
            &format!(
                "Remove {} orphaned SST stage(s)? [y/N] ",
                inventory.orphaned.len()
            ),
            false,
            &ctx.cancellation,
        )
        .await?
        {
            return Ok(0);
        }
    }

    let mut failed = !inventory.unknown.is_empty();
    for stage in &inventory.orphaned {
        // Inventory can change while a human is confirming or while earlier
        // stages are being removed. Re-read it directly before each mutation.
        let current = ctx
            .repository
            .inventory(&ctx.cancellation)
            .await
            .with_context(|| {
                format!(
                    "revalidate worktree inventory before removing stage {}",
                    stage.name
                )
            })?;
        if live_stage_names(&current).contains(&stage.name) {
            failed = true;
            emit(
                args.json,
                format!(
                    "Skipping {}: a live worktree now uses this stage.",
                    stage.name
                ),
            );
            continue;
        }
        if !wt_sst::stage_name_in_namespace(&stage.name, &ctx.config.stage.prefix)
            || stage.name == ctx.config.stage.default_personal
        {
            failed = true;
            emit(
                args.json,
                format!(
                    "Skipping {}: it is outside the removable stage namespace.",
                    stage.name
                ),
            );
            continue;
        }
        let result = remove_stage(ctx, stage).await;
        match result {
            Ok(()) => emit(args.json, format!("✓ removed SST stage {}", stage.name)),
            Err(error) => {
                failed = true;
                emit(
                    args.json,
                    format!("✗ failed to remove SST stage {}: {error:#}", stage.name),
                );
            }
        }
    }
    Ok(if failed { 1 } else { 0 })
}

fn emit(json_output: bool, message: String) {
    if json_output {
        eprintln!("{message}");
    } else {
        println!("{message}");
    }
}

fn service(ctx: &AppContext, sst: SstConfig) -> SstService {
    SstService::new(sst, ctx.config.stage.clone(), ctx.processes.clone())
}

fn live_stage_names(worktrees: &[wt_vcs::WorktreeRecord]) -> BTreeSet<String> {
    worktrees
        .iter()
        .filter(|worktree| !worktree.is_main)
        .map(|worktree| worktree.target.stage.clone())
        .filter(|stage| !stage.is_empty())
        .collect()
}

async fn remove_stage(ctx: &AppContext, stage: &SstStage) -> Result<()> {
    let mut spec = CommandSpec::new("pnpm");
    spec.args = ["sst", "remove", "--stage", &stage.name]
        .into_iter()
        .map(Into::into)
        .collect();
    spec.cwd = Some(ctx.config.paths.main_clone.clone());
    spec.timeout = Duration::from_secs(20 * 60);
    spec.output_limit = 8 * 1024 * 1024;
    ctx.processes
        .run(spec, &ctx.cancellation)
        .await
        .context("run pnpm sst remove")?
        .checked("pnpm sst remove")
        .context("destroy SST stage")?;
    Ok(())
}

fn print_inventory(inventory: &StageInventory) {
    println!("SST stages");
    print_group("live", &inventory.live);
    print_group("orphaned", &inventory.orphaned);
    if !inventory.unknown.is_empty() {
        println!("\nunknown (state could not be verified; never cleaned automatically):");
        for unknown in &inventory.unknown {
            println!(
                "  {:<34} {:>10}  {}",
                unknown.stage.name,
                human_size(unknown.stage.size_bytes),
                unknown.reason
            );
        }
    }
    println!(
        "\n{} live, {} orphaned, {} unknown",
        inventory.live.len(),
        inventory.orphaned.len(),
        inventory.unknown.len()
    );
}

fn print_group(label: &str, stages: &[SstStage]) {
    println!("\n{label}:");
    if stages.is_empty() {
        println!("  none");
        return;
    }
    for stage in stages {
        println!(
            "  {:<34} {:>10}  {}",
            stage.name,
            human_size(stage.size_bytes),
            stage.modified
        );
    }
}

#[cfg(test)]
mod tests {
    use super::live_stage_names;
    use wt_core::local_worktree_target;
    use wt_vcs::{RepositoryKind, WorktreeRecord};

    #[test]
    fn live_inventory_excludes_main_and_retains_nonempty_stages() {
        let record = |slug: &str, stage: &str, is_main| WorktreeRecord {
            target: local_worktree_target(slug, slug, format!("/tmp/{slug}"), stage),
            is_main,
            head_sha: None,
            detached: false,
            kind: RepositoryKind::LinkedWorktree,
            locked: false,
            prunable: false,
            git_dir: None,
            common_dir: None,
        };
        assert_eq!(
            live_stage_names(&[
                record("main", "", true),
                record("one", "m-one", false),
                record("two", "m-two", false),
            ]),
            ["m-one".into(), "m-two".into()].into()
        );
    }
}
