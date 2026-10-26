use anyhow::Result;
use clap::Args;
use wt_github::{GithubClient, GithubOptions};
use wt_platform::process::CommandSpec;
use wt_stack::{
    RestackOptions, RestackOutcome, StackConfig, StackEvent, StackService, StateConfig,
};
use wt_store::RepositoryIdentity;

use crate::context::AppContext;

#[derive(Debug, Clone, Args, Default)]
pub struct RestackArgs {
    /// Worktree branch; defaults to the branch checked out in the current directory.
    #[arg(value_name = "BRANCH")]
    pub branch: Option<String>,
    /// Rebase the selected stack onto a different target ref.
    #[arg(long)]
    pub onto: Option<String>,
    /// Keep backups newer than this many days (`0` removes all recognized backups).
    #[arg(long)]
    pub days: Option<u64>,
}

pub async fn run(ctx: &AppContext, args: &RestackArgs) -> Result<i32> {
    let config = StackConfig {
        main_clone: ctx.config.paths.main_clone.clone(),
        lock_dir: ctx.config.paths.lock_dir.clone(),
        trunk_branch: ctx.config.branch.base.clone(),
        fetch_options: crate::origin::options(ctx),
        state: StateConfig {
            path: ctx.config.paths.state_db.clone(),
            identity: RepositoryIdentity::new(
                ctx.config.repo_id.clone(),
                ctx.config.repo_path.to_string_lossy(),
            ),
        },
    };
    let github = GithubClient::new(
        ctx.processes.clone(),
        ctx.config.paths.main_clone.clone(),
        GithubOptions::from_config(&ctx.config, false),
    );
    let service = StackService::new(
        config,
        (*ctx.repository).clone(),
        ctx.processes.clone(),
        github,
    );
    if args.branch.as_deref() == Some("prune-backups") {
        if args.onto.is_some() {
            eprintln!("usage: wt restack prune-backups [--days <n>]");
            return Ok(2);
        }
        let mut event = |event| print_event(event);
        let result = service
            .prune_backups(args.days.unwrap_or(0), &ctx.cancellation, &mut event)
            .await?;
        println!(
            "deleted {} backup branch(es) ({} kept)",
            result.deleted.len(),
            result.kept.len()
        );
        return Ok(0);
    }
    if args.days.is_some() {
        eprintln!("--days is only accepted with prune-backups");
        return Ok(2);
    }
    let branch = match args.branch.as_ref() {
        Some(branch) => branch.clone(),
        None => match branch_from_cwd(ctx).await? {
            Some(branch) => {
                println!("restacking from current branch {branch}");
                branch
            }
            None => {
                eprintln!("usage: wt restack [<branch>] [--onto <ref>]");
                eprintln!("(no branch given and the cwd is not on one)");
                return Ok(2);
            }
        },
    };
    let mut event = |event| print_event(event);
    match service
        .restack(
            &branch,
            RestackOptions {
                onto: args.onto.clone(),
            },
            &ctx.cancellation,
            &mut event,
        )
        .await?
    {
        RestackOutcome::Complete { replayed, total } => {
            println!("✓ restacked {branch} ({replayed}/{total} worktree(s) replayed)");
            Ok(0)
        }
        RestackOutcome::Conflict {
            branch,
            backup_ref,
            error,
        } => {
            eprintln!("{error}");
            eprintln!("  failing branch: {branch}");
            eprintln!("  backup branch:  {backup_ref}");
            eprintln!("  resolve in that worktree, then re-run `wt restack` or use /restack.");
            Ok(3)
        }
        RestackOutcome::Refused { error } => {
            eprintln!("{error}");
            Ok(1)
        }
    }
}

fn print_event(event: StackEvent) {
    match event {
        StackEvent::Log(message) => println!("  {message}"),
        StackEvent::Attention(message) => eprintln!("  attention: {message}"),
    }
}

async fn branch_from_cwd(ctx: &AppContext) -> Result<Option<String>> {
    let output = ctx
        .processes
        .run(
            CommandSpec::new("git")
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .cwd(ctx.cwd.clone()),
            &ctx.cancellation,
        )
        .await?;
    if !output.status.success() {
        return Ok(None);
    }
    let branch = output.stdout_text().trim().to_owned();
    if branch.is_empty() || branch == "HEAD" {
        Ok(None)
    } else {
        Ok(Some(branch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::process::Command;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: RestackArgs,
    }

    #[test]
    fn parser_accepts_branch_onto_and_backup_prune_forms() {
        let parsed = TestCli::try_parse_from(["wt", "feature/a", "--onto", "origin/main"]).unwrap();
        assert_eq!(parsed.args.branch.as_deref(), Some("feature/a"));
        assert_eq!(parsed.args.onto.as_deref(), Some("origin/main"));
        let parsed = TestCli::try_parse_from(["wt", "prune-backups", "--days", "3"]).unwrap();
        assert_eq!(parsed.args.branch.as_deref(), Some("prune-backups"));
        assert_eq!(parsed.args.days, Some(3));
    }

    #[test]
    fn parser_rejects_unknown_flags_and_extra_branches() {
        assert!(TestCli::try_parse_from(["wt", "feature", "another"]).is_err());
        assert!(TestCli::try_parse_from(["wt", "--unknown"]).is_err());
    }

    #[tokio::test]
    async fn detached_checkout_has_no_current_branch() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let mut context = fixture.ctx.clone();
        let path = context.config.paths.worktree_root.join("one");
        let output = Command::new("git")
            .args(["checkout", "--detach"])
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(output.status.success());
        context.cwd = path;
        let result = branch_from_cwd(&context).await.unwrap();
        assert_eq!(result, None);
        fixture.close().await.unwrap();
    }
}
