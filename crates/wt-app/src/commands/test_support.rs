#![cfg(test)]

use std::{collections::BTreeMap, fs, process::Command, sync::Arc};

use anyhow::{Context, Result};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wt_config::{Config, LoadOptions};
use wt_platform::process::ProcessRunner;
use wt_vcs::{GitRepository, RepositoryConfig, StageConfig};

use crate::{context::AppContext, database::Database};

pub struct CommandFixture {
    pub _root: TempDir,
    pub ctx: AppContext,
}

impl CommandFixture {
    pub async fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let repo = root.path().join("main");
        let worktree_root = root.path().join("worktrees");
        let home = root.path().join("home");
        fs::create_dir_all(&home)?;
        fs::create_dir_all(&worktree_root)?;
        fs::create_dir_all(&repo)?;
        run_git(&repo, &["init", "-b", "main"])?;
        run_git(&repo, &["config", "user.name", "Fixture"])?;
        run_git(&repo, &["config", "user.email", "fixture@example.invalid"])?;
        fs::write(repo.join("tracked.txt"), "base\n")?;
        run_git(&repo, &["add", "tracked.txt"])?;
        run_git(&repo, &["commit", "-m", "fixture base"])?;
        run_git(&repo, &["branch", "base-branch"])?;
        let first = worktree_root.join("one");
        let second = worktree_root.join("two");
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature/one",
                first.to_str().unwrap(),
                "main",
            ],
        )?;
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature/two",
                second.to_str().unwrap(),
                "main",
            ],
        )?;
        fs::write(
            repo.join(".wt.toml"),
            format!(
                "[paths]\nmain_clone = {:?}\nworktree_root = {:?}\nstate_db = {:?}\ncache_db = {:?}\n[branch]\nprefix = \"feature/\"\nbase = \"main\"\n[stage]\nprefix = \"stage-\"\n",
                repo.to_string_lossy(),
                worktree_root.to_string_lossy(),
                root.path().join("state.sqlite").to_string_lossy(),
                root.path().join("cache.sqlite").to_string_lossy(),
            ),
        )?;
        let options = LoadOptions::new(&first, &home, BTreeMap::new());
        let config = Arc::new(Config::load(&options)?);
        let repository = Arc::new(GitRepository::new(
            RepositoryConfig {
                main_clone: config.paths.main_clone.clone(),
                worktree_root: config.paths.worktree_root.clone(),
                trunk_branch: config.branch.base.clone(),
                stage: StageConfig {
                    prefix: config.stage.prefix.clone(),
                    issue_id_pattern: config.branch.id_pattern.clone(),
                },
            },
            ProcessRunner::default(),
        ));
        let database = Database::open(&config).await?;
        let ctx = AppContext {
            config,
            home,
            cwd: first.clone(),
            database,
            repository,
            processes: ProcessRunner::default(),
            cancellation: CancellationToken::new(),
            section_writes: Default::default(),
        };
        Ok(Self { _root: root, ctx })
    }

    pub async fn close(&self) -> Result<()> {
        self.ctx.database.clone().shutdown().await
    }
}

fn run_git(cwd: &std::path::Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .context("run fixture git")?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
