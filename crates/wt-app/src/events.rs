//! GitHub webhook daemon composition and per-user launchd ownership.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;
use wt_config::Config;
use wt_events::{EventDaemonConfig, EventFuture, EventGithubData, EventSource};
use wt_github::{GithubClient, GithubOptions};
use wt_platform::process::CommandSpec;

use crate::context::AppContext;

const EVENT_LOCK: &str = "events-agent";
const LAUNCHD_LABEL: &str = "com.wt.events";

#[derive(Clone)]
struct AppEventSource {
    context: AppContext,
}

impl EventSource for AppEventSource {
    fn local_branches<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> EventFuture<'a, Result<Vec<String>, String>> {
        Box::pin(async move {
            let inventory = self
                .context
                .repository
                .inventory(cancel)
                .await
                .map_err(|error| error.to_string())?;
            Ok(inventory
                .into_iter()
                .filter(|row| !row.is_main)
                .map(|row| row.target.branch)
                .filter(|branch| !branch.is_empty())
                .collect())
        })
    }

    fn remote_branches<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> EventFuture<'a, Result<Option<Vec<String>>, String>> {
        Box::pin(async move {
            let Some(remote) = self.context.config.remote.clone() else {
                return Ok(None);
            };
            let snapshot = wt_remote::RemoteClient::new(self.context.processes.clone(), remote)
                .snapshot(cancel)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Some(
                snapshot
                    .worktrees
                    .into_iter()
                    .filter(|row| row.exists)
                    .map(|row| row.branch)
                    .filter(|branch| !branch.is_empty())
                    .collect(),
            ))
        })
    }

    fn fetch_origin<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> EventFuture<'a, Result<(), String>> {
        Box::pin(async move {
            crate::origin::refresh(&self.context, cancel)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }

    fn fetch_github<'a>(
        &'a self,
        branches: Vec<String>,
        cancel: &'a CancellationToken,
    ) -> EventFuture<'a, Result<EventGithubData, String>> {
        Box::pin(async move {
            if branches.is_empty() {
                return Ok(EventGithubData {
                    prs: serde_json::json!({}),
                    merge_queue: serde_json::json!({}),
                });
            }
            let has_ci = tokio::fs::read_dir(
                self.context
                    .config
                    .paths
                    .main_clone
                    .join(".github/workflows"),
            )
            .await
            .is_ok();
            let github = GithubClient::new(
                self.context.processes.clone(),
                self.context.config.paths.main_clone.clone(),
                GithubOptions::from_config(&self.context.config, has_ci),
            );
            let data = github
                .fetch_worktrees(&branches, cancel)
                .await
                .map_err(|error| error.to_string())?;
            Ok(EventGithubData {
                prs: serde_json::to_value(data.prs).map_err(|error| error.to_string())?,
                merge_queue: serde_json::to_value(data.merge_queue)
                    .map_err(|error| error.to_string())?,
            })
        })
    }
}

pub async fn serve(context: &AppContext) -> Result<()> {
    let events = context
        .config
        .github
        .events
        .as_ref()
        .context("[github.events] is not configured")?;
    let secret = resolve_secret(&context.config).await?;
    let config = EventDaemonConfig::new(
        events.host.clone(),
        events.port,
        secret,
        context.config.paths.cache_root.join("events"),
        context.config.branch.base.clone(),
        Some(env!("WT_BUILD_ID").to_owned()),
    );
    let daemon = wt_events::start_daemon(
        config,
        Arc::new(AppEventSource {
            context: context.clone(),
        }),
        context.cancellation.clone(),
    )
    .await?;
    tracing::info!(address = %daemon.local_addr(), "GitHub events daemon is listening");
    context.cancellation.cancelled().await;
    daemon.shutdown().await;
    Ok(())
}

