use std::io::IsTerminal;
use std::path::Path;

use anyhow::Result;
use clap::Args;
use wt_lifecycle::{LifecycleService, RemoveOptions, ServiceConfig};

use crate::{commands::resolve::resolve_named_worktree, context::AppContext};

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

    let mut destroy_stage = args.destroy_stage;
    if args.no_destroy_stage {
        destroy_stage = false;
    } else if !args.destroy_stage
        && is_our_stage_deployed(
            Path::new(&record.target.path),
            &record.target.stage,
            &ctx.config.stage.prefix,
        )
    {
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
                landed: false,
                destroy_stage,
                expected_revision: None,
            },
        )
        .await?;
        println!("✓ removal queued for {} (job {job})", record.target.slug());
        return Ok(0);
    }
    let service = LifecycleService::new(
        ServiceConfig::from_config(&ctx.config),
        (*ctx.repository).clone(),
        ctx.processes.clone(),
    );
    let result = service
        .remove(
            &record.target,
            RemoveOptions {
                force: args.force,
                delete_branch: !args.keep_branch,
                landed: false,
                destroy_stage,
            },
            &ctx.cancellation,
        )
        .await;
    match result {
        Ok(result) => {
            println!("✓ removed {}", record.target.slug());
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

fn safe_stage(path: &Path, prefix: &str) -> Result<String, String> {
    if prefix.is_empty() {
        return Err("personal stage prefix is not configured".into());
    }
    let pin = std::fs::read_to_string(path.join(".sst/stage"))
        .map_err(|_| "no .sst/stage pinned".to_owned())?;
    let stage = pin.trim();
    if !stage.starts_with(prefix) {
        return Err(format!(
            "pinned stage {stage:?} does not carry personal prefix {prefix:?}"
        ));
    }
    Ok(stage.to_owned())
}

pub(crate) fn is_our_stage_deployed(path: &Path, stage: &str, prefix: &str) -> bool {
    safe_stage(path, prefix).is_ok_and(|pinned| pinned == stage)
        && std::fs::read_to_string(path.join(".sst/outputs.json"))
            .is_ok_and(|contents| contents.contains(stage))
}

#[cfg(test)]
mod tests {
    use super::{is_our_stage_deployed, safe_stage};

    #[test]
    fn stage_safety_requires_owned_pin_and_matching_outputs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".sst")).unwrap();
        std::fs::write(dir.path().join(".sst/stage"), "personal-eng-4-a1b2\n").unwrap();
        assert_eq!(
            safe_stage(dir.path(), "personal-").unwrap(),
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
    }
}
