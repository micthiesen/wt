//! The wt application: CLI commands, the terminal board and its host.
//!
//! The `wt` binary in `crates/wt-app` is a shim that calls [`main`] with the
//! identity it was compiled as; everything else lives here so a release
//! rebuilds only that shim (docs/ci.md).

mod commands;
mod stale_build;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use wt_config::{Config, LoadOptions};
use wt_platform::process::ProcessRunner;
use wt_runtime::TaskScope;
use wt_vcs::{GitRepository, RepositoryConfig, StageConfig};

#[derive(Parser)]
#[command(name = "wt", version = wt_app_core::build_info::native_version(), about = "Git worktree manager")]
struct Cli {
    #[arg(long = "_boot-probe", hide = true)]
    boot_probe: bool,
    /// Print version
    #[arg(short = 'v', action = clap::ArgAction::Version)]
    _version: Option<bool>,
    #[command(subcommand)]
    command: Option<Command>,
}

pub use wt_app_core::build_info::BuildInfo;

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn boot_token_does_not_reach_child_processes() {
        const CHILD: &str = "WT_TEST_BOOT_TOKEN_CHILD";
        let token = wt_launcher::BOOT_ATTEMPT_TOKEN_ENV;
        if std::env::var_os(CHILD).is_some() {
            // SAFETY: this re-executed process runs only this test, and the
            // harness thread waiting on it does not read the environment.
            let options = unsafe { startup_options() };
            assert_eq!(options.env.get(token).map(String::as_str), Some("attempt"));
            let inherited = std::process::Command::new("/usr/bin/env").output().unwrap();
            assert!(
                !String::from_utf8_lossy(&inherited.stdout).contains(token),
                "a child process inherited the boot token"
            );
            return;
        }
        // The environment is process-wide, so the startup path runs in a
        // process of its own, launched the way the launcher starts wt.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli_tests::boot_token_does_not_reach_child_processes",
            ])
            .env(CHILD, "1")
            .env(token, "attempt")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}"
        );
    }

    #[test]
    fn error_chain_prints_an_embedded_source_once() {
        let io = std::io::Error::from_raw_os_error(1);
        let error = anyhow::Error::new(wt_harness::CodexTrustError::Io {
            path: "/h/.codex/config.toml".into(),
            source: io,
        })
        .context("prepare Codex target trust");
        let text = error_chain(&error);
        assert_eq!(text.matches("os error 1").count(), 1, "{text}");
        assert!(
            text.starts_with(
                "prepare Codex target trust: Codex trust file /h/.codex/config.toml: "
            )
        );
        let plain = anyhow::anyhow!("inner").context("outer");
        assert_eq!(error_chain(&plain), "outer: inner");
    }

    /// Visible arguments and subcommands that still have no help text, as
    /// `wt <path> <arg>`. Agents are told to read `--help` before guessing a
    /// flag, so a new entry here needs a doc comment instead.
    const UNDOCUMENTED: &[&str] = &[
        "wt controller serve",
        "wt controller status",
        "wt controller exec",
        "wt controller exec <service>",
        "wt stages <json>",
        "wt stages <clean>",
        "wt stages <yes>",
        "wt state migrate",
        "wt state migrate <from>",
        "wt state migrate <keep_legacy>",
        "wt doctor <target>",
        "wt doctor <all>",
        "wt doctor <json>",
        "wt perf <json>",
        "wt dev start <slug>",
        "wt dev start <wait>",
        "wt dev start <timeout>",
        "wt dev start <rebuild>",
        "wt dev reset <slug>",
        "wt dev reset <wait>",
        "wt dev reset <timeout>",
        "wt dev stop <slug>",
        "wt dev status <slug>",
        "wt dev status <all>",
        "wt dev status <json>",
        "wt dev queue <slug>",
        "wt dev queue <first>",
        "wt dev queue <normal>",
        "wt dev queue <json>",
        "wt dev logs <slug>",
        "wt remote <args>",
        "wt skills sync <units>",
        "wt skills sync <yes>",
        "wt skills sync <force>",
        "wt skills diff <unit>",
        "wt skills reset <answers>",
        "wt skills reset <declines>",
        "wt claude send <target>",
        "wt claude send <text>",
        "wt claude ls <json>",
        "wt claude selftest <slug>",
        "wt claude stop <target>",
        "wt manager send",
        "wt manager send <hold>",
        "wt manager send <text>",
        "wt manager report",
        "wt manager report <ok>",
        "wt manager report <warn>",
        "wt manager report <err>",
        "wt manager report <info>",
        "wt manager report <text>",
        "wt hold release",
        "wt hold release <resource>",
        "wt hold release <event_at>",
        "wt hold release <owner>",
        "wt hold release <reason>",
        "wt hold status",
        "wt hold status <resource>",
        "wt init <directory>",
        "wt init <primary>",
        "wt archive <target>",
        "wt restore <target>",
        "wt section <json>",
        "wt section list",
        "wt section list <json>",
        "wt section rename",
        "wt section rename <old>",
        "wt section rename <new>",
        "wt section rm",
        "wt section rm <name>",
        "wt guide <clear>",
        "wt guide <json>",
    ];

    #[test]
    fn every_visible_argument_and_subcommand_has_help() {
        use clap::CommandFactory;
        fn walk(command: &clap::Command, path: &str, missing: &mut Vec<String>) {
            for arg in command.get_arguments() {
                if !arg.is_hide_set() && arg.get_help().is_none() {
                    missing.push(format!("{path} <{}>", arg.get_id()));
                }
            }
            for sub in command.get_subcommands() {
                // The generated `help` subtree repeats every subcommand.
                if sub.is_hide_set() || sub.get_name() == "help" {
                    continue;
                }
                let path = format!("{path} {}", sub.get_name());
                if sub.get_about().is_none() {
                    missing.push(path.clone());
                }
                walk(sub, &path, missing);
            }
        }
        let mut command = Cli::command();
        command.build();
        let mut missing = Vec::new();
        walk(&command, "wt", &mut missing);
        let unexpected = missing
            .iter()
            .filter(|entry| !UNDOCUMENTED.contains(&entry.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            unexpected.is_empty(),
            "no --help text for:\n{}",
            unexpected.join("\n")
        );
        let documented = UNDOCUMENTED
            .iter()
            .filter(|entry| !missing.iter().any(|found| found == *entry))
            .collect::<Vec<_>>();
        assert!(
            documented.is_empty(),
            "now documented; remove from UNDOCUMENTED: {documented:?}"
        );
    }

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
                assert_eq!(
                    error.to_string().trim(),
                    format!("wt {}", wt_app_core::build_info::native_version())
                );
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
    #[command(name = "_workload", hide = true)]
    Workload(wt_app_core::workload::WorkloadArgs),
    /// Coordinate account changes on managed workers.
    Auth(commands::auth::AuthArgs),
    /// Serve explicitly configured controller capabilities to managed workers.
    Controller(commands::controller_bridge::ControllerArgs),
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
    Issue(wt_ops::commands::issue::IssueArgs),
    /// Diagnose repository and coding-agent session health.
    Doctor(commands::doctor::DoctorArgs),
    /// Show a compact fleet report, for every worktree or the ones named.
    Fleet(commands::fleet::FleetArgs),
    /// Inspect wt process resource usage and machine load.
    Perf(wt_ops::commands::perf::PerfArgs),
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
    /// Asked by wt's gh shim before `gh pr create`; ARGS are gh's own.
    #[command(name = "_gh-guard", hide = true)]
    GhGuard {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(name = "_snapshot", hide = true)]
    Snapshot { args: Vec<String> },
    #[command(name = "_session", hide = true)]
    Session { args: Vec<String> },
    #[command(name = "_remote", hide = true)]
    WorkerDispatch { args: Vec<String> },
    #[command(name = "_host", hide = true)]
    Host,
    /// Converge a controller-managed worker; runs before any config exists.
    #[command(name = "_managed", hide = true)]
    Managed { args: Vec<String> },
    /// Install or inspect checked native releases.
    Update(commands::update::UpdateArgs),
    /// Activate a previously installed native version.
    Rollback(commands::rollback::RollbackArgs),
    /// Inspect and synchronize bundled coding-agent skills and instructions.
    Skills(commands::skills::SkillsArgs),
    /// Inspect, start and message coding-agent sessions.
    Agent(wt_ops::commands::agent::AgentArgs),
    /// Claude compatibility commands and diagnostics.
    Claude(commands::claude::ClaudeArgs),
    /// Diagnose Codex message transport and session readiness.
    Codex(commands::codex::CodexArgs),
    /// Coordinate the manager session and its report spool.
    Manager(wt_ops::commands::manager::ManagerArgs),
    /// Inspect or assert a bounded maintenance hold.
    Hold(wt_ops::commands::hold::HoldArgs),
    /// Show whether automations are running and what is queued.
    Automations(commands::automations::AutomationsArgs),
    /// Initialize repository configuration without an existing wt installation.
    Init(commands::init::InitArgs),
    /// List your open pull requests that no worktree row tracks.
    Orphans(commands::orphans::OrphansArgs),
    /// Execute an acknowledged, durable background removal job.
    #[command(name = "_destroy", hide = true)]
    Destroy(wt_ops::commands::_destroy::DestroyWorkerArgs),
    /// Create a worktree from an issue, branch or slug.
    New(commands::new::NewCommandArgs),
    /// Keep a worktree whose PR landed and start its next PR on a fresh branch.
    Next(commands::next::NextArgs),
    /// Remove a worktree after checking for work that would be lost.
    #[command(name = "rm", alias = "remove")]
    Remove(wt_ops::commands::remove::RemoveArgs),
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
    /// Wake this worktree's session once conditions hold.
    ///
    /// The session gets a prompt saying what held, with your note. Use it
    /// instead of sleeping or polling: `wt wake pr:3806:production -m "run
    /// the smoke test"`, `wt wake 17:00 -m "check the nightly job"`.
    Wake(commands::watch::WakeArgs),
    /// Tell the person once conditions hold.
    ///
    /// A desktop banner (and the `[notify]` command when configured) says
    /// what held, or your text. Use it for news that needs no session:
    /// `wt notify pr:3806:production`, `wt notify release:production+24h -m
    /// "soak is over"`, `wt notify 5pm -m "stand-up"`.
    Notify(commands::watch::NotifyArgs),
    /// Block until conditions hold, then say which.
    ///
    /// For what resolves while you are still working: CI, a review bot's
    /// pass, the merge queue. Nothing is stored and it works with the TUI
    /// closed: `wt wait --any settled reviewed evicted merged closed`. Exit
    /// 0 held, 124 timed out, 2 unreadable condition, 1 could not tell. A
    /// wait that outlives the session is `wt wake`.
    Wait(commands::wait::WaitArgs),
    /// List, cancel or check watches: what worktrees are waiting for.
    Watch(commands::watch::WatchArgs),
    /// Show what a worktree still owes: its merge gate, the checks owed
    /// once deployed, and the steps it comes back for.
    ///
    /// One list per worktree; `--all` groups the fleet's by what is due now,
    /// what must happen before the next production release, and what is
    /// waiting. Watches that only tell the person are not owed and stay
    /// under `wt watch`.
    Owed(commands::owed::OwedArgs),
    /// Close something the worktree owes, with the evidence for it.
    ///
    /// A wake delivered after the worktree's work landed stays owed until
    /// it is closed: `wt done <id> -m "what you did and what you saw"`. A
    /// check is closed by its stage: `wt done staging -m "..."`. To put
    /// either off instead: `wt done <id> --again tomorrow`.
    Done(commands::owed::DoneArgs),
    /// Show, set or clear a recorded fork base.
    Base(commands::base::BaseArgs),
    /// Organize worktrees into named sections.
    Section(wt_ops::commands::section::SectionArgs),
    /// Record dependencies and conflicts that expire when branches move.
    Edge(commands::edge::EdgeArgs),
    /// Show or record the worktree's change guide (`[guides] enabled`).
    Guide(commands::guide::GuideArgs),
    /// Show a worktree's recorded history: statuses, PR, checks, reviews,
    /// messages and automation fires, newest first.
    History(commands::history::HistoryArgs),
    /// Add a one-line note to a worktree's history.
    ///
    /// `wt mark "handed to QA"`. With `--claude-hook` it instead reads a
    /// Claude Code hook event on stdin and records a skill or slash command
    /// that `[history] skills` lists.
    Mark(commands::mark::MarkArgs),
    /// Show or set the worktree's title.
    ///
    /// Titles rank user > agent > issue > fallback; a lower source never
    /// replaces a higher one. An agent may replace any title but a person's,
    /// as often as the outcome changes; a person's rename is final.
    Title(commands::title::TitleArgs),
    /// Arm merge-when-ready or cancel the PR's actual merge state.
    ///
    /// Chooses the merge queue or classic auto-merge from the PR's base
    /// branch, so pending checks are fine. Armed is not landed: a queue can
    /// evict the PR and GitHub can drop auto-merge, so watch until the PR is
    /// merged, and re-arm only after fixing what disarmed it. Exit 75 means a
    /// required check has not reported yet: wait and re-run.
    Merge(commands::merge::MergeArgs),
    /// Open a worktree in the configured editor.
    Open(commands::open::OpenArgs),
    /// Show the latest retained destroy log, following it on a terminal.
    Logs(wt_ops::commands::logs::LogsArgs),
    /// Print the running native version.
    Version,
    /// Inspect the native inventory source during conversion.
    #[command(name = "_inventory", hide = true)]
    Inventory,
}