pub async fn reconcile_at_startup(context: &AppContext) -> Result<()> {
    // Disabled integrations and foreign/unknown launch agents are left alone.
    if context.config.github.events.is_none() {
        return Ok(());
    }
    let plist = plist_path(&context.home);
    if !tokio::fs::try_exists(&plist).await.unwrap_or(false) {
        return Ok(());
    }
    let lock = wt_platform::lock::FileLock::acquire(
        &context.home.join(".cache/wt"),
        EVENT_LOCK,
        "reconcile GitHub events agent",
        &context.cancellation,
    )
    .await?;
    let owns = owns_agent(context, &plist).await?;
    if !owns {
        return Ok(());
    }
    let _lock = lock;
    let events_dir = events_directory(&context.config);
    let prior_state = wt_events::read_state(&events_dir).await.ok().flatten();
    if prior_state.as_ref().is_some_and(|state| {
        state.writer_sha.as_deref() == Some(env!("WT_BUILD_ID")) && state.pid.is_some_and(pid_alive)
    }) {
        return Ok(());
    }
    let prior_pid = prior_state.and_then(|state| state.pid);
    unload_agent(context, &plist).await?;
    install_agent_locked(context, &plist).await?;
    let result = launchctl(context, "load", &plist).await?;
    if !result.status.success() {
        bail!(
            "could not reload GitHub events launch agent: {}",
            result.stderr_text().trim()
        )
    }
    wait_for_new_daemon(context, prior_pid, Duration::from_secs(10)).await
}

pub fn events_directory(config: &Config) -> PathBuf {
    config.paths.cache_root.join("events")
}

async fn resolve_secret(config: &Config) -> Result<String> {
    let events = config
        .github
        .events
        .as_ref()
        .context("[github.events] is not configured")?;
    if let Some(secret) = &events.secret {
        return Ok(secret.clone());
    }
    if let Some(path) = &events.secret_file {
        let value = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read GitHub webhook secret file {path}"))?;
        let secret = value.trim();
        if !secret.is_empty() {
            return Ok(secret.to_owned());
        }
    }
    bail!(
        "GitHub events webhook secret is missing; run `wt events secret` and configure [github.events].secret_file"
    )
}

pub(crate) async fn lock_user_events(context: &AppContext) -> Result<wt_platform::lock::FileLock> {
    Ok(wt_platform::lock::FileLock::acquire(
        &context.home.join(".cache/wt"),
        EVENT_LOCK,
        "manage GitHub events agent",
        &context.cancellation,
    )
    .await?)
}

pub(crate) async fn launchctl(
    context: &AppContext,
    action: &str,
    plist: &Path,
) -> Result<wt_platform::process::ProcessOutput> {
    if !cfg!(target_os = "macos") {
        bail!("GitHub events launchd management is supported only on macOS");
    }
    let mut spec = CommandSpec::new("launchctl")
        .args([action, "-w"])
        .args([plist.as_os_str()]);
    spec.timeout = Duration::from_secs(20);
    context
        .processes
        .run(spec, &context.cancellation)
        .await
        .context("run launchctl")
}

