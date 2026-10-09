//! Cancellable tmux transport for a caller-selected tmux server.
//!
//! `TmuxClient` never reads wt configuration or ambient `$TMUX`. The caller
//! chooses a named socket (`-L`) or socket path (`-S`) and a stable server cwd.
//! All commands use argv through `wt-platform`; pane text is sent through a
//! uniquely named tmux buffer on stdin and pasted without submitting Enter.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessError, ProcessOutput, ProcessRunner};

// tmux replaces non-printing separators with underscores in format output.
// Session names cannot contain ':'. Put names/paths that can contain colons
// last and parse with splitn so their contents remain intact.
const FORMAT_SEPARATOR: char = ':';
static BUFFER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TmuxSocket {
    Name(String),
    Path(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxServer {
    pub socket: TmuxSocket,
    /// The tmux server inherits the cwd of the first client. Keep it stable
    /// for the server lifetime even when callers run from removable checkouts.
    pub cwd: PathBuf,
    /// Optional config file passed at server launch; tests and embedded
    /// callers can avoid reading a user's tmux configuration.
    pub config_file: Option<PathBuf>,
}

impl TmuxServer {
    pub fn named(name: impl Into<String>) -> Self {
        Self::new(TmuxSocket::Name(name.into()))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self::new(TmuxSocket::Path(path.into()))
    }

    pub fn new(socket: TmuxSocket) -> Self {
        let cwd = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        Self {
            socket,
            cwd,
            config_file: None,
        }
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    pub fn with_config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_file = Some(path.into());
        self
    }
}

#[derive(Clone)]
pub struct TmuxClient {
    runner: ProcessRunner,
    server: TmuxServer,
}

#[derive(Debug, Error)]
pub enum TmuxError {
    #[error("tmux {operation}: {source}")]
    Process {
        operation: &'static str,
        #[source]
        source: ProcessError,
    },
    #[error("tmux {operation} failed (exit {code:?}): {stderr}")]
    Command {
        operation: &'static str,
        code: Option<i32>,
        stderr: String,
        stdout: String,
    },
    #[error("tmux {operation} returned malformed output: {line:?}")]
    MalformedOutput {
        operation: &'static str,
        line: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionInfo {
    pub name: String,
    pub id: String,
    pub created_at: i64,
    pub attached_clients: u32,
    pub window_count: u32,
    pub harness_session_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowInfo {
    pub id: String,
    pub index: u32,
    pub name: String,
    pub active: bool,
    pub pane_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneInfo {
    pub id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u32,
    pub index: u32,
    pub active: bool,
    pub pid: Option<u32>,
    pub current_path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub dead: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateSession {
    pub name: String,
    pub cwd: PathBuf,
    /// The command is shell-quoted by this crate because tmux accepts a shell
    /// command string after `new-session`, rather than separate executable
    /// arguments. Empty means use the user's configured shell.
    pub command: Vec<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateWindow {
    pub session: String,
    pub name: Option<String>,
    pub cwd: Option<PathBuf>,
    pub command: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OptionScope {
    Server,
    Session(String),
    Window(WindowTarget),
    Pane(PaneTarget),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneTarget(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowTarget(String);

impl PaneTarget {
    /// Target the active pane in an exact session. The trailing colon is
    /// required: `=session` is a session target, not a pane target.
    pub fn active_session_pane(session: impl AsRef<str>) -> Self {
        Self(exact_pane_target(session.as_ref()))
    }

    pub fn id(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl WindowTarget {
    pub fn id(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn active_session_window(session: impl AsRef<str>) -> Self {
        Self(exact_pane_target(session.as_ref()))
    }

    pub fn session_window(session: impl AsRef<str>, index: u32) -> Self {
        Self(format!("={}:{index}", session.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TmuxClient {
    pub fn new(runner: ProcessRunner, server: TmuxServer) -> Self {
        Self { runner, server }
    }

    pub fn server(&self) -> &TmuxServer {
        &self.server
    }

    /// Returns an empty list only when the server is definitely absent.
    /// Permission, spawn, and malformed-output failures remain errors.
    pub async fn list_sessions(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<SessionInfo>, TmuxError> {
        let format = format!(
            "#{{session_name}}{FORMAT_SEPARATOR}#{{session_id}}{FORMAT_SEPARATOR}#{{session_created}}{FORMAT_SEPARATOR}#{{session_attached}}{FORMAT_SEPARATOR}#{{session_windows}}{FORMAT_SEPARATOR}#{{@wt-harness-session-id}}"
        );
        let result = self
            .run(
                "list-sessions",
                ["list-sessions", "-F", format.as_str()],
                None,
                cancellation,
            )
            .await?;
        if !result.status.success() {
            if tmux_server_definitely_absent(&result.stderr_text()) {
                return Ok(Vec::new());
            }
            return Err(command_error("list-sessions", result));
        }
        parse_sessions(&result.stdout_text())
    }

    pub async fn session_exists(
        &self,
        name: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, TmuxError> {
        let target = exact_session_target(name);
        let result = self
            .run(
                "has-session",
                ["has-session", "-t", target.as_str()],
                None,
                cancellation,
            )
            .await?;
        if result.status.success() {
            return Ok(true);
        }
        let stderr = result.stderr_text();
        if tmux_server_definitely_absent(&stderr) || tmux_session_absent(&stderr) {
            return Ok(false);
        }
        Err(command_error("has-session", result))
    }

    pub async fn list_windows(
        &self,
        session: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<WindowInfo>, TmuxError> {
        let format = format!(
            "#{{window_id}}{FORMAT_SEPARATOR}#{{window_index}}{FORMAT_SEPARATOR}#{{window_active}}{FORMAT_SEPARATOR}#{{window_panes}}{FORMAT_SEPARATOR}#{{window_name}}"
        );
        let target = exact_session_target(session);
        let result = self
            .run(
                "list-windows",
                ["list-windows", "-t", target.as_str(), "-F", format.as_str()],
                None,
                cancellation,
            )
            .await?;
        checked("list-windows", result).and_then(|output| parse_windows(&output.stdout_text()))
    }

    pub async fn list_panes(
        &self,
        session: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PaneInfo>, TmuxError> {
        let format = format!(
            "#{{pane_id}}{FORMAT_SEPARATOR}#{{session_name}}{FORMAT_SEPARATOR}#{{window_id}}{FORMAT_SEPARATOR}#{{window_index}}{FORMAT_SEPARATOR}#{{pane_index}}{FORMAT_SEPARATOR}#{{pane_active}}{FORMAT_SEPARATOR}#{{pane_pid}}{FORMAT_SEPARATOR}#{{pane_width}}{FORMAT_SEPARATOR}#{{pane_height}}{FORMAT_SEPARATOR}#{{pane_dead}}{FORMAT_SEPARATOR}#{{pane_current_path}}"
        );
        let target = exact_session_target(session);
        let result = self
            .run(
                "list-panes",
                ["list-panes", "-t", target.as_str(), "-F", format.as_str()],
                None,
                cancellation,
            )
            .await?;
        checked("list-panes", result).and_then(|output| parse_panes(&output.stdout_text()))
    }

    pub fn create_session_args(&self, session: &CreateSession) -> Vec<OsString> {
        let mut args = self.prefix_args();
        args.extend(["new-session".into(), "-d".into()]);
        args.extend(["-s".into(), session.name.clone().into()]);
        args.extend(["-c".into(), session.cwd.as_os_str().to_owned()]);
        if let Some(width) = session.width {
            args.extend(["-x".into(), width.to_string().into()]);
        }
        if let Some(height) = session.height {
            args.extend(["-y".into(), height.to_string().into()]);
        }
        if !session.command.is_empty() {
            let command = session
                .command
                .iter()
                .map(|arg| shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" ");
            args.push(command.into());
        }
        args
    }

    pub async fn create_session(
        &self,
        session: &CreateSession,
        cancellation: &CancellationToken,
    ) -> Result<(), TmuxError> {
        let args = self.create_session_args(session);
        let prefix_len = self.prefix_args().len();
        let result = self
            .run_os(
                "new-session",
                args.into_iter().skip(prefix_len),
                None,
                None,
                cancellation,
            )
            .await?;
        checked("new-session", result).map(|_| ())
    }

    pub fn attach_session_args(&self, session: &str) -> Vec<OsString> {
        let mut args = self.prefix_args();
        args.extend([
            "attach-session".into(),
            "-t".into(),
            exact_session_target(session).into(),
        ]);
        args
    }

    pub fn create_window_args(&self, window: &CreateWindow) -> Vec<OsString> {
        let mut args = self.prefix_args();
        args.extend(["new-window".into(), "-d".into()]);
        args.extend(["-t".into(), exact_pane_target(&window.session).into()]);
        if let Some(name) = &window.name {
            args.extend(["-n".into(), name.clone().into()]);
        }
        if let Some(cwd) = &window.cwd {
            args.extend(["-c".into(), cwd.as_os_str().to_owned()]);
        }
        if !window.command.is_empty() {
            let command = window
                .command
                .iter()
                .map(|arg| shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" ");
            args.push(command.into());
        }
        args
    }

    pub async fn create_window(
        &self,
        window: &CreateWindow,
        cancellation: &CancellationToken,
    ) -> Result<(), TmuxError> {
        let args = self.create_window_args(window);
        let prefix_len = self.prefix_args().len();
        let result = self
            .run_os(
                "new-window",
                args.into_iter().skip(prefix_len),
                None,
                None,
                cancellation,
            )
            .await?;
        checked("new-window", result).map(|_| ())
    }

    pub async fn select_window(
        &self,
        target: &WindowTarget,
        cancellation: &CancellationToken,
    ) -> Result<(), TmuxError> {
        let result = self
            .run(
                "select-window",
                ["select-window", "-t", target.as_str()],
                None,
                cancellation,
            )
            .await?;
        checked("select-window", result).map(|_| ())
    }

    pub async fn kill_window(
        &self,
        target: &WindowTarget,
        cancellation: &CancellationToken,
    ) -> Result<(), TmuxError> {
        let result = self
            .run(
                "kill-window",
                ["kill-window", "-t", target.as_str()],
                None,
                cancellation,
            )
            .await?;
        checked("kill-window", result).map(|_| ())
    }

    pub async fn capture_pane(
        &self,
        target: &PaneTarget,
        history_lines: Option<u32>,
        cancellation: &CancellationToken,
    ) -> Result<String, TmuxError> {
        let mut args: Vec<OsString> = vec!["capture-pane".into(), "-p".into()];
        if let Some(lines) = history_lines {
            args.extend([
                "-S".into(),
                format!("-{lines}").into(),
                "-E".into(),
                "-".into(),
            ]);
        }
        args.extend(["-t".into(), target.as_str().into()]);
        let result = self
            .run_os("capture-pane", args, None, None, cancellation)
            .await?;
        checked("capture-pane", result).map(|output| output.stdout_text())
    }

    pub async fn resize_pane(
        &self,
        target: &PaneTarget,
        width: u32,
        height: u32,
        cancellation: &CancellationToken,
    ) -> Result<(), TmuxError> {
        let result = self
            .run_os(
                "resize-pane",
                [
                    "resize-pane".into(),
                    "-t".into(),
                    target.as_str().into(),
                    "-x".into(),
                    width.to_string().into(),
                    "-y".into(),
                    height.to_string().into(),
                ],
                None,
                None,
                cancellation,
            )
            .await?;
        checked("resize-pane", result).map(|_| ())
    }

    /// Paste literal text into a pane. Newlines are data; this never sends
    /// Enter or any other key after the paste.
    pub async fn send_literal(
        &self,
        target: &PaneTarget,
        text: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), TmuxError> {
        let sequence = BUFFER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let buffer = format!("wt-tmux-{}-{sequence}", std::process::id());
        let load = self
            .run(
                "load-buffer",
                ["load-buffer", "-b", buffer.as_str(), "-"],
                Some(text.as_bytes().to_vec()),
                cancellation,
            )
            .await;
        match load {
            Ok(output) => match checked("load-buffer", output) {
                Ok(_) => (),
                Err(error) => {
                    self.cleanup_buffer(&buffer).await;
                    return Err(error);
                }
            },
            Err(error) => {
                // A cancellation/timeout can race a successful tmux write,
                // so treat load-buffer's side effect as ambiguous and clean it.
                self.cleanup_buffer(&buffer).await;
                return Err(error);
            }
        };
        let paste = self
            .run(
                "paste-buffer",
                [
                    "paste-buffer",
                    "-d",
                    "-p",
                    "-b",
                    buffer.as_str(),
                    "-t",
                    target.as_str(),
                ],
                None,
                cancellation,
            )
            .await;
        // `-d` removes the buffer on a successful paste; delete-buffer is
        // harmless if it is already gone and covers failures/cancellation.
        self.cleanup_buffer(&buffer).await;
        checked("paste-buffer", paste?).map(|_| ())
    }

    /// Send explicit tmux key names such as `Enter`, `C-d`, or `Escape`.
    /// Use `send_literal` for user text so key interpretation is impossible.
    pub async fn send_keys(
        &self,
        target: &PaneTarget,
        keys: &[&str],
        cancellation: &CancellationToken,
    ) -> Result<(), TmuxError> {
        let mut args = vec![
            OsString::from("send-keys"),
            "-t".into(),
            target.as_str().into(),
        ];
        args.extend(keys.iter().map(OsString::from));
        let result = self
            .run_os("send-keys", args, None, None, cancellation)
            .await?;
        checked("send-keys", result).map(|_| ())
    }

    pub async fn get_option(
        &self,
        scope: &OptionScope,
        option: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, TmuxError> {
        let args = show_option_args(scope, option, true);
        let result = self
            .run_os("show-options", args, None, None, cancellation)
            .await?;
        if !result.status.success() {
            let stderr = result.stderr_text();
            if tmux_server_definitely_absent(&stderr) {
                return Ok(None);
            }
            return Err(command_error("show-options", result));
        }
        let output = result.stdout_text();
        if !output.is_empty() {
            return Ok(Some(output.trim_end_matches(['\r', '\n']).to_owned()));
        }
        // `show-options -qv` emits nothing both for an absent option and a
        // present empty value. Probe the display form only in that case.
        let result = self
            .run_os(
                "show-options",
                show_option_args(scope, option, false),
                None,
                None,
                cancellation,
            )
            .await?;
        if !result.status.success() {
            let stderr = result.stderr_text();
            if tmux_server_definitely_absent(&stderr) {
                return Ok(None);
            }
            return Err(command_error("show-options", result));
        }
        Ok((!result.stdout.is_empty()).then(String::new))
    }

    pub async fn set_option(
        &self,
        scope: &OptionScope,
        option: &str,
        value: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<(), TmuxError> {
        let mut args = vec![OsString::from("set-option")];
        let (scope_flag, target) = option_scope_args(scope);
        if let Some(flag) = scope_flag {
            args.push(flag.into());
        }
        if value.is_none() {
            args.push("-u".into());
        }
        if let Some(target) = target {
            args.extend(["-t".into(), target.into()]);
        }
        args.push(option.into());
        if let Some(value) = value {
            args.push(value.into());
        }
        let result = self
            .run_os("set-option", args, None, None, cancellation)
            .await?;
        checked("set-option", result).map(|_| ())
    }

    pub async fn rename_session(
        &self,
        current: &str,
        new_name: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), TmuxError> {
        let target = exact_session_target(current);
        let result = self
            .run(
                "rename-session",
                ["rename-session", "-t", target.as_str(), new_name],
                None,
                cancellation,
            )
            .await?;
        checked("rename-session", result).map(|_| ())
    }

    pub async fn rename_window(
        &self,
        target: &WindowTarget,
        new_name: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), TmuxError> {
        let result = self
            .run(
                "rename-window",
                ["rename-window", "-t", target.as_str(), new_name],
                None,
                cancellation,
            )
            .await?;
        checked("rename-window", result).map(|_| ())
    }

    /// Returns `false` when the session or server is already absent.
    pub async fn kill_session(
        &self,
        name: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, TmuxError> {
        let target = exact_session_target(name);
        let result = self
            .run(
                "kill-session",
                ["kill-session", "-t", target.as_str()],
                None,
                cancellation,
            )
            .await?;
        if result.status.success() {
            return Ok(true);
        }
        let stderr = result.stderr_text();
        if tmux_server_definitely_absent(&stderr) || tmux_session_absent(&stderr) {
            return Ok(false);
        }
        Err(command_error("kill-session", result))
    }

    pub async fn kill_server(&self, cancellation: &CancellationToken) -> Result<(), TmuxError> {
        let result = self
            .run("kill-server", ["kill-server"], None, cancellation)
            .await?;
        if result.status.success() || tmux_server_definitely_absent(&result.stderr_text()) {
            return Ok(());
        }
        Err(command_error("kill-server", result))
    }

    async fn run<const N: usize>(
        &self,
        operation: &'static str,
        args: [&str; N],
        input: Option<Vec<u8>>,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, TmuxError> {
        self.run_os(
            operation,
            args.into_iter().map(OsString::from),
            input,
            None,
            cancellation,
        )
        .await
    }

    async fn run_os(
        &self,
        operation: &'static str,
        args: impl IntoIterator<Item = OsString>,
        input: Option<Vec<u8>>,
        cwd: Option<&Path>,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, TmuxError> {
        self.run_os_with_timeout(
            operation,
            args,
            input,
            cwd,
            Duration::from_secs(15),
            cancellation,
        )
        .await
    }

    async fn run_os_with_timeout(
        &self,
        operation: &'static str,
        args: impl IntoIterator<Item = OsString>,
        input: Option<Vec<u8>>,
        cwd: Option<&Path>,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, TmuxError> {
        let mut spec = CommandSpec::new("tmux").args(self.prefix_args()).args(args);
        spec.cwd = Some(cwd.unwrap_or(&self.server.cwd).to_path_buf());
        spec.input = input;
        spec.timeout = timeout;
        spec.output_limit = 8 * 1024 * 1024;
        self.runner
            .run(spec, cancellation)
            .await
            .map_err(|source| TmuxError::Process { operation, source })
    }

    async fn cleanup_buffer(&self, buffer: &str) {
        let _ = self
            .run_os_with_timeout(
                "delete-buffer",
                ["delete-buffer".into(), "-b".into(), buffer.into()],
                None,
                None,
                Duration::from_secs(2),
                &CancellationToken::new(),
            )
            .await;
    }

    fn prefix_args(&self) -> Vec<OsString> {
        let mut args = match &self.server.socket {
            TmuxSocket::Name(name) => vec!["-L".into(), name.clone().into()],
            TmuxSocket::Path(path) => vec!["-S".into(), path.as_os_str().to_owned()],
        };
        if let Some(config_file) = &self.server.config_file {
            args.extend(["-f".into(), config_file.as_os_str().to_owned()]);
        }
        args
    }
}

pub fn tmux_server_definitely_absent(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("no server running")
        || (lower.contains("error connecting")
            && (lower.contains("no such file or directory") || lower.contains("enoent")))
}

pub fn parse_sessions(output: &str) -> Result<Vec<SessionInfo>, TmuxError> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let fields = split_fields(line, 6, "list-sessions")?;
            Ok(SessionInfo {
                name: fields[0].to_owned(),
                id: fields[1].to_owned(),
                created_at: parse_number(fields[2], "list-sessions", line)?,
                attached_clients: parse_number(fields[3], "list-sessions", line)?,
                window_count: parse_number(fields[4], "list-sessions", line)?,
                harness_session_id: (!fields[5].is_empty()).then(|| fields[5].to_owned()),
            })
        })
        .collect()
}

pub fn parse_windows(output: &str) -> Result<Vec<WindowInfo>, TmuxError> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let fields = split_fields(line, 5, "list-windows")?;
            Ok(WindowInfo {
                id: fields[0].to_owned(),
                index: parse_number(fields[1], "list-windows", line)?,
                active: parse_bool(fields[2], "list-windows", line)?,
                pane_count: parse_number(fields[3], "list-windows", line)?,
                name: fields[4].to_owned(),
            })
        })
        .collect()
}

pub fn parse_panes(output: &str) -> Result<Vec<PaneInfo>, TmuxError> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let fields = split_fields(line, 11, "list-panes")?;
            Ok(PaneInfo {
                id: fields[0].to_owned(),
                session_name: fields[1].to_owned(),
                window_id: fields[2].to_owned(),
                window_index: parse_number(fields[3], "list-panes", line)?,
                index: parse_number(fields[4], "list-panes", line)?,
                active: parse_bool(fields[5], "list-panes", line)?,
                pid: (!fields[6].is_empty())
                    .then(|| parse_number(fields[6], "list-panes", line))
                    .transpose()?,
                width: parse_number(fields[7], "list-panes", line)?,
                height: parse_number(fields[8], "list-panes", line)?,
                dead: parse_bool(fields[9], "list-panes", line)?,
                current_path: PathBuf::from(fields[10]),
            })
        })
        .collect()
}

pub fn exact_session_target(name: &str) -> String {
    format!("={name}")
}

pub fn exact_pane_target(name: &str) -> String {
    format!("={name}:")
}

pub fn exact_window_target(name: &str) -> String {
    if name.starts_with('@') || name.starts_with('=') {
        name.to_owned()
    } else if name.contains(':') {
        format!("={name}")
    } else {
        exact_pane_target(name)
    }
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests;

fn split_fields<'a>(
    line: &'a str,
    expected: usize,
    operation: &'static str,
) -> Result<Vec<&'a str>, TmuxError> {
    let fields = line.splitn(expected, FORMAT_SEPARATOR).collect::<Vec<_>>();
    if fields.len() != expected {
        return Err(TmuxError::MalformedOutput {
            operation,
            line: line.to_owned(),
        });
    }
    Ok(fields)
}

fn parse_number<T: std::str::FromStr>(
    value: &str,
    operation: &'static str,
    line: &str,
) -> Result<T, TmuxError> {
    value.parse().map_err(|_| TmuxError::MalformedOutput {
        operation,
        line: line.to_owned(),
    })
}

fn parse_bool(value: &str, operation: &'static str, line: &str) -> Result<bool, TmuxError> {
    match value {
        "1" => Ok(true),
        "0" => Ok(false),
        _ => Err(TmuxError::MalformedOutput {
            operation,
            line: line.to_owned(),
        }),
    }
}

fn option_scope_args(scope: &OptionScope) -> (Option<&'static str>, Option<String>) {
    match scope {
        OptionScope::Server => (Some("-s"), None),
        OptionScope::Session(session) => (None, Some(exact_session_target(session))),
        OptionScope::Window(target) => (Some("-w"), Some(target.as_str().to_owned())),
        OptionScope::Pane(target) => (Some("-p"), Some(target.as_str().to_owned())),
    }
}

fn show_option_args(scope: &OptionScope, option: &str, value_only: bool) -> Vec<OsString> {
    let mut args = vec![OsString::from("show-options")];
    let (scope_flag, target) = option_scope_args(scope);
    if let Some(flag) = scope_flag {
        args.push(flag.into());
    }
    args.push(if value_only {
        "-qv".into()
    } else {
        "-q".into()
    });
    if let Some(target) = target {
        args.extend(["-t".into(), target.into()]);
    }
    args.push(option.into());
    args
}

fn tmux_session_absent(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains("can't find session")
}

fn checked(operation: &'static str, output: ProcessOutput) -> Result<ProcessOutput, TmuxError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(command_error(operation, output))
    }
}

fn command_error(operation: &'static str, output: ProcessOutput) -> TmuxError {
    TmuxError::Command {
        operation,
        code: output.status.code(),
        stderr: output.stderr_text(),
        stdout: output.stdout_text(),
    }
}
