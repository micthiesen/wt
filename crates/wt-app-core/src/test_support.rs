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
        // Tiny temporary repositories must not inherit a host fsmonitor daemon:
        // its filesystem event waits can dominate every fixture status/diff.
        run_git(&repo, &["config", "core.fsmonitor", "false"])?;
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
        let caches = crate::query_cache::store(&config);
        let processes = ProcessRunner::default();
        let ctx = AppContext {
            config,
            home,
            cwd: first.clone(),
            database,
            repository,
            background_processes: processes.clone(),
            processes,
            interactive_processes: ProcessRunner::default(),
            cancellation: CancellationToken::new(),
            section_writes: Default::default(),
            caches,
        };
        Ok(Self { _root: root, ctx })
    }

    pub async fn close(&self) -> Result<()> {
        remove_tmux_server(&self.ctx, &self.ctx.config.tmux.socket).await;
        self.ctx.database.clone().shutdown().await
    }
}

/// Stop a fixture's tmux server and delete its socket file, which tmux leaves
/// behind even after `kill-server`. Only a socket at tmux's own path for this
/// name is removed.
pub async fn remove_tmux_server(ctx: &AppContext, socket: &str) {
    let tmux = wt_tmux::TmuxClient::new(ctx.processes.clone(), wt_tmux::TmuxServer::named(socket));
    let _ = tmux.kill_server(&ctx.cancellation).await;
    let base = std::env::var_os("TMUX_TMPDIR").unwrap_or_else(|| "/tmp".into());
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let path = std::path::Path::new(&base)
        .join(format!("tmux-{uid}"))
        .join(socket);
    use std::os::unix::fs::FileTypeExt;
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_socket()) {
        let _ = fs::remove_file(path);
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