/// Stop an owned launch agent before changing or removing its plist.
/// `launchctl unload` may fail when the job was already absent, so its exit
/// status alone is not enough to decide whether it is safe to proceed. Query
/// launchd's job table and continue only after the exact label is absent and
/// every PID recorded for the owned daemon before unload has exited.
async fn unload_agent(context: &AppContext, plist: &Path) -> Result<()> {
    let prior_launchd = launch_agent_status(context).await?;
    let prior_state = wt_events::read_state(&events_directory(&context.config))
        .await
        .context("read GitHub events daemon state before unload")?;
    let mut known_pids = Vec::new();
    if let Some(pid) = prior_launchd.pid {
        known_pids.push(pid);
    }
    if let Some(pid) = prior_state.and_then(|state| state.pid)
        && !known_pids.contains(&pid)
    {
        known_pids.push(pid);
    }

    let unload = launchctl(context, "unload", plist).await;
    let unload_error = match unload {
        Ok(result) if result.status.success() => None,
        Ok(result) => Some(format!(
            "launchctl unload failed: {}",
            result.stderr_text().trim()
        )),
        Err(error) => Some(format!("launchctl unload could not run: {error:#}")),
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match launch_agent_status(context).await {
            Ok(status) if !status.loaded && known_pids.iter().all(|pid| !pid_alive(*pid)) => {
                return Ok(());
            }
            Ok(status) if unload_error.is_some() && status.loaded => {
                let detail = unload_error
                    .as_deref()
                    .unwrap_or("launchctl unload returned success, but the job remains loaded");
                bail!(
                    "could not stop GitHub events launch agent; {detail}. The plist was left in place at {}. Check `launchctl list` and retry `wt events uninstall` or `wt events restart`",
                    plist.display()
                );
            }
            Ok(status) if tokio::time::Instant::now() >= deadline => {
                let mut pending = known_pids
                    .iter()
                    .copied()
                    .filter(|pid| pid_alive(*pid))
                    .collect::<Vec<_>>();
                pending.sort_unstable();
                pending.dedup();
                let detail = if status.loaded {
                    "launchd still lists the job".to_owned()
                } else {
                    format!("recorded daemon PID(s) are still alive: {pending:?}")
                };
                bail!(
                    "could not stop GitHub events launch agent; {detail}. The plist was left in place at {}. Check `launchctl list` and retry `wt events uninstall` or `wt events restart`",
                    plist.display()
                );
            }
            Ok(_) => {}
            Err(error) => bail!(
                "could not confirm GitHub events launch agent is unloaded: {error:#}. The plist was left in place at {}",
                plist.display()
            ),
        }
        tokio::select! {
            _ = context.cancellation.cancelled() => bail!(
                "cancelled before confirming GitHub events launch agent was unloaded; the plist was left in place at {}",
                plist.display()
            ),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
}

#[derive(Default)]
struct LaunchAgentStatus {
    loaded: bool,
    pid: Option<u32>,
}

async fn launch_agent_status(context: &AppContext) -> Result<LaunchAgentStatus> {
    if !cfg!(target_os = "macos") {
        bail!("GitHub events launchd management is supported only on macOS");
    }
    let mut spec = CommandSpec::new("launchctl").args(["list"]);
    spec.timeout = Duration::from_secs(20);
    spec.output_limit = 1024 * 1024;
    let result = context
        .processes
        .run(spec, &context.cancellation)
        .await
        .context("query launchd job state")?;
    if !result.status.success() {
        bail!("launchctl list failed: {}", result.stderr_text().trim());
    }
    if result.stdout_truncated {
        bail!("launchctl list output was truncated; job state is unknown");
    }
    let output = result.stdout_text();
    let mut lines = output.lines().filter(|line| !line.trim().is_empty());
    let header = lines
        .next()
        .context("launchctl list returned no job table; job state is unknown")?;
    if header.split_whitespace().collect::<Vec<_>>() != ["PID", "Status", "Label"] {
        bail!("launchctl list returned an unrecognized job table; job state is unknown");
    }
    let mut status = LaunchAgentStatus::default();
    for line in lines {
        let columns = line.split_whitespace().collect::<Vec<_>>();
        if columns.len() != 3 {
            bail!("launchctl list returned an unrecognized job row; job state is unknown");
        }
        let pid = if columns[0] == "-" {
            None
        } else {
            Some(
                columns[0]
                    .parse::<u32>()
                    .context("launchctl list returned an invalid PID; job state is unknown")?,
            )
        };
        if columns[1] != "-" {
            columns[1]
                .parse::<i32>()
                .context("launchctl list returned an invalid status; job state is unknown")?;
        }
        if columns[2] == LAUNCHD_LABEL {
            if status.loaded {
                bail!("launchctl list returned duplicate GitHub events job rows");
            }
            status.loaded = true;
            status.pid = pid;
        }
    }
    Ok(status)
}

pub(crate) async fn install_agent(context: &AppContext) -> Result<()> {
    ensure_configured(context)?;
    let _lock = lock_user_events(context).await?;
    let plist = plist_path(&context.home);
    if tokio::fs::try_exists(&plist).await? && !owns_agent(context, &plist).await? {
        bail!(
            "events launch agent already exists but its repository ownership is unknown or belongs elsewhere; leaving it unchanged"
        )
    }
    if !secret_is_configured(context).await {
        ensure_secret_for_install(context).await?;
        if !secret_is_configured(context).await {
            bail!(
                "a persistent webhook secret is required before installing the agent; set [github.events].secret or secret_file"
            )
        }
    }
    let events_dir = events_directory(&context.config);
    tokio::fs::create_dir_all(&events_dir).await?;
    tokio::fs::create_dir_all(plist.parent().unwrap()).await?;
    let xml = plist_contents(context)?;
    write_atomic(&plist, xml.as_bytes()).await?;
    println!("installed launchd agent at {}", plist.display());
    Ok(())
}

pub(crate) async fn ensure_secret_for_install(context: &AppContext) -> Result<Option<String>> {
    let events = context
        .config
        .github
        .events
        .as_ref()
        .context("[github.events] is not configured")?;
    if let Some(secret) = &events.secret {
        return Ok(Some(secret.clone()));
    }
    if let Some(path) = &events.secret_file {
        match tokio::fs::read_to_string(path).await {
            Ok(value) if !value.trim().is_empty() => return Ok(Some(value.trim().to_owned())),
            Ok(_) => bail!(
                "configured webhook secret file {path} is empty; refusing to replace it automatically"
            ),
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error)
                    .with_context(|| format!("read configured webhook secret file {path}"));
            }
            Err(_) => {}
        }
    }
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).context("generate webhook secret")?;
    let secret = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    if let Some(path) = &events.secret_file {
        if let Some(parent) = Path::new(path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        match write_secret(path, format!("{secret}\n").as_bytes()).await {
            Ok(()) => Ok(Some(secret)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                let stored = tokio::fs::read_to_string(path)
                    .await
                    .with_context(|| format!("read concurrently created webhook secret {path}"))?;
                if stored.trim().is_empty() {
                    bail!("configured webhook secret file {path} is empty");
                }
                Ok(Some(stored.trim().to_owned()))
            }
            Err(error) => Err(error),
        }
    } else {
        println!("Add this to [github.events] in your config.toml: secret = \"{secret}\"");
        Ok(Some(secret))
    }
}

