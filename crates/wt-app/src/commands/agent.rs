use std::{
    fs::File,
    io::{self, IsTerminal, Read},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use wt_harness::{ClaudeInjectFailureKind, ClaudeMessageOutcome, HarnessMessageOutcome};

use crate::{
    context::AppContext,
    harness::{
        AgentRoute, AgentTargetKind, AppHarness, SelectionSource, unavailable_source_message,
    },
};

const MAX_STDIN_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Args)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub command: AgentCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum AgentCommand {
    /// List addressable targets and selected/live harnesses.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Send text to the active harness, or the configured primary if none is live.
    Send {
        target: String,
        #[arg(long)]
        hold: Option<String>,
        #[arg(trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// Start an agent using the bundled start skill prompt.
    Start { target: String },
}

pub async fn run(context: &AppContext, args: &AgentArgs) -> Result<i32> {
    let app = AppHarness::new(context);
    let routes = app.routes(context).await?;
    if routes
        .iter()
        .any(|route| route.choice.source == SelectionSource::Unavailable)
    {
        bail!("could not inspect wt's tmux session registry; no harness was selected or started");
    }
    match &args.command {
        AgentCommand::Ls { json } => list(&routes, *json),
        AgentCommand::Send { target, hold, text } => {
            let mut text = read_message(text, &context.cancellation).await?;
            let Some(route) = AppHarness::target_for(target, &routes) else {
                if target == "wt"
                    && context
                        .config
                        .paths
                        .wt_source
                        .as_ref()
                        .is_none_or(|path| !path.is_dir())
                {
                    bail!("{}", unavailable_source_message());
                }
                bail!("unknown agent target: {target}; run `wt agent ls` to see available targets");
            };
            send_route(context, &app, route, &mut text, hold.as_deref()).await
        }
        AgentCommand::Start { target } => {
            let Some(route) = AppHarness::target_for(target, &routes) else {
                if target == "wt" && context.config.paths.wt_source.is_none() {
                    bail!("{}", unavailable_source_message());
                }
                bail!("unknown agent target: {target}");
            };
            if route.target.kind != AgentTargetKind::Worktree {
                bail!(
                    "wt agent start is worktree-only: {} is a special session",
                    route.target.slug
                );
            }
            let harness = route
                .choice
                .selected
                .context("harness selection is unavailable")?;
            let prompt = crate::skills::start_prompt(context, harness).await?;
            let mut body = prompt;
            send_route(context, &app, route, &mut body, None).await
        }
    }
}

pub async fn send_to(
    context: &AppContext,
    requested: &str,
    text: &str,
    hold: Option<&str>,
) -> Result<i32> {
    let app = AppHarness::new(context);
    let routes = app.routes(context).await?;
    if routes
        .iter()
        .any(|route| route.choice.source == SelectionSource::Unavailable)
    {
        bail!("could not inspect wt's tmux session registry; no harness was selected or started");
    }
    let Some(route) = AppHarness::target_for(requested, &routes) else {
        if requested == "wt"
            && context
                .config
                .paths
                .wt_source
                .as_ref()
                .is_none_or(|path| !path.is_dir())
        {
            bail!("{}", unavailable_source_message());
        }
        bail!("unknown agent target: {requested}; run `wt agent ls` to see available targets");
    };
    if route.target.remote {
        bail!(
            "remote agent messaging is not wired to the remote runtime yet; refusing local delivery to {}",
            route.target.slug
        );
    }
    let mut body = text.trim().to_owned();
    send_route(context, &app, route, &mut body, hold).await
}

async fn send_route(
    context: &AppContext,
    app: &AppHarness,
    route: &AgentRoute,
    body: &mut String,
    hold: Option<&str>,
) -> Result<i32> {
    if route.target.remote {
        bail!(
            "remote agent messaging is not wired to the remote runtime yet; refusing local delivery to {}",
            route.target.slug
        );
    }
    if body.is_empty() {
        bail!("message body is empty");
    }
    if let Some(id) = hold {
        *body = super::hold::prepare_hold_message(&super::hold::hold_path(context), id, body)?;
    }
    let sender = std::env::var("WT_AGENT")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let outcome = app.send(route, body, sender.as_deref(), context).await?;
    print_delivery(context, route, outcome)
}

fn list(routes: &[AgentRoute], json: bool) -> Result<i32> {
    if json {
        let value = routes
            .iter()
            .map(|route| {
                serde_json::json!({
                    "target": route.target.slug,
                    "kind": match route.target.kind { AgentTargetKind::Special => "special", AgentTargetKind::Worktree => "worktree" },
                    "branch": route.target.branch,
                    "cwd": route.target.cwd,
                    "active_harnesses": route.choice.live,
                    "selected_harness": route.choice.selected.map(|id| id.as_str()),
                    "selection": match route.choice.source { SelectionSource::Live => "live", SelectionSource::Primary => "primary", SelectionSource::Unavailable => "unavailable", SelectionSource::RemoteUnavailable => "remote-unavailable" },
                })
            })
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        for route in routes {
            let kind = if route.target.kind == AgentTargetKind::Special {
                " [special]"
            } else {
                ""
            };
            let harness = route
                .choice
                .selected
                .map(|id| id.as_str())
                .unwrap_or("unavailable");
            let reason = match route.choice.source {
                SelectionSource::Live
                    if route
                        .choice
                        .live
                        .as_ref()
                        .is_some_and(|live| live.len() > 1) =>
                {
                    "multiple live; primary preference"
                }
                SelectionSource::Live => "active session",
                SelectionSource::Primary => "configured primary; no active session",
                SelectionSource::Unavailable => "tmux inventory unavailable",
                SelectionSource::RemoteUnavailable => "remote runtime unavailable",
            };
            println!("{}{kind}  {harness}  ({reason})", route.target.slug);
        }
    }
    Ok(0)
}

async fn read_message(
    args: &[String],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String> {
    if !args.is_empty() {
        if args.len() == 1 && matches!(args[0].as_str(), "-" | "/dev/stdin" | "/dev/fd/0") {
            bail!(
                "{} is not a message body; omit it to read piped stdin",
                args[0]
            );
        }
        return Ok(args.join(" ").trim().to_owned());
    }
    let bytes = read_bounded_stdin(cancel).await?;
    if bytes.len() > MAX_STDIN_BYTES {
        bail!("message stdin exceeds the {MAX_STDIN_BYTES}-byte limit");
    }
    String::from_utf8(bytes)
        .context("message stdin is not UTF-8")
        .map(|text| text.trim().to_owned())
}

#[cfg(unix)]
async fn read_bounded_stdin(cancel: &tokio_util::sync::CancellationToken) -> Result<Vec<u8>> {
    if std::io::stdin().is_terminal() {
        return crate::prompt::read_line("Message: ", cancel)
            .await?
            .map(String::into_bytes)
            .context("message input cancelled");
    }
    // SAFETY: fstat only writes to this initialized stack value.
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    // SAFETY: stdin is a valid process descriptor and stat points to writable memory.
    if unsafe { libc::fstat(libc::STDIN_FILENO, &mut stat) } == -1 {
        return Err(io::Error::last_os_error()).context("inspect message stdin");
    }
    // Duplicate stdin so this reader owns its descriptor. A pipe/socket is
    // read through AsyncFd; regular files use bounded async fs work.
    // SAFETY: dup creates a new owned descriptor on success.
    let raw = unsafe { libc::dup(libc::STDIN_FILENO) };
    if raw == -1 {
        return Err(io::Error::last_os_error()).context("duplicate message stdin");
    }
    // SAFETY: raw is a new descriptor returned by dup and ownership transfers here.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    let file = File::from(owned);
    let file_type = stat.st_mode & libc::S_IFMT;
    if file_type == libc::S_IFREG {
        let task = tokio::task::spawn_blocking(move || {
            let mut bytes = Vec::new();
            file.take((MAX_STDIN_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            Ok::<_, io::Error>(bytes)
        });
        return tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("reading message stdin cancelled"),
            result = tokio::time::timeout(Duration::from_secs(30), task) => result
                .context("reading message stdin timed out")?
                .context("message stdin reader failed")?
                .context("read message stdin"),
        };
    }
    read_nonblocking(file, cancel).await
}

#[cfg(unix)]
async fn read_nonblocking(
    file: File,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<u8>> {
    use tokio::io::unix::AsyncFd;

    let fd = file.as_raw_fd();
    // `dup` shares file status flags with stdin. Record and restore O_NONBLOCK
    // through the duplicate before it closes so callers keep their flags.
    // SAFETY: fcntl F_GETFL reads status flags from the owned descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error()).context("read message stdin flags");
    }
    // SAFETY: fcntl F_SETFL applies status flags to the valid owned descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error()).context("make message stdin nonblocking");
    }
    let owned = NonblockingInput {
        file,
        original_flags: flags,
    };
    let async_fd = AsyncFd::new(owned).context("register message stdin")?;
    let mut output = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let mut ready = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("reading message stdin cancelled"),
            _ = tokio::time::sleep_until(deadline) => bail!("reading message stdin timed out"),
            ready = async_fd.readable() => ready.context("wait for message stdin")?,
        };
        let mut buffer = [0u8; 16 * 1024];
        let result = ready.try_io(|fd| {
            let count = unsafe {
                libc::read(
                    fd.get_ref().file.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if count == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        });
        let count = match result {
            Ok(Ok(count)) => count,
            Ok(Err(error)) => return Err(error).context("read message stdin"),
            Err(_) => continue,
        };
        if count == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..count]);
        if output.len() > MAX_STDIN_BYTES {
            bail!("message stdin exceeds the {MAX_STDIN_BYTES}-byte limit");
        }
    }
    Ok(output)
}

