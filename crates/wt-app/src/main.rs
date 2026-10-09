mod activity_source;
mod bootstrap;
mod commands;
mod context;
mod controller;
mod controller_actions;
mod database;
mod editor;
mod freshness;
mod harness;
mod inventory;
mod lifecycle_ops;
mod logging;
mod prompt;
mod skills;
mod sources;
mod updates;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use wt_config::{Config, LoadOptions};
use wt_platform::process::ProcessRunner;
use wt_runtime::TaskScope;
use wt_vcs::{GitRepository, RepositoryConfig, StageConfig};

#[derive(Parser)]
#[command(name = "wt", version, about = "Git worktree manager")]
struct Cli {
    #[arg(long = "_boot-probe", hide = true)]
    boot_probe: bool,
    #[arg(short = 'v', action = clap::ArgAction::Version)]
    _version: Option<bool>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn version_and_help_parse_without_repository_configuration() {
        for args in [vec!["wt"], vec!["wt", "status"], vec!["wt", "merge", "one"]] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        for args in [
            vec!["wt", "-v"],
            vec!["wt", "--version"],
            vec!["wt", "--help"],
            vec!["wt", "status", "--help"],
        ] {
            let error = Cli::try_parse_from(args)
                .err()
                .expect("help/version exit before execution");
            assert!(matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ));
        }
        assert_eq!(
            Cli::try_parse_from(["wt", "invalid-command"])
                .err()
                .unwrap()
                .exit_code(),
            2
        );
    }
}

#[derive(Subcommand)]
enum Command {
    /// Install or inspect checked native releases.
    Update(commands::update::UpdateArgs),
    /// Activate a previously installed native version.
    Rollback(commands::rollback::RollbackArgs),
    /// Inspect and synchronize bundled coding-agent skills and instructions.
    Skills(commands::skills::SkillsArgs),
    /// Inspect, start and message coding-agent sessions.
    Agent(commands::agent::AgentArgs),
    /// Claude compatibility commands and diagnostics.
    Claude(commands::claude::ClaudeArgs),
    /// Diagnose Codex message transport and session readiness.
    Codex(commands::codex::CodexArgs),
    /// Coordinate the manager session and its report spool.
    Manager(commands::manager::ManagerArgs),
    /// Inspect or assert a bounded maintenance hold.
    Hold(commands::hold::HoldArgs),
    /// Initialize repository configuration without an existing wt installation.
    Init(commands::init::InitArgs),
    /// Execute an acknowledged, durable background removal job.
    #[command(name = "_destroy", hide = true)]
    Destroy(commands::_destroy::DestroyWorkerArgs),
    /// Create a worktree from an issue, branch or slug.
    New(commands::new::NewArgs),
    /// Remove a worktree after checking for work that would be lost.
    #[command(name = "rm", alias = "remove")]
    Remove(commands::remove::RemoveArgs),
    /// Clean worktrees whose work has landed.
    #[command(name = "clean", alias = "cleanup")]
    Cleanup(commands::cleanup::CleanupArgs),
    /// List worktrees and retained removal records.
    #[command(name = "ls", alias = "list")]
    List(commands::list::ListArgs),
    /// Put a worktree in the archive without removing its checkout.
    Archive(commands::archive::ArchiveArgs),
    /// Return an archived worktree to the active board.
    Restore(commands::restore::RestoreArgs),
    /// Show or assert a worktree's work status.
    Status(commands::status::StatusArgs),
    /// Show, set or clear a recorded fork base.
    Base(commands::base::BaseArgs),
    /// Organize worktrees into named sections.
    Section(commands::section::SectionArgs),
    /// Record dependencies and conflicts that expire when branches move.
    Edge(commands::edge::EdgeArgs),
    /// Arm merge-when-ready or cancel the PR's actual merge state.
    Merge(commands::merge::MergeArgs),
    /// Open a worktree in the configured editor.
    Open(commands::open::OpenArgs),
    /// Follow the latest retained destroy log.
    Logs(commands::logs::LogsArgs),
    /// Print the running native version.
    Version,
    /// Inspect the native inventory source during conversion.
    #[command(name = "_inventory", hide = true)]
    Inventory,
}

fn main() {
    let cli = Cli::parse();
    if cli.boot_probe {
        println!("wt-build-id:{}:{}", env!("WT_BUILD_ID"), env!("WT_TARGET"));
        return;
    }
    // Recovery and informational commands never load repository configuration.
    if matches!(cli.command, Some(Command::Version)) {
        println!(
            "wt {} ({}, {})",
            env!("CARGO_PKG_VERSION"),
            env!("WT_BUILD_ID"),
            env!("WT_TARGET")
        );
        return;
    }
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("wt: {error:#}");
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    let options = LoadOptions::default();
    let boot = if matches!(cli.command, Some(Command::Update(_) | Command::Rollback(_))) {
        None
    } else {
        bootstrap::BootAttempt::from_options(&options)?
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .thread_name("wt-worker")
        .enable_all()
        .build();
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) => {
            if let Some(boot) = boot
                && let Err(recovery) = boot.failed_sync(error.to_string())
            {
                eprintln!("wt: boot recovery: {recovery:#}");
            }
            return Err(error).context("create worker runtime");
        }
    };
    runtime.block_on(async move {
        let scope = TaskScope::new();
        let token = scope.token();
        if let Err(error) = signals::start(&scope, token.clone()) {
            if let Some(boot) = boot
                && let Err(recovery) = boot.failed(error.to_string()).await
            {
                eprintln!("wt: boot recovery: {recovery:#}");
            }
            return Err(error);
        }
        let result = async {
            // Config and logging belong to one repository. Their failures do
            // not reject the global installation for every other repository.
            if let Some(boot) = boot {
                boot.confirm().await?;
            }
            match &cli.command {
                Some(Command::Update(args)) => {
                    return commands::update::run(&options, args, &token).await;
                }
                Some(Command::Rollback(args)) => {
                    return commands::rollback::run(&options, args).await;
                }
                _ => {}
            }
            if let Some(Command::Init(args)) = &cli.command {
                return commands::init::run(&options, args, &token).await;
            }
            let config_options = options.clone();
            let config = Arc::new(
                tokio::task::spawn_blocking(move || Config::load(&config_options)).await??,
            );
            let _logging = logging::initialize(&config.paths.app_log_dir)?;
            if cli.command.is_none() {
                updates::startup_check(&options, &config, &token).await?;
            }
            run_application(cli, options, config, &scope, token).await
        }
        .await;
        let shutdown = scope.shutdown(Duration::from_secs(5)).await;
        // Preserve the actual command failure while reporting failed cleanup.
        if let Err(error) = &shutdown {
            tracing::error!(%error, "application shutdown failed");
        }
        let code = result?;
        shutdown?;
        Ok(code)
    })
}