pub(crate) async fn secret_is_configured(context: &AppContext) -> bool {
    resolve_secret(&context.config).await.is_ok()
}

pub(crate) async fn status(context: &AppContext) -> Result<()> {
    let events = context.config.github.events.as_ref();
    let dir = events_directory(&context.config);
    let state = wt_events::read_state(&dir)
        .await
        .context("read GitHub events daemon state")?;
    let snapshot = wt_events::read_snapshot(&dir)
        .await
        .context("read GitHub events snapshot")?;
    if events.is_none() {
        println!("[github.events] not configured");
        return Ok(());
    }
    let state_alive = state.as_ref().and_then(|s| s.pid).is_some_and(pid_alive);
    let config = events.unwrap();
    println!("GitHub events daemon");
    println!(
        "  status       {}",
        if state_alive {
            "running"
        } else {
            "not running"
        }
    );
    println!("  bind         {}:{}", config.host, config.port);
    println!(
        "  secret       {}",
        if resolve_secret(&context.config).await.is_ok() {
            "set"
        } else {
            "missing"
        }
    );
    if let Some(state) = state {
        println!(
            "  pid          {}",
            state.pid.map_or("unknown".into(), |p| p.to_string())
        );
        println!("  events       {}", state.event_count);
        println!(
            "  started      {}",
            state.started_at.map_or("unknown".into(), ago)
        );
        println!(
            "  last fetch   {}",
            state.last_fetch_at.map_or("never".into(), ago)
        );
        if state.pid.is_some_and(pid_alive)
            && state.writer_sha.as_deref() != Some(env!("WT_BUILD_ID"))
        {
            println!("  build        stale; run `wt events restart`");
        }
        if let Some(error) = state.last_error {
            println!("  last error   {error}");
        }
    }
    match snapshot {
        Some(snapshot) => {
            println!(
                "  snapshot     {} PRs, written {}",
                snapshot.prs.as_object().map_or(0, serde_json::Map::len),
                ago(snapshot.updated_at)
            );
            if snapshot.writer_sha.as_deref() != Some(env!("WT_BUILD_ID")) {
                println!("  snapshot build stale; live GitHub reads will be used until refreshed");
            }
        }
        None => println!("  snapshot     none"),
    }
    Ok(())
}