/// Run wt as the binary `info` describes, exiting the process when done.
pub fn main(info: BuildInfo) {
    wt_app_core::build_info::install(info);
    // wt runs inside the Claude sessions it starts, which carry BUN_INSPECT
    // for their own inspector socket. Every Bun program wt would run (hooks,
    // dev-server commands, actions) would otherwise try to bind that socket
    // and fail to start. wt sets it explicitly for each Claude it creates.
    // SAFETY: no other thread exists yet.
    unsafe { std::env::remove_var("BUN_INSPECT") };
    let cli = Cli::parse();
    if cli.boot_probe {
        println!(
            "wt-build-id:{}:{}",
            wt_app_core::build_info::build_id(),
            wt_app_core::build_info::target()
        );
        return;
    }
    stale_build::warn(
        &std::env::args_os().collect::<Vec<_>>(),
        wt_app_core::build_info::version(),
        wt_app_core::build_info::build_id(),
    );
    // Recovery and informational commands never load repository configuration.
    if matches!(cli.command, Some(Command::Version)) {
        println!("wt {}", wt_app_core::build_info::native_version());
        return;
    }
    let hook = matches!(&cli.command, Some(Command::Mark(args)) if args.claude_hook);
    match run(cli) {
        // A harness shows a hook's output to the model or the person on
        // every prompt, so a hook that cannot record says nothing.
        _ if hook => std::process::exit(0),
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("wt: {}", error_chain(&error));
            std::process::exit(1);
        }
    }
}