#[cfg(not(unix))]
async fn read_bounded_stdin(cancel: &tokio_util::sync::CancellationToken) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("reading message stdin cancelled"),
        result = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::task::spawn_blocking(move || std::io::stdin().take((MAX_STDIN_BYTES + 1) as u64).read_to_end(&mut bytes)).await
        }) => result.context("reading message stdin timed out")??.context("message stdin reader failed")?,
    }
    Ok(bytes)
}

#[cfg(unix)]
struct NonblockingInput {
    file: File,
    original_flags: i32,
}

#[cfg(unix)]
impl AsRawFd for NonblockingInput {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }
}

#[cfg(unix)]
impl Drop for NonblockingInput {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains open until the file field is dropped.
        unsafe {
            libc::fcntl(self.file.as_raw_fd(), libc::F_SETFL, self.original_flags);
        }
    }
}

fn print_delivery(
    context: &AppContext,
    route: &AgentRoute,
    outcome: HarnessMessageOutcome,
) -> Result<i32> {
    match outcome {
        HarnessMessageOutcome::Claude(ClaudeMessageOutcome::Sent {
            transport,
            cold_started,
            delivered,
            resent,
            fallback,
        }) => {
            println!(
                "sent to {} via Claude {:?} (cold_started={cold_started}, delivered={delivered:?}, resent={resent})",
                route.target.slug, transport
            );
            if let Some((level, advice)) = claude_fallback_advice(
                &context.config.paths.cache_root,
                fallback,
                std::env::var("WT_INSPECT").is_ok_and(|value| value.eq_ignore_ascii_case("off")),
            ) {
                println!("{level}: {advice}");
            }
            Ok(0)
        }
        HarnessMessageOutcome::Claude(ClaudeMessageOutcome::Failed {
            reason,
            maybe_submitted,
        }) => {
            bail!(
                "Claude delivery to {} failed (maybe_submitted={maybe_submitted}): {reason}",
                route.target.slug
            )
        }
        HarnessMessageOutcome::Codex(wt_harness::CodexMessageOutcome::Queued(delivery)) => {
            println!(
                "queued for {} in Codex ({:?}, reconciled={})",
                route.target.slug, delivery.state, delivery.reconciled
            );
            Ok(0)
        }
        HarnessMessageOutcome::Codex(wt_harness::CodexMessageOutcome::CliQueued { thread_id }) => {
            println!(
                "queued for {} in Codex thread {thread_id}",
                route.target.slug
            );
            Ok(0)
        }
        HarnessMessageOutcome::Codex(wt_harness::CodexMessageOutcome::Terminal {
            cold_started,
            delivered,
            reason,
        }) => {
            println!(
                "sent to {} through Codex terminal (cold_started={cold_started}, delivered={delivered:?}): {reason}",
                route.target.slug
            );
            Ok(0)
        }
        HarnessMessageOutcome::Codex(wt_harness::CodexMessageOutcome::NeedsTerminalFallback {
            reason,
        }) => {
            bail!(
                "Codex delivery to {} needs terminal fallback: {reason}",
                route.target.slug
            )
        }
        HarnessMessageOutcome::Codex(wt_harness::CodexMessageOutcome::Ambiguous { reason }) => {
            bail!(
                "Codex delivery to {} is ambiguous; do not retry because it may already be queued: {reason}",
                route.target.slug
            )
        }
        HarnessMessageOutcome::Codex(wt_harness::CodexMessageOutcome::Failed { reason }) => {
            bail!("Codex delivery to {} failed: {reason}", route.target.slug)
        }
        HarnessMessageOutcome::OpenCode(outcome) => {
            println!(
                "sent to {} through OpenCode (cold_started={})",
                route.target.slug, outcome.cold_started
            );
            Ok(0)
        }
    }
}

