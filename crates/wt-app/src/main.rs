mod action_builtins;
mod action_dispatch;
mod action_palette;
mod action_source;
mod actions;
mod activity_source;
mod attention_source;
mod automation_builtins;
mod automation_engine;
mod automation_facts;
mod automation_source;
mod board_layout;
mod bootstrap;
mod commands;
mod context;
mod controller;
mod controller_actions;
mod database;
mod dev;
mod dev_source;
mod display_time;
mod editor;
mod events;
mod fleet_cleanup;
mod fork_base;
mod freshness;
mod git_presentation;
mod github_actions;
mod github_events_source;
mod github_pickers;
mod hard_refresh;
mod harness;
mod history_actions;
mod history_source;
mod host_cleanup;
mod host_dispatch;
mod host_protocol;
mod host_routing;
mod host_server;
mod host_service;
mod host_stdio;
mod install;
mod inventory;
mod issue_identity;
mod issue_source;
mod lifecycle_ops;
mod local_source;
mod logging;
mod naming;
mod naming_source;
mod origin;
mod perf_source;
mod prompt;
mod remote;
mod remote_board;
mod remote_cache;
mod remote_host;
mod restack_action;
mod review_requests;
mod section_actions;
mod session_activity;
mod session_board;
mod session_source;
mod session_ui;
mod skills;
mod sources;
mod terminal_palette;
mod updates;
mod work_presentation;
mod worktree_facts;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use wt_config::{Config, LoadOptions};
use wt_platform::process::ProcessRunner;
use wt_runtime::TaskScope;
use wt_vcs::{GitRepository, RepositoryConfig, StageConfig};

#[derive(Parser)]
#[command(name = "wt", version = NATIVE_VERSION, about = "Git worktree manager")]
struct Cli {
    #[arg(long = "_boot-probe", hide = true)]
    boot_probe: bool,
    #[arg(short = 'v', action = clap::ArgAction::Version)]
    _version: Option<bool>,
    #[command(subcommand)]
    command: Option<Command>,
}

const NATIVE_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("WT_BUILD_ID"),
    ", ",
    env!("WT_TARGET"),
    ")"
);

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
            if error.kind() == clap::error::ErrorKind::DisplayVersion {
                assert_eq!(error.to_string().trim(), format!("wt {NATIVE_VERSION}"));
            }
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
    /// Execute one acknowledged, durable action job.
    #[command(name = "_action-worker", hide = true)]
    ActionWorker(commands::_action_worker::ActionWorkerArgs),
    #[command(name = "_restack-worker", hide = true)]
    RestackWorker(commands::_restack_worker::RestackWorkerArgs),
    /// Manage the optional GitHub webhook daemon.
    Events(commands::events::EventsArgs),
    /// Inspect SST stages and safely clean confirmed orphaned stages.
    Stages(commands::stages::StagesArgs),
    /// Migrate legacy state into the repository's durable native store.
    State(commands::state::StateArgs),
    /// Inspect or change the task attached to a worktree.
    Issue(commands::issue::IssueArgs),
    /// Diagnose repository and coding-agent session health.
    Doctor(commands::doctor::DoctorArgs),
    /// Show a compact fleet report.
    Fleet(commands::fleet::FleetArgs),
    /// Inspect wt process resource usage and machine load.
    Perf(commands::perf::PerfArgs),
    /// Manage a worktree's supervised development server.
    Dev(commands::dev::DevArgs),
    #[command(name = "_dev-supervise", hide = true)]
    DevSupervisor(commands::dev::DevSupervisorArgs),
    #[command(name = "_dev-giveup", hide = true)]
    DevGiveup(commands::_dev_giveup::DevGiveupArgs),
    #[command(name = "_claude-hook", hide = true)]
    ClaudeHook(commands::_claude_hook::ClaudeHookArgs),
    /// Install a verified native release without a source checkout.
    Install(commands::install::InstallArgs),
    /// Forward a command to the configured native SSH worker.
    Remote(commands::remote::RemoteArgs),
    /// Replay a stack onto updated parents without replaying squash-merged work.
    Restack(commands::restack::RestackArgs),
    #[command(name = "_hello", hide = true)]
    Hello { args: Vec<String> },
    #[command(name = "_snapshot", hide = true)]
    Snapshot { args: Vec<String> },
    #[command(name = "_session", hide = true)]
    Session { args: Vec<String> },
    #[command(name = "_remote", hide = true)]
    WorkerDispatch { args: Vec<String> },
    #[command(name = "_host", hide = true)]
    Host,
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
        println!("wt {NATIVE_VERSION}");
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
    let boot = if matches!(
        cli.command,
        Some(Command::Install(_) | Command::Update(_) | Command::Rollback(_))
    ) {
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
                Some(Command::ClaudeHook(args)) => {
                    return commands::_claude_hook::run(args).await;
                }
                Some(Command::Install(args)) => {
                    return commands::install::run(&options, args, &token).await;
                }
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
            if let Some(Command::Hello { args }) = &cli.command {
                return commands::_hello::run(&config, args);
            }
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
        let result = dispatch_command(&context, command).await;
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
    let events_context = context.clone();
    scope.spawn(async move {
        if let Err(error) = events::reconcile_at_startup(&events_context).await {
            tracing::warn!(%error, "GitHub events service reconciliation failed");
        }
    });
    let commands = tokio_util::sync::CancellationToken::new();
    let host = Arc::new(host_service::HostService::start(
        scope,
        context.clone(),
        commands,
    ));
    let fleet = remote_board::start(scope, &context, host);
    let board = display_time::overlay(scope, fleet.board.clone());
    let controller = controller::start(scope, context.clone(), fleet, controller_port);
    // A child cancellation token does not cancel its parent. The terminal
    // token handles Ctrl+C and explicit application shutdown owns the scope.
    let result = terminal_palette::run(scope, board, actions, token, &context).await;
    scope.cancel();
    let actions_finished = controller.shutdown().await;
    let shutdown = scope.shutdown(Duration::from_secs(5)).await;
    database.shutdown().await?;
    actions_finished?;
    shutdown?;
    result.context("terminal")?;
    Ok(0)
}