/// `{:#}` with each cause once. Many wt errors already embed their source in
/// their own message, so the plain chain would print it twice.
fn error_chain(error: &anyhow::Error) -> String {
    let mut text = String::new();
    for cause in error.chain() {
        let cause = cause.to_string();
        if !text.ends_with(&cause) {
            if !text.is_empty() {
                text.push_str(": ");
            }
            text.push_str(&cause);
        }
    }
    text
}

/// Snapshot the startup environment, then drop the launcher's boot token
/// from this process. The token acknowledges this one launch. A worker or
/// tmux server that inherited it would present it against a later install
/// state and be refused at startup.
///
/// # Safety
/// No other thread may exist: the process environment is not synchronized.
unsafe fn startup_options() -> LoadOptions {
    let options = LoadOptions::default();
    unsafe { std::env::remove_var(wt_launcher::BOOT_ATTEMPT_TOKEN_ENV) };
    options
}

fn run(mut cli: Cli) -> Result<i32> {
    // SAFETY: the worker runtime below starts the first other thread.
    let options = unsafe { startup_options() };
    let boot = if matches!(
        cli.command,
        Some(Command::Install(_) | Command::Update(_) | Command::Rollback(_))
    ) {
        None
    } else {
        wt_app_core::bootstrap::BootAttempt::from_options(&options)?
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
    let result = runtime.block_on(async move {
        let scope = TaskScope::new();
        let mut scope_drained = false;
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
                Some(Command::Workload(args)) => {
                    return wt_app_core::workload::run(args, &token).await;
                }
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
                Some(Command::Managed { args }) => {
                    return commands::_managed::run(args, &token).await;
                }
                Some(Command::Auth(args)) => {
                    if let Some(code) =
                        commands::auth::run_global(&options.home, args, &token).await?
                    {
                        return Ok(code);
                    }
                }
                Some(Command::Controller(commands::controller_bridge::ControllerArgs {
                    command: commands::controller_bridge::ControllerCommand::Exec { service },
                })) => return commands::controller_bridge::exec(service).await,
                _ => {}
            }
            if let Some(Command::Init(args)) = &cli.command {
                return commands::init::run(&options, args, &token).await;
            }
            let config_options = options.clone();
            let config = Arc::new(
                tokio::task::spawn_blocking(move || Config::load(&config_options)).await??,
            );
            let _logging = wt_app_core::logging::initialize(&config.paths.app_log_dir)?;
            if let Some(Command::Hello { args }) = &cli.command {
                return commands::_hello::run(&config, args);
            }
            if cli.command.is_none() {
                wt_ops::updates::startup_check(&options, &config, &token).await?;
            }
            // The hook runs on every prompt: one that has nothing to record
            // ends before the database and the worktree inventory are opened.
            if let Some(Command::Mark(args)) = &mut cli.command
                && args.claude_hook
            {
                args.hook_use = commands::mark::read_hook(&config.history);
                if args.hook_use.is_none() {
                    return Ok(0);
                }
            }
            run_application(cli, options, config, &scope, token, &mut scope_drained).await
        }
        .await;
        let shutdown = if scope_drained {
            Ok(())
        } else {
            scope.shutdown(Duration::from_secs(5)).await
        };
        // Preserve the actual command failure while reporting failed cleanup.
        if let Err(error) = &shutdown {
            tracing::error!(%error, "application shutdown failed");
        }
        let code = result?;
        shutdown?;
        Ok(code)
    });
    // Accepted commands, automation dispatches and database writes have their
    // own explicit drains above. Runtime destruction must not add an unbounded
    // wait for a cancelled read's non-interruptible blocking worker.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