async fn run_application(
    cli: Cli,
    options: LoadOptions,
    config: Arc<Config>,
    scope: &TaskScope,
    token: tokio_util::sync::CancellationToken,
) -> Result<i32> {
    let processes = ProcessRunner::default();
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
        processes.clone(),
    ));
    if matches!(cli.command, Some(Command::Inventory)) {
        let rows = repository.inventory_status(&token).await?;
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    let database = database::Database::open(&config).await?;
    if let Some(command) = &cli.command {
        let context = context::AppContext {
            config: config.clone(),
            home: options.home.clone(),
            cwd: options.cwd,
            database: database.clone(),
            repository: repository.clone(),
            processes,
            cancellation: token,
        };
        let result = match command {
            Command::Skills(args) => commands::skills::run(&context, args).await,
            Command::Agent(args) => commands::agent::run(&context, args).await,
            Command::Claude(args) => commands::claude::run(&context, args).await,
            Command::Codex(args) => commands::codex::run(&context, args).await,
            Command::Manager(args) => commands::manager::run(&context, args).await,
            Command::Hold(args) => commands::hold::run(&context, args).await,
            Command::Destroy(args) => commands::_destroy::run_worker(&context, args).await,
            Command::New(args) => commands::new::run(&context, args).await,
            Command::Remove(args) => commands::remove::run(&context, args).await,
            Command::Cleanup(args) => commands::cleanup::run(&context, args).await,
            Command::List(args) => commands::list::run(&context, args).await,
            Command::Archive(args) => commands::archive::run(&context, args).await,
            Command::Restore(args) => commands::restore::run(&context, args).await,
            Command::Status(args) => commands::status::run(&context, args).await,
            Command::Base(args) => commands::base::run(&context, args).await,
            Command::Section(args) => commands::section::run(&context, args).await,
            Command::Edge(args) => commands::edge::run(&context, args).await,
            Command::Merge(args) => commands::merge::run(&context, args).await,
            Command::Open(args) => commands::open::run(&context, args).await,
            Command::Logs(args) => commands::logs::run(&context, args).await,
            Command::Init(_)
            | Command::Update(_)
            | Command::Rollback(_)
            | Command::Inventory
            | Command::Version => unreachable!("handled before opening state"),
        };
        database.shutdown().await?;
        return result;
    }
    let (actions, controller_port) = wt_tui::action_channel();
    let context = context::AppContext {
        config,
        home: options.home,
        cwd: options.cwd,
        database: database.clone(),
        repository,
        processes,
        cancellation: token.clone(),
    };
    skills::startup_check(&context).await?;
    let sources = sources::start(scope, &context);
    let controller = controller::start(
        scope,
        context,
        sources.local,
        sources.board.clone(),
        controller_port,
    );
    // A child cancellation token does not cancel its parent. The terminal
    // token handles Ctrl+C and explicit application shutdown owns the scope.
    let result = wt_tui::run(sources.board, actions, token).await;
    scope.cancel();
    let actions_finished = controller.shutdown().await;
    let shutdown = scope.shutdown(Duration::from_secs(5)).await;
    database.shutdown().await?;
    actions_finished?;
    shutdown?;
    result.context("terminal")?;
    Ok(0)
}

mod signals {
    use super::*;
    use tokio_util::sync::CancellationToken;

    pub fn start(scope: &TaskScope, application: CancellationToken) -> Result<()> {
        let shutdown = scope.token();
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            // Register synchronously before any command can spawn a child. A
            // separate scope token lets normal CLI completion stop this task.
            let mut interrupt = signal(SignalKind::interrupt())?;
            let mut terminate = signal(SignalKind::terminate())?;
            let mut hangup = signal(SignalKind::hangup())?;
            scope.spawn(async move {
                tokio::select! {
                    _ = shutdown.cancelled() => {},
                    _ = interrupt.recv() => application.cancel(),
                    _ = terminate.recv() => application.cancel(),
                    _ = hangup.recv() => application.cancel(),
                }
            });
        }
        #[cfg(not(unix))]
        scope.spawn(async move {
            tokio::select! {
                _ = shutdown.cancelled() => {},
                _ = tokio::signal::ctrl_c() => application.cancel(),
            }
        });
        Ok(())
    }
}