async fn dispatch_command(context: &context::AppContext, command: &Command) -> Result<i32> {
    match command {
        Command::ActionWorker(args) => commands::_action_worker::run(context, args).await,
        Command::RestackWorker(args) => commands::_restack_worker::run(context, args).await,
        Command::Events(args) => commands::events::run(context, args).await,
        Command::Stages(args) => commands::stages::run(context, args).await,
        Command::State(args) => commands::state::run(context, args).await,
        Command::Issue(args) => commands::issue::run(context, args).await,
        Command::Doctor(args) => commands::doctor::run(context, args).await,
        Command::Fleet(args) => commands::fleet::run(context, args).await,
        Command::Perf(args) => commands::perf::run(context, args).await,
        Command::Dev(args) => commands::dev::run(context, args).await,
        Command::DevSupervisor(args) => commands::dev::run_supervisor_command(context, args).await,
        Command::DevGiveup(args) => commands::_dev_giveup::run(context, args).await,
        Command::ClaudeHook(args) => commands::_claude_hook::run(args).await,
        Command::Skills(args) => commands::skills::run(context, args).await,
        Command::Agent(args) => commands::agent::run(context, args).await,
        Command::Claude(args) => commands::claude::run(context, args).await,
        Command::Codex(args) => commands::codex::run(context, args).await,
        Command::Manager(args) => commands::manager::run(context, args).await,
        Command::Hold(args) => commands::hold::run(context, args).await,
        Command::Destroy(args) => commands::_destroy::run_worker(context, args).await,
        Command::New(args) => commands::new::run(context, args).await,
        Command::Remove(args) => commands::remove::run(context, args).await,
        Command::Cleanup(args) => commands::cleanup::run(context, args).await,
        Command::List(args) => commands::list::run(context, args).await,
        Command::Archive(args) => commands::archive::run(context, args).await,
        Command::Restore(args) => commands::restore::run(context, args).await,
        Command::Status(args) => commands::status::run(context, args).await,
        Command::Base(args) => commands::base::run(context, args).await,
        Command::Section(args) => commands::section::run(context, args).await,
        Command::Edge(args) => commands::edge::run(context, args).await,
        Command::Merge(args) => commands::merge::run(context, args).await,
        Command::Open(args) => commands::open::run(context, args).await,
        Command::Logs(args) => commands::logs::run(context, args).await,
        Command::Remote(args) => commands::remote::run(context, args).await,
        Command::Restack(args) => commands::restack::run(context, args).await,
        Command::Hello { args } => commands::_hello::run(&context.config, args),
        Command::Snapshot { args } => commands::_snapshot::run(context, args).await,
        Command::Session { args } => commands::_session::run(context, args).await,
        Command::WorkerDispatch { args } => Box::pin(commands::_remote::run(context, args)).await,
        Command::Host => host_server::run(context).await,
        Command::Version => {
            println!("wt {NATIVE_VERSION}");
            Ok(0)
        }
        Command::Inventory => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &context
                        .repository
                        .inventory_status(&context.cancellation)
                        .await?
                )?
            );
            Ok(0)
        }
        Command::Install(args) => {
            commands::install::run(&LoadOptions::default(), args, &context.cancellation).await
        }
        Command::Init(args) => {
            commands::init::run(&LoadOptions::default(), args, &context.cancellation).await
        }
        Command::Update(args) => {
            commands::update::run(&LoadOptions::default(), args, &context.cancellation).await
        }
        Command::Rollback(args) => commands::rollback::run(&LoadOptions::default(), args).await,
    }
}

pub(crate) async fn dispatch_worker_args(
    context: &context::AppContext,
    argv: &[String],
) -> Result<i32> {
    let cli =
        match Cli::try_parse_from(std::iter::once("wt".to_owned()).chain(argv.iter().cloned())) {
            Ok(cli) => cli,
            Err(error) => {
                let code = error.exit_code();
                error.print()?;
                return Ok(code);
            }
        };
    let command = cli
        .command
        .context("remote invocation requires a command; use _session for interactive sessions")?;
    if matches!(command, Command::WorkerDispatch { .. } | Command::Remote(_)) {
        anyhow::bail!("recursive remote forwarding is not supported");
    }
    dispatch_command(context, &command).await
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