async fn run_application(
    cli: Cli,
    options: LoadOptions,
    config: Arc<Config>,
    scope: &TaskScope,
    token: tokio_util::sync::CancellationToken,
    scope_drained: &mut bool,
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
    let database = wt_app_core::database::Database::open(&config).await?;
    if let Some(command) = &cli.command {
        let context = wt_app_core::context::AppContext {
            config: config.clone(),
            home: options.home.clone(),
            cwd: options.cwd,
            database: database.clone(),
            repository: repository.clone(),
            background_processes: processes.clone(),
            processes,
            interactive_processes: ProcessRunner::default(),
            cancellation: token,
            section_writes: Default::default(),
            caches: wt_app_core::query_cache::store(&config),
        };
        let result = dispatch_command(&context, command).await;
        // A failed drain must not replace the command's own failure.
        let shutdown = database.shutdown().await;
        if let (Err(_), Err(error)) = (&result, &shutdown) {
            tracing::error!(%error, "state database shutdown failed");
        }
        let code = result?;
        shutdown?;
        return Ok(code);
    }
    let (actions, controller_port) = wt_tui::action_channel();
    let caches = wt_app_core::query_cache::store(&config);
    let context = wt_app_core::context::AppContext {
        config,
        home: options.home,
        cwd: options.cwd,
        database: database.clone(),
        repository,
        background_processes: processes.clone(),
        processes,
        interactive_processes: ProcessRunner::default(),
        cancellation: token.clone(),
        section_writes: Default::default(),
        caches,
    };
    wt_app_core::skills::startup_check(&context).await?;
    let events_context = context.clone();
    scope.spawn(async move {
        if let Err(error) = wt_ops::events::reconcile(&events_context).await {
            tracing::warn!(%error, event_channel = "attention", "GitHub events service reconciliation failed");
        }
    });
    let commands = tokio_util::sync::CancellationToken::new();
    let host = Arc::new(wt_host::host_service::HostService::start(
        scope,
        context.clone(),
        commands,
    ));
    let fleet = wt_host::remote_board::start(scope, &context, host);
    let board = wt_app_core::display_time::overlay(scope, fleet.board.clone());
    let controller = wt_host::controller::start(scope, context.clone(), fleet, controller_port);
    // A child cancellation token does not cancel its parent. The terminal
    // token handles Ctrl+C and explicit application shutdown owns the scope.
    let result = wt_host::terminal_palette::run(scope, board, actions, token, &context).await;
    scope.cancel();
    let actions_finished = controller.shutdown().await;
    let shutdown = scope.shutdown(Duration::from_secs(5)).await;
    // Even a failed drain consumed its full grace period. The outer error path
    // must report it without waiting through that same grace period again.
    *scope_drained = true;
    let database_closed = database.shutdown().await;
    if let Err(error) = &database_closed {
        tracing::error!(%error, "state database shutdown failed");
    }
    actions_finished?;
    shutdown?;
    result.context("terminal")?;
    database_closed?;
    Ok(0)
}