pub(crate) async fn agent_mutation(context: &AppContext, action: &str) -> Result<i32> {
    let _lock = lock_user_events(context).await?;
    let plist = plist_path(&context.home);
    if action == "install" {
        install_agent_locked(context, &plist).await?;
        return Ok(0);
    }
    if !tokio::fs::try_exists(&plist).await? {
        bail!(
            "no launchd agent at {}; run `wt events install` first",
            plist.display()
        );
    }
    if !owns_agent(context, &plist).await? {
        bail!("events launch agent belongs to another configuration or its ownership is unknown");
    }
    if action == "uninstall" {
        unload_agent(context, &plist).await?;
        tokio::fs::remove_file(&plist).await?;
        println!("removed {}", plist.display());
        return Ok(0);
    }
    let old_pid = if action == "restart" {
        wt_events::read_state(&events_directory(&context.config))
            .await
            .ok()
            .flatten()
            .and_then(|state| state.pid)
    } else {
        None
    };
    if action == "restart" {
        unload_agent(context, &plist).await?;
    }
    if action == "load" || action == "restart" {
        install_agent_locked(context, &plist).await?;
    }
    let verb = if action == "restart" { "load" } else { action };
    let result = launchctl(context, verb, &plist).await?;
    if !result.status.success() {
        bail!("launchctl {verb} failed: {}", result.stderr_text().trim());
    }
    if action == "restart" {
        wait_for_new_daemon(context, old_pid, Duration::from_secs(10)).await?;
    }
    println!(
        "{} {}",
        if verb == "load" { "started" } else { "stopped" },
        LAUNCHD_LABEL
    );
    Ok(0)
}

async fn install_agent_locked(context: &AppContext, plist: &Path) -> Result<()> {
    ensure_configured(context)?;
    tokio::fs::create_dir_all(events_directory(&context.config)).await?;
    ensure_secret_for_install(context).await?;
    if !secret_is_configured(context).await {
        bail!(
            "a persistent webhook secret is required before installing the agent; set [github.events].secret or secret_file"
        )
    }
    tokio::fs::create_dir_all(plist.parent().unwrap()).await?;
    write_atomic(plist, plist_contents(context)?.as_bytes()).await?;
    Ok(())
}

async fn wait_for_new_daemon(
    context: &AppContext,
    prior_pid: Option<u32>,
    timeout: Duration,
) -> Result<()> {
    let events_dir = events_directory(&context.config);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let state = wt_events::read_state(&events_dir).await.ok().flatten();
        if state.as_ref().is_some_and(|state| {
            state.writer_sha.as_deref() == Some(env!("WT_BUILD_ID"))
                && state
                    .pid
                    .is_some_and(|pid| Some(pid) != prior_pid && pid_alive(pid))
        }) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "events daemon did not become ready within {}s",
                timeout.as_secs()
            );
        }
        tokio::select! {
            _ = context.cancellation.cancelled() => bail!("events daemon wait cancelled"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
}

async fn owns_agent(context: &AppContext, plist: &Path) -> Result<bool> {
    let mut spec = CommandSpec::new("plutil")
        .args(["-convert", "json", "-o", "-"])
        .args([plist.as_os_str()]);
    spec.timeout = Duration::from_secs(5);
    let output = context.processes.run(spec, &context.cancellation).await?;
    if !output.status.success() {
        return Ok(false);
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    let env = parsed
        .get("EnvironmentVariables")
        .and_then(serde_json::Value::as_object);
    let expected = config_environment(context);
    let Some(env) = env else {
        return Ok(false);
    };
    let repo = env
        .get("WT_REPO_CONFIG")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let global = env
        .get("WT_CONFIG")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            env.get("XDG_CONFIG_HOME")
                .and_then(serde_json::Value::as_str)
                .map(|p| format!("{p}/wt/config.toml"))
        })
        .or_else(|| {
            env.get("HOME")
                .and_then(serde_json::Value::as_str)
                .map(|p| format!("{p}/.config/wt/config.toml"))
        });
    let expected_dir = events_directory(&context.config);
    Ok(canonical_optional(repo, &context.cwd) == expected.1
        && global.is_some_and(|path| canonical_optional(&path, &context.cwd) == expected.0)
        && parsed
            .get("StandardOutPath")
            .and_then(serde_json::Value::as_str)
            == Some(
                expected_dir
                    .join("daemon.out.log")
                    .to_string_lossy()
                    .as_ref(),
            )
        && parsed
            .get("StandardErrorPath")
            .and_then(serde_json::Value::as_str)
            == Some(
                expected_dir
                    .join("daemon.err.log")
                    .to_string_lossy()
                    .as_ref(),
            ))
}

