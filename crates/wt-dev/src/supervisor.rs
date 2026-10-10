use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use thiserror::Error;
use tokio::{
    fs,
    process::{Child, Command},
    signal::unix::{SignalKind, signal},
};
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessError, ProcessRunner};
use wt_tmux::{PaneTarget, TmuxClient, TmuxError};

const ESTABLISHED_AFTER: Duration = Duration::from_secs(300);
const DETERMINISTIC_FAILURE_BEFORE: Duration = Duration::from_secs(10);
const GIVE_UP_AFTER: u32 = 3;
const RESTART_INITIAL: Duration = Duration::from_secs(2);
const RESTART_CEILING: Duration = Duration::from_secs(60);
const TERMINATE_GRACE: Duration = Duration::from_secs(5);
const HOOK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const LOG_BYTE_LIMIT: usize = 1_000_000;
const RESTARTS_BYTE_LIMIT: usize = 128;

#[derive(Clone)]
pub struct SupervisorConfig {
    pub slug: String,
    pub path: PathBuf,
    pub command: String,
    pub stop_command: Option<String>,
    pub port: u16,
    pub dev_dir: PathBuf,
    pub tmux: TmuxClient,
    pub processes: ProcessRunner,
    pub shell: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorExit {
    Stopped,
    Crashed,
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("dev supervisor I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("dev supervisor process: {0}")]
    Process(#[from] ProcessError),
    #[error("dev supervisor tmux: {0}")]
    Tmux(#[from] TmuxError),
    #[error("dev supervisor spawn: {0}")]
    Spawn(#[source] io::Error),
    #[error("dev supervisor signal setup: {0}")]
    Signal(#[source] io::Error),
    #[error("dev supervisor child has no process id")]
    MissingPid,
    #[error("dev supervisor cancelled")]
    Cancelled,
}

pub async fn run_supervisor(
    config: SupervisorConfig,
    cancellation: CancellationToken,
) -> Result<SupervisorExit, SupervisorError> {
    let mut interrupt = signal(SignalKind::interrupt()).map_err(SupervisorError::Signal)?;
    let mut terminate = signal(SignalKind::terminate()).map_err(SupervisorError::Signal)?;
    let mut hangup = signal(SignalKind::hangup()).map_err(SupervisorError::Signal)?;
    fs::create_dir_all(&config.dev_dir)
        .await
        .map_err(|source| SupervisorError::Io {
            path: config.dev_dir.clone(),
            source,
        })?;
    let marker = config.dev_dir.join(format!("{}.state", config.slug));
    let attempts_path = config
        .dev_dir
        .join(format!("{}.state.attempts", config.slug));
    let crash_log = config.dev_dir.join(format!("{}.crash.log", config.slug));
    let mut failures = 0_u32;
    let mut previous_signature: Option<String> = None;
    let mut delay = RESTART_INITIAL;

    loop {
        if cancellation.is_cancelled() {
            write_bounded(&marker, b"stopped", 32).await?;
            release_session(&config).await?;
            return Err(SupervisorError::Cancelled);
        }
        write_bounded(&marker, b"running", 32).await?;
        let started = Instant::now();
        let mut child = spawn_dev_command(&config)?;
        let child_pid = child.id().ok_or(SupervisorError::MissingPid)?;
        let child_result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                terminate_child(&mut child).await?;
                write_bounded(&marker, b"stopped", 32).await?;
                release_session(&config).await?;
                return Err(SupervisorError::Cancelled);
            }
            _ = interrupt.recv() => {
                terminate_child(&mut child).await?;
                write_bounded(&marker, b"stopped", 32).await?;
                release_session(&config).await?;
                return Ok(SupervisorExit::Stopped);
            }
            _ = terminate.recv() => {
                terminate_child(&mut child).await?;
                write_bounded(&marker, b"stopped", 32).await?;
                release_session(&config).await?;
                return Ok(SupervisorExit::Stopped);
            }
            _ = hangup.recv() => {
                terminate_child(&mut child).await?;
                write_bounded(&marker, b"stopped", 32).await?;
                release_session(&config).await?;
                return Ok(SupervisorExit::Stopped);
            }
            result = child.wait() => result.map_err(SupervisorError::Spawn)?,
        };
        terminate_orphaned_group(child_pid).await;
        let elapsed = started.elapsed();
        let exit = child_result.code().unwrap_or(1);
        if matches!(exit, 130 | 143) {
            write_bounded(&marker, b"stopped", 32).await?;
            release_session(&config).await?;
            return Ok(SupervisorExit::Stopped);
        }

        if elapsed < ESTABLISHED_AFTER {
            failures = failures.saturating_add(1);
        } else {
            failures = 0;
            delay = RESTART_INITIAL;
            previous_signature = None;
        }
        let pane_output = config
            .tmux
            .capture_pane(
                &PaneTarget::active_session_pane(format!("{}-dev", config.slug)),
                Some(2000),
                &cancellation,
            )
            .await
            .unwrap_or_default();
        let signature = failure_signature(exit, &pane_output);
        if elapsed < DETERMINISTIC_FAILURE_BEFORE
            && signature.is_some()
            && signature == previous_signature
        {
            failures = GIVE_UP_AFTER;
            println!("wt: same failure twice in a row (exit {exit}) — not retrying.");
        }
        previous_signature = signature;
        write_bounded(
            &attempts_path,
            format!("{failures} {exit}").as_bytes(),
            RESTARTS_BYTE_LIMIT,
        )
        .await?;

        if failures >= GIVE_UP_AFTER {
            write_bounded(&marker, b"crashed", 32).await?;
            let summary = useful_crash_output(&pane_output);
            if !summary.is_empty() {
                write_bounded(&crash_log, summary.as_bytes(), LOG_BYTE_LIMIT).await?;
            }
            run_stop_command(&config, &cancellation).await?;
            // The saved pane is the diagnostic source; ending this session
            // releases its capacity slot and leaves `crashed` in durable cache.
            let _ = config
                .tmux
                .kill_session(&format!("{}-dev", config.slug), &cancellation)
                .await;
            return Ok(SupervisorExit::Crashed);
        }

        println!(
            "wt: dev server exited ({exit}) after {}s — restarting in {}s (attempt {} of {GIVE_UP_AFTER})",
            elapsed.as_secs(),
            delay.as_secs(),
            failures + 1
        );
        tokio::select! {
            _ = cancellation.cancelled() => {
                write_bounded(&marker, b"stopped", 32).await?;
                release_session(&config).await?;
                return Err(SupervisorError::Cancelled);
            }
            _ = interrupt.recv() => {
                write_bounded(&marker, b"stopped", 32).await?;
                release_session(&config).await?;
                return Ok(SupervisorExit::Stopped);
            }
            _ = terminate.recv() => {
                write_bounded(&marker, b"stopped", 32).await?;
                release_session(&config).await?;
                return Ok(SupervisorExit::Stopped);
            }
            _ = hangup.recv() => {
                write_bounded(&marker, b"stopped", 32).await?;
                release_session(&config).await?;
                return Ok(SupervisorExit::Stopped);
            }
            _ = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(RESTART_CEILING);
    }
}

async fn release_session(config: &SupervisorConfig) -> Result<(), SupervisorError> {
    config
        .tmux
        .kill_session(&format!("{}-dev", config.slug), &CancellationToken::new())
        .await?;
    Ok(())
}

fn spawn_dev_command(config: &SupervisorConfig) -> Result<Child, SupervisorError> {
    let mut command = Command::new(&config.shell);
    command
        .args(["-lc", config.command.as_str()])
        .current_dir(&config.path)
        .env("PORT", config.port.to_string())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(false);
    #[cfg(unix)]
    command.process_group(0);
    command.spawn().map_err(SupervisorError::Spawn)
}

async fn terminate_child(child: &mut Child) -> Result<(), SupervisorError> {
    let Some(pid) = child.id() else {
        return child
            .wait()
            .await
            .map(|_| ())
            .map_err(SupervisorError::Spawn);
    };
    signal_group(pid, libc::SIGTERM);
    if tokio::time::timeout(TERMINATE_GRACE, child.wait())
        .await
        .is_err()
    {
        signal_group(pid, libc::SIGKILL);
        child.wait().await.map_err(SupervisorError::Spawn)?;
    }
    terminate_orphaned_group(pid).await;
    Ok(())
}

/// The command leader may exit while a background descendant keeps the
/// isolated process group alive. Reap the leader above, then terminate any
/// remaining members before the supervisor retries or declares a crash.
async fn terminate_orphaned_group(pid: u32) {
    #[cfg(unix)]
    {
        if !group_exists(pid) {
            return;
        }
        signal_group(pid, libc::SIGTERM);
        let deadline = Instant::now() + TERMINATE_GRACE;
        while Instant::now() < deadline && group_exists(pid) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if group_exists(pid) {
            signal_group(pid, libc::SIGKILL);
            let deadline = Instant::now() + TERMINATE_GRACE;
            while Instant::now() < deadline && group_exists(pid) {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(unix)]
fn group_exists(pid: u32) -> bool {
    // SAFETY: signal zero only probes the isolated child's process group.
    if unsafe { libc::kill(-(pid as i32), 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) {
    // SAFETY: negative pid targets the isolated child process group created
    // specifically for this supervised command.
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _signal: i32) {}

async fn run_stop_command(
    config: &SupervisorConfig,
    cancellation: &CancellationToken,
) -> Result<(), SupervisorError> {
    let Some(command) = config.stop_command.as_deref() else {
        return Ok(());
    };
    let command = command
        .replace("{{slug}}", &config.slug)
        .replace("{{path}}", &config.path.to_string_lossy())
        .replace("{{port}}", &config.port.to_string());
    let mut spec = CommandSpec::new(&config.shell)
        .args(["-lc", command.as_str()])
        .cwd(&config.path);
    spec.timeout = HOOK_TIMEOUT;
    spec.env
        .push(("PORT".into(), Some(config.port.to_string().into())));
    let result = config.processes.run(spec, cancellation).await?;
    if !result.status.success() {
        eprintln!(
            "wt: stop_command failed after dev server crash (exit {:?}): {}",
            result.status.code(),
            first_line(&result.stderr_text())
                .or_else(|| first_line(&result.stdout_text()))
                .unwrap_or_default(),
        );
    }
    Ok(())
}

fn failure_signature(exit: i32, output: &str) -> Option<String> {
    let last = output
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty() && !line.starts_with("wt:"))?;
    Some(format!("{exit}|{last}"))
}

pub(crate) fn useful_crash_output(output: &str) -> String {
    let text = output
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with("wt:"))
        .collect::<Vec<_>>()
        .join("\n");
    if text.len() <= LOG_BYTE_LIMIT {
        return text;
    }
    let start = text.len().saturating_sub(LOG_BYTE_LIMIT);
    let mut boundary = start;
    while !text.is_char_boundary(boundary) {
        boundary += 1;
    }
    text[boundary..].to_owned()
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

async fn write_bounded(path: &Path, bytes: &[u8], limit: usize) -> Result<(), SupervisorError> {
    if bytes.len() > limit {
        return Err(SupervisorError::Io {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "supervisor state exceeded its bound",
            ),
        });
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| SupervisorError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
    }
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    tokio::fs::write(&temp, bytes)
        .await
        .map_err(|source| SupervisorError::Io {
            path: temp.clone(),
            source,
        })?;
    tokio::fs::rename(&temp, path)
        .await
        .map_err(|source| SupervisorError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn already_exited_process_group_does_not_delay_restart() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("exit 0").kill_on_drop(false);
        command.process_group(0);
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        child.wait().await.unwrap();
        let started = Instant::now();
        terminate_orphaned_group(pid).await;
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn crash_summary_is_bounded_and_skips_supervisor_lines() {
        assert_eq!(
            useful_crash_output("useful error\nwt: restart\n"),
            "useful error"
        );
        let clipped = useful_crash_output(&"é".repeat(LOG_BYTE_LIMIT));
        assert!(clipped.len() <= LOG_BYTE_LIMIT);
        assert!(clipped.is_char_boundary(0));
    }
}