async fn dispatch_command(
    context: &wt_app_core::context::AppContext,
    command: &Command,
) -> Result<i32> {
    match command {
        Command::ActionWorker(args) => commands::_action_worker::run(context, args).await,
        Command::RestackWorker(args) => commands::_restack_worker::run(context, args).await,
        Command::Events(args) => commands::events::run(context, args).await,
        Command::Stages(args) => commands::stages::run(context, args).await,
        Command::State(args) => commands::state::run(context, args).await,
        Command::Issue(args) => wt_ops::commands::issue::run(context, args).await,
        Command::Doctor(args) => commands::doctor::run(context, args).await,
        Command::Fleet(args) => commands::fleet::run(context, args).await,
        Command::Perf(args) => wt_ops::commands::perf::run(context, args).await,
        Command::Dev(args) => commands::dev::run(context, args).await,
        Command::DevSupervisor(args) => commands::dev::run_supervisor_command(context, args).await,
        Command::Workload(_) => unreachable!("workload helper runs before app composition"),
        Command::DevGiveup(args) => commands::_dev_giveup::run(context, args).await,
        Command::ClaudeHook(args) => commands::_claude_hook::run(args).await,
        Command::Skills(args) => commands::skills::run(context, args).await,
        Command::Agent(args) => wt_ops::commands::agent::run(context, args).await,
        Command::Claude(args) => commands::claude::run(context, args).await,
        Command::Codex(args) => commands::codex::run(context, args).await,
        Command::Manager(args) => wt_ops::commands::manager::run(context, args).await,
        Command::Hold(args) => wt_ops::commands::hold::run(context, args).await,
        Command::Automations(args) => commands::automations::run(context, args).await,
        Command::Destroy(args) => wt_ops::commands::_destroy::run_worker(context, args).await,
        Command::New(args) => commands::new::run(context, args).await,
        Command::Next(args) => commands::next::run(context, args).await,
        Command::Orphans(args) => commands::orphans::run(context, args).await,
        Command::Remove(args) => wt_ops::commands::remove::run(context, args).await,
        Command::Cleanup(args) => commands::cleanup::run(context, args).await,
        Command::List(args) => commands::list::run(context, args).await,
        Command::Archive(args) => commands::archive::run(context, args).await,
        Command::Restore(args) => commands::restore::run(context, args).await,
        Command::Status(args) => commands::status::run(context, args).await,
        Command::Wake(args) => commands::watch::run_wake(context, args).await,
        Command::Notify(args) => commands::watch::run_notify(context, args).await,
        Command::Wait(args) => commands::wait::run(context, args).await,
        Command::Watch(args) => commands::watch::run_watch(context, args).await,
        Command::Owed(args) => commands::owed::run_owed(context, args).await,
        Command::Done(args) => commands::owed::run_done(context, args).await,
        Command::Base(args) => commands::base::run(context, args).await,
        Command::Section(args) => commands::section::run(context, args).await,
        Command::Edge(args) => commands::edge::run(context, args).await,
        Command::Guide(args) => commands::guide::run(context, args).await,
        Command::History(args) => commands::history::run(context, args).await,
        Command::Mark(args) => commands::mark::run(context, args).await,
        Command::Title(args) => commands::title::run(context, args).await,
        Command::Merge(args) => commands::merge::run(context, args).await,
        Command::Open(args) => commands::open::run(context, args).await,
        Command::Logs(args) => wt_ops::commands::logs::run(context, args).await,
        Command::Remote(args) => commands::remote::run(context, args).await,
        Command::Auth(args) => commands::auth::run(context, args).await,
        Command::Controller(args) => match &args.command {
            commands::controller_bridge::ControllerCommand::Serve => {
                commands::controller_bridge::serve(context).await
            }
            commands::controller_bridge::ControllerCommand::Status => {
                commands::controller_bridge::status(context).await
            }
            commands::controller_bridge::ControllerCommand::Exec { service } => {
                commands::controller_bridge::exec(service).await
            }
        },
        Command::Restack(args) => commands::restack::run(context, args).await,
        Command::Hello { args } => commands::_hello::run(&context.config, args),
        Command::GhGuard { args } => commands::_gh_guard::run(context, args).await,
        Command::Snapshot { args } => commands::_snapshot::run(context, args).await,
        Command::Session { args } => commands::_session::run(context, args).await,
        Command::WorkerDispatch { args } => Box::pin(commands::_remote::run(context, args)).await,
        Command::Host => wt_host::host_server::run(context).await,
        Command::Version => {
            println!("wt {}", wt_app_core::build_info::native_version());
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
        Command::Managed { args } => commands::_managed::run(args, &context.cancellation).await,
    }
}

pub(crate) async fn dispatch_worker_args(
    context: &wt_app_core::context::AppContext,
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