fn plist_contents(context: &AppContext) -> Result<String> {
    let events = context
        .config
        .github
        .events
        .as_ref()
        .context("[github.events] is not configured")?;
    let _ = events;
    let root = std::env::var_os("WT_INSTALL_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| context.home.join(".local/share/wt"));
    let launcher = root
        .join("bin")
        .join(if cfg!(windows) { "wt.exe" } else { "wt" });
    if !launcher.is_file() {
        bail!(
            "native stable launcher is missing at {}; install wt before installing the GitHub events agent",
            launcher.display()
        );
    }
    let (global, repo) = config_environment(context);
    let mut env = vec![
        ("HOME", context.home.to_string_lossy().into_owned()),
        (
            "PATH",
            std::env::var("PATH")
                .unwrap_or_else(|_| "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin".into()),
        ),
        ("WT_CONFIG", global),
    ];
    env.push(("WT_REPO_CONFIG", repo));
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        env.push(("XDG_CONFIG_HOME", xdg.to_string_lossy().into_owned()));
    }
    let dir = events_directory(&context.config);
    let arguments = [
        launcher.to_string_lossy().into_owned(),
        "events".into(),
        "serve".into(),
    ];
    let arg_lines = arguments
        .iter()
        .map(|arg| format!("    <string>{}</string>", xml_escape(arg)))
        .collect::<Vec<_>>()
        .join("\n");
    let env_lines = env
        .iter()
        .map(|(key, value)| {
            format!(
                "    <key>{}</key>\n    <string>{}</string>",
                xml_escape(key),
                xml_escape(value)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LAUNCHD_LABEL}</string>\n<key>ProgramArguments</key><array>\n{arg_lines}\n</array>\n<key>EnvironmentVariables</key><dict>\n{env_lines}\n</dict>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n<key>StandardOutPath</key><string>{}</string>\n<key>StandardErrorPath</key><string>{}</string>\n</dict></plist>\n",
        xml_escape(&dir.join("daemon.out.log").to_string_lossy()),
        xml_escape(&dir.join("daemon.err.log").to_string_lossy())
    ))
}

fn config_environment(context: &AppContext) -> (String, String) {
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| context.home.join(".config"));
    let global = std::env::var_os("WT_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg.join("wt/config.toml"));
    let repo = std::env::var_os("WT_REPO_CONFIG").map(PathBuf::from);
    (
        canonical_path(&global, &context.cwd),
        repo.as_deref()
            .map_or(String::new(), |path| canonical_path(path, &context.cwd)),
    )
}

fn canonical_path(path: &Path, cwd: &Path) -> String {
    let expanded = if let Ok(rest) = path.strip_prefix("~") {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest)
    } else if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    std::fs::canonicalize(&expanded)
        .unwrap_or(expanded)
        .to_string_lossy()
        .into_owned()
}

fn canonical_optional(value: &str, cwd: &Path) -> String {
    if value.is_empty() {
        String::new()
    } else {
        canonical_path(Path::new(value), cwd)
    }
}

fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

fn ensure_configured(context: &AppContext) -> Result<()> {
    context
        .config
        .github
        .events
        .as_ref()
        .context("[github.events] is not configured")?;
    Ok(())
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("agent path has no parent")?;
    tokio::fs::create_dir_all(parent).await?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

#[cfg(unix)]
async fn write_secret(path: &str, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .await?;
    use tokio::io::AsyncWriteExt;
    let mut file = file;
    file.write_all(bytes).await?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .await?;
    Ok(())
}

#[cfg(not(unix))]
async fn write_secret(path: &str, bytes: &[u8]) -> Result<()> {
    tokio::fs::write(path, bytes)
        .await
        .context("write webhook secret")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as libc::pid_t, 0) == 0
                || std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn ago(timestamp: u64) -> String {
    let seconds = (now_millis().saturating_sub(timestamp)) / 1_000;
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_xml_escaping_handles_path_specials() {
        assert_eq!(xml_escape("a&b<c>\"'"), "a&amp;b&lt;c&gt;&quot;&apos;");
    }
}