fn claude_fallback_advice(
    cache_root: &std::path::Path,
    fallback: Option<ClaudeInjectFailureKind>,
    inspector_disabled: bool,
) -> Option<(&'static str, String)> {
    let kind = fallback?;
    if inspector_disabled {
        return Some((
            "info",
            "Claude prompt injection was intentionally disabled by WT_INSPECT=off; terminal delivery was expected".into(),
        ));
    }
    if kind == ClaudeInjectFailureKind::Absent {
        let stale = wt_harness::stale_harness_shims(cache_root);
        if !stale.is_empty() {
            return Some((
                "warning",
                format!(
                    "stale {} harness shim(s) in {}; these can interfere with managed harness launches and may explain the missing Claude inspector socket. Remove them, start a new wt-managed session, then run `wt claude selftest`",
                    stale.join(", "),
                    cache_root.join("shims").display()
                ),
            ));
        }
    }
    let (level, advice) = match kind {
        ClaudeInjectFailureKind::Absent => (
            "info",
            "this Claude session has no inspector socket; start it from wt and run `wt claude selftest` if the problem persists".into(),
        ),
        ClaudeInjectFailureKind::Stale => (
            "info",
            "this session's inspector socket is stale; restart the session from wt, then run `wt claude selftest`".into(),
        ),
        ClaudeInjectFailureKind::NotReady => (
            "warning",
            "Claude's prompt was not reachable; run `wt claude selftest` to check whether its injector anchors changed".into(),
        ),
        ClaudeInjectFailureKind::Blocked => (
            "info",
            "Claude is waiting on a human; terminal fallback does not indicate an inspector failure".into(),
        ),
        ClaudeInjectFailureKind::SubmittedUnknown => (
            "warning",
            "the inspector submit may have been accepted; check the target transcript before retrying".into(),
        ),
        ClaudeInjectFailureKind::Failed => (
            "warning",
            "inspector delivery failed; run `wt claude selftest` if this repeats".into(),
        ),
    };
    Some((level, advice))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stdin_is_bounded_and_text_arguments_preserve_spaces() {
        let cancel = tokio_util::sync::CancellationToken::new();
        assert_eq!(
            read_message(&["a".into(), "b c".into()], &cancel)
                .await
                .unwrap(),
            "a b c"
        );
    }

    #[test]
    fn special_session_start_is_not_accepted_as_worktree_start() {
        let route = AgentRoute {
            target: crate::harness::AgentTarget {
                slug: "manager".into(),
                kind: AgentTargetKind::Special,
                branch: None,
                cwd: "/main".into(),
                managed_name: Some("manager".into()),
                remote: false,
            },
            choice: crate::harness::HarnessChoice {
                selected: Some(wt_core::HarnessId::Claude),
                source: SelectionSource::Primary,
                live: Some(vec![]),
            },
        };
        assert_eq!(route.target.kind, AgentTargetKind::Special);
    }

    #[test]
    fn fallback_advice_distinguishes_intentional_disable_and_stale_shims() {
        let cache = tempfile::tempdir().unwrap();
        let disabled =
            claude_fallback_advice(cache.path(), Some(ClaudeInjectFailureKind::Failed), true)
                .unwrap();
        assert_eq!(disabled.0, "info");
        assert!(disabled.1.contains("WT_INSPECT=off"));

        let shim_dir = cache.path().join("shims");
        std::fs::create_dir_all(&shim_dir).unwrap();
        std::fs::write(shim_dir.join("claude"), "stale").unwrap();
        let advice =
            claude_fallback_advice(cache.path(), Some(ClaudeInjectFailureKind::Absent), false)
                .unwrap();
        assert_eq!(advice.0, "warning");
        assert!(advice.1.contains(&shim_dir.display().to_string()));
        assert!(advice.1.contains("wt claude selftest"));

        let intentional =
            claude_fallback_advice(cache.path(), Some(ClaudeInjectFailureKind::Absent), true)
                .unwrap();
        assert_eq!(intentional.0, "info");
        assert!(intentional.1.contains("intentionally disabled"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_pipe_read_restores_flags_and_does_not_wait_for_eof() {
        use std::os::fd::{FromRawFd, OwnedFd};
        let mut pipe = [0; 2];
        // SAFETY: pipe writes two valid descriptors to the initialized array.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        // Keep the writer open: the reader can finish only when cancelled.
        // SAFETY: pipe returned distinct owned descriptors.
        let reader = File::from(unsafe { OwnedFd::from_raw_fd(pipe[0]) });
        let sentinel = unsafe { libc::dup(reader.as_raw_fd()) };
        assert_ne!(sentinel, -1);
        // SAFETY: dup returned an independently owned descriptor.
        let _sentinel = unsafe { OwnedFd::from_raw_fd(sentinel) };
        // SAFETY: the second pipe descriptor is independently owned.
        let _writer = unsafe { OwnedFd::from_raw_fd(pipe[1]) };
        let before = unsafe { libc::fcntl(sentinel, libc::F_GETFL) };
        let cancel = tokio_util::sync::CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move { read_nonblocking(reader, &task_cancel).await });
        for _ in 0..100 {
            if unsafe { libc::fcntl(sentinel, libc::F_GETFL) } & libc::O_NONBLOCK != 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_ne!(
            unsafe { libc::fcntl(sentinel, libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        cancel.cancel();
        let error = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("cancelled read should finish without pipe EOF")
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(unsafe { libc::fcntl(sentinel, libc::F_GETFL) }, before);
    }
}
