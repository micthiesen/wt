//! Cancellable tmux transport for a caller-selected tmux server.
//!
//! `TmuxClient` never reads wt configuration or ambient `$TMUX`. The caller
//! chooses a named socket (`-L`) or socket path (`-S`) and a stable server cwd.
//! Commands use `wt-platform`; session creation streams a quoted tmux command
//! on stdin to avoid the client argv frame limit. Pane text uses a uniquely
//! named tmux buffer on stdin and is pasted without submitting Enter.

use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessError, ProcessOutput, ProcessRunner};

// tmux replaces non-printing separators with underscores in format output.
// Session names cannot contain ':'. Put names/paths that can contain colons
// last and parse with splitn so their contents remain intact.
const FORMAT_SEPARATOR: char = ':';
static BUFFER_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static PALETTE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Session environment variable naming a POSIX shell file that each new
/// session command sources before it starts, for values that must stay out
/// of argv.
pub const SESSION_SOURCE: &str = "WT_SESSION_SOURCE";

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

/// Terminal colors observed by the owning TUI. Invalid or partial observations
/// are ignored rather than replaced with a guessed palette.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalPalette {
    pub default_foreground: String,
    pub default_background: String,
}

/// Validate and canonicalize the colors used in the generated tmux config.
pub fn terminal_palette(value: &serde_json::Value) -> Option<TerminalPalette> {
    let palette: TerminalPalette = serde_json::from_value(value.clone()).ok()?;
    let valid = |color: &str| {
        color.len() == 7
            && color.starts_with('#')
            && color[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    if !valid(&palette.default_foreground) || !valid(&palette.default_background) {
        return None;
    }
    Some(TerminalPalette {
        default_foreground: palette.default_foreground.to_ascii_lowercase(),
        default_background: palette.default_background.to_ascii_lowercase(),
    })
}

/// Render tmux's global window styles from a previously observed palette.
pub fn terminal_palette_config(value: &serde_json::Value) -> String {
    let Some(palette) = terminal_palette(value) else {
        return String::new();
    };
    let style = format!(
        "fg={},bg={}",
        palette.default_foreground, palette.default_background
    );
    format!("set -g window-style '{style}'\nset -g window-active-style '{style}'\n")
}

/// Persist a real OSC palette observation. Invalid values leave the previous
/// observation untouched so an unsupported terminal does not erase known
/// colors.
pub fn save_terminal_palette(
    cache_root: &Path,
    default_foreground: &str,
    default_background: &str,
) -> io::Result<bool> {
    let value = serde_json::json!({
        "defaultForeground": color_to_hex(default_foreground),
        "defaultBackground": color_to_hex(default_background),
    });
    if value["defaultForeground"].is_null() || value["defaultBackground"].is_null() {
        return Ok(false);
    }
    let Some(palette) = terminal_palette(&value) else {
        return Ok(false);
    };
    fs::create_dir_all(cache_root)?;
    let target = cache_root.join("terminal-palette.json");
    let sequence = PALETTE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = cache_root.join(format!(
        "terminal-palette.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let bytes = serde_json::to_vec(&palette).map_err(io::Error::other)?;
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(true)
}

fn color_to_hex(value: &str) -> Option<String> {
    if value.len() == 7
        && value.starts_with('#')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Some(value.to_ascii_lowercase());
    }
    let components = value.strip_prefix("rgb:")?.split('/').collect::<Vec<_>>();
    if components.len() != 3 {
        return None;
    }
    let mut output = String::from("#");
    for component in components {
        if !(1..=4).contains(&component.len())
            || !component.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        let value = u32::from_str_radix(component, 16).ok()?;
        let max = (1_u32 << (4 * component.len())) - 1;
        let scaled = ((value * 255 + max / 2) / max) as u8;
        use std::fmt::Write as _;
        write!(&mut output, "{scaled:02x}").ok()?;
    }
    Some(output)
}

/// Write an optional palette config for a private tmux server. `None` means
/// there was no trustworthy observation. When active, source the user's
/// ordinary tmux config first so adding palette styles does not hide it.
pub fn write_terminal_palette_config(cache_root: &Path, home: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(cache_root)?;
    let styles = cached_palette_styles(cache_root);
    let target = cache_root.join("tmux-palette.conf");
    let sequence = PALETTE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = cache_root.join(format!(
        "tmux-palette.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let user_config = home.join(".tmux.conf");
    let content = format!(
        "source-file -q {}\n{styles}",
        tmux_config_quote(&user_config.to_string_lossy())
    );
    let result = fs::write(&temporary, content).and_then(|()| fs::rename(&temporary, &target));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(target)
}

fn cached_palette_styles(cache_root: &Path) -> String {
    let palette_path = cache_root.join("terminal-palette.json");
    fs::metadata(&palette_path)
        .ok()
        .filter(|metadata| metadata.is_file() && metadata.len() <= 64 * 1024)
        .and_then(|_| fs::read(&palette_path).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .map(|value| terminal_palette_config(&value))
        .unwrap_or_default()
}

/// Defaults for wt's private server, including workers with no user tmux config.
pub const DEFAULT_TERMINAL_CONFIG: &str = r#"set -g status off
set -g alternate-screen on
set -g set-titles off
set -s escape-time 0
set -g mouse on
set -g focus-events on
set -g default-terminal tmux-256color
set -s set-clipboard on
set -g allow-passthrough on
set -s extended-keys always
set -sq extended-keys-format csi-u
set -s terminal-features 'xterm*:RGB:sync:extkeys:hyperlinks,tmux*:RGB:sync:extkeys:hyperlinks,alacritty*:RGB:sync:extkeys:hyperlinks'
"#;

/// Which session-switch key a session answers to. Each attached session
/// records its kind so its own key returns to the board.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionShortcut {
    Harness,
    Shell,
    Diff,
}

impl SessionShortcut {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::Shell => "shell",
            Self::Diff => "diff",
        }
    }
}

fn terminal_shortcut_bindings() -> String {
    let mut bindings = String::new();
    for (key, kind, code) in [
        ("F10", "shell", 110),
        ("F11", "diff", 111),
        ("F12", "harness", 112),
    ] {
        use std::fmt::Write as _;
        writeln!(
            bindings,
            "bind-key -n {key} if-shell -F '#{{==:#{{@wt-shortcut}},{kind}}}' 'detach-client' 'detach-client -E \"exit {code}\"'"
        ).expect("write to string");
    }
    bindings
}

/// Generate the complete private-server configuration separately from the
/// palette file, which is refreshed independently as terminals report colors.
pub fn write_terminal_config(
    cache_root: &Path,
    home: &Path,
    preamble: Option<&str>,
) -> io::Result<PathBuf> {
    fs::create_dir_all(cache_root)?;
    let styles = cached_palette_styles(cache_root);
    let shortcuts = terminal_shortcut_bindings();
    let content = format!(
        "source-file -q {}\n{}\n{}\nset -g remain-on-exit-format 'Command exited (#{{?@wt-terminal-exit-status,#{{@wt-terminal-exit-status}},#{{pane_dead_status}}}}). Enter: retry | current session key: return to wt'\nbind-key -n Enter if-shell -F '#{{pane_dead}}' 'set-option -u @wt-harness-session-id ; respawn-pane' 'send-keys Enter'\n{shortcuts}",
        tmux_config_quote(&home.join(".tmux.conf").to_string_lossy()),
        preamble.unwrap_or(DEFAULT_TERMINAL_CONFIG),
        styles,
    );
    let target = cache_root.join("tmux-session.conf");
    let temporary = cache_root.join(format!(
        "tmux-session.{}.{}.tmp",
        std::process::id(),
        PALETTE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = fs::write(&temporary, content).and_then(|()| fs::rename(&temporary, &target));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(target)
}

fn tmux_config_quote(value: &str) -> String {
    let mut quoted = String::from("'");
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
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
    session_environment: Vec<(String, String)>,
    retain_failure: bool,
    command_prefix: Vec<String>,
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
    /// Presentation only. Never establishes a conversation's identity.
    pub title: String,
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
        Self {
            runner,
            server,
            session_environment: Vec::new(),
            retain_failure: false,
            command_prefix: Vec::new(),
        }
    }

    /// Set immutable environment values for each newly created session. These
    /// are passed to tmux `new-session -e`, never written to server-global
    /// state, so separate clients can safely share one server with distinct
    /// configuration selectors.
    pub fn with_session_environment(
        mut self,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.session_environment = environment.into_iter().collect();
        self
    }

    /// Wrap the command inside the pane, including retained-command retries.
    /// An empty command keeps tmux's normal default-shell behavior.
    pub fn with_command_prefix(mut self, prefix: Vec<String>) -> Self {
        self.command_prefix = prefix;
        self
    }

    pub fn server(&self) -> &TmuxServer {
        &self.server
    }

    /// Interactive terminals retain failure output. Background action/dev
    /// clients keep their own completion lifecycle and do not opt in.
    pub fn with_failure_retention(mut self) -> Self {
        self.retain_failure = true;
        self
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
        if let Some(present) =
            session_presence_after_failure(&result, &target, self.list_sessions(cancellation))
                .await?
        {
            return Ok(present);
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
        let format = String::from(
            "#{pane_id}\t#{session_name}\t#{window_id}\t#{window_index}\t#{pane_index}\t#{pane_active}\t#{pane_pid}\t#{pane_width}\t#{pane_height}\t#{pane_dead}\t#{q:pane_title}\t#{pane_current_path}",
        );
        let target = exact_session_target(session);
        let result = self
            .run(
                "list-panes",
                [
                    "-u",
                    "list-panes",
                    "-t",
                    target.as_str(),
                    "-F",
                    format.as_str(),
                ],
                None,
                cancellation,
            )
            .await?;
        checked("list-panes", result).and_then(|output| parse_panes(&output.stdout_text()))
    }

    /// One batched snapshot, including retained failed panes. Callers can
    /// distinguish an inspectable terminal from a still-running agent.
    pub async fn list_all_panes(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<PaneInfo>, TmuxError> {
        let format = String::from(
            "#{pane_id}\t#{session_name}\t#{window_id}\t#{window_index}\t#{pane_index}\t#{pane_active}\t#{pane_pid}\t#{pane_width}\t#{pane_height}\t#{pane_dead}\t#{q:pane_title}\t#{pane_current_path}",
        );
        let result = self
            .run(
                "list-panes",
                ["-u", "list-panes", "-a", "-F", &format],
                None,
                cancellation,
            )
            .await?;
        if !result.status.success() && tmux_server_definitely_absent(&result.stderr_text()) {
            return Ok(Vec::new());
        }
        checked("list-panes", result).and_then(|output| parse_panes(&output.stdout_text()))
    }

    pub fn create_session_args(&self, session: &CreateSession) -> Vec<OsString> {
        let mut args = self.prefix_args();
        args.extend(["new-session".into(), "-d".into()]);
        args.extend(["-s".into(), session.name.clone().into()]);
        args.extend(["-c".into(), session.cwd.as_os_str().to_owned()]);
        for (key, value) in &self.session_environment {
            args.extend(["-e".into(), format!("{key}={value}").into()]);
        }
        if let Some(width) = session.width {
            args.extend(["-x".into(), width.to_string().into()]);
        }
        if let Some(height) = session.height {
            args.extend(["-y".into(), height.to_string().into()]);
        }
        if !session.command.is_empty() {
            let workload_command = self
                .command_prefix
                .iter()
                .chain(&session.command)
                .cloned()
                .collect::<Vec<_>>();
            let session_command = if self.retain_failure {
                // Decide retention before the pane process exits instead of
                // relying on tmux's `failed` status reaper. Linux tmux with
                // libutempter can lose SIGCHLD and retain successful commands
                // forever (upstream tmux/tmux#4559). Reset on every retry.
                let supervisor = r#"tmux set-option -p -t "$TMUX_PANE" remain-on-exit off || exit
tmux set-option -p -t "$TMUX_PANE" @wt-terminal-supervisor "$$" || exit
tmux set-option -pu -t "$TMUX_PANE" @wt-terminal-exit-status
trap ':' INT QUIT
"$@"
wt_terminal_status=$?
if [ "$wt_terminal_status" -ne 0 ]; then
    tmux set-option -p -t "$TMUX_PANE" @wt-terminal-exit-status "$wt_terminal_status"
    tmux set-option -p -t "$TMUX_PANE" remain-on-exit on
fi
exit "$wt_terminal_status""#;
                [
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    supervisor.to_owned(),
                    "sh".to_owned(),
                ]
                .into_iter()
                .chain(workload_command.iter().cloned())
                .collect::<Vec<_>>()
            } else {
                workload_command
            };
            // `new-session -e` records the session environment, but tmux runs
            // explicit commands through the user's configured shell first.
            // Shell startup files can rewrite PATH, so install the same values
            // once more at the actual command boundary.
            let command = if self.session_environment.is_empty() {
                session_command.clone()
            } else {
                // A session source file holds secrets. Sourcing it inside
                // the pane keeps their values out of every process's argv.
                let source = self
                    .session_environment
                    .iter()
                    .any(|(key, _)| key == SESSION_SOURCE)
                    .then(|| {
                        [
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            format!(
                                "if [ -r \"${SESSION_SOURCE}\" ]; then . \"${SESSION_SOURCE}\"; fi; exec \"$@\""
                            ),
                            "sh".to_owned(),
                        ]
                    })
                    .into_iter()
                    .flatten();
                std::iter::once("/usr/bin/env".to_owned())
                    .chain(
                        self.session_environment
                            .iter()
                            .map(|(key, value)| format!("{key}={value}")),
                    )
                    .chain(source)
                    .chain(session_command)
                    .collect()
            };
            let command = command
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
        self.create_session_with_options(session, &[], cancellation)
            .await
    }

    /// Create and stamp a slot in one server command queue. Cancellation or a
    /// lost client reply cannot leave a successfully created slot waiting for
    /// a second client request to record its ownership.
    pub async fn create_session_with_options(
        &self,
        session: &CreateSession,
        options: &[(&str, &str)],
        cancellation: &CancellationToken,
    ) -> Result<(), TmuxError> {
        let args = self.create_session_args(session);
        let prefix_len = self.prefix_args().len();
        // tmux's client command frame is bounded. Managed PATH and other
        // pinned environment values can exceed it even though the server can
        // execute the command. Stream one quoted command through source-file
        // instead; tmux retains the same environment and pane command for
        // respawn. start-server also supports the first session on a cold
        // socket. This is one mutation, never an automatic retry.
        let mut command = tmux_command_source(&args[prefix_len..]);
        for (key, value) in options {
            command.extend(tmux_command_source(&[
                "set-option".into(),
                "-t".into(),
                exact_pane_target(&session.name).into(),
                (*key).into(),
                (*value).into(),
            ]));
        }
        let result = self
            .run_os(
                "new-session",
                ["start-server", ";", "source-file", "-"].map(OsString::from),
                Some(command),
                None,
                cancellation,
            )
            .await?;
        checked("new-session", result).map(|_| ())
    }

    /// Attach to `session`, tagging it with the caller's `shortcut` kind so
    /// the matching F10/F11/F12 key detaches back to the board. The kind is
    /// never inferred from the name: a worktree `fix-shell` has an agent
    /// session named exactly `fix-shell`.
    pub fn attach_session_args(&self, session: &str, shortcut: SessionShortcut) -> Vec<OsString> {
        let mut args = self.prefix_args();
        if let Some(config) = &self.server.config_file {
            args.extend([
                "source-file".into(),
                config.as_os_str().to_owned(),
                ";".into(),
            ]);
        }
        args.extend([
            "set-option".into(),
            "-t".into(),
            exact_pane_target(session).into(),
            "@wt-shortcut".into(),
            shortcut.as_str().into(),
            ";".into(),
        ]);
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
            let command = self
                .command_prefix
                .iter()
                .chain(&window.command)
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

    /// Type text as literal key input, without bracketed-paste markers or Enter.
    /// Native terminal commands may distinguish typed input from pasted prompts.
    pub async fn type_literal(
        &self,
        target: &PaneTarget,
        text: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), TmuxError> {
        let result = self
            .run(
                "send-keys",
                ["send-keys", "-t", target.as_str(), "-l", "--", text],
                None,
                cancellation,
            )
            .await?;
        checked("send-keys", result).map(|_| ())
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

    /// Apply terminal defaults to an already-running private server. The
    /// persisted config handles cold starts; this avoids stale colors for
    /// sessions in a server that was started before the observation.
    pub async fn apply_terminal_palette(
        &self,
        default_foreground: &str,
        default_background: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, TmuxError> {
        let (Some(foreground), Some(background)) = (
            color_to_hex(default_foreground),
            color_to_hex(default_background),
        ) else {
            return Ok(false);
        };
        let style = format!("fg={foreground},bg={background}");
        let result = self
            .run_os(
                "apply terminal palette",
                [
                    "set-option".into(),
                    "-g".into(),
                    "window-style".into(),
                    style.clone().into(),
                    ";".into(),
                    "set-option".into(),
                    "-g".into(),
                    "window-active-style".into(),
                    style.into(),
                ],
                None,
                None,
                cancellation,
            )
            .await?;
        if result.status.success() {
            return Ok(true);
        }
        if tmux_server_definitely_absent(&result.stderr_text()) {
            return Ok(false);
        }
        Err(command_error("apply terminal palette", result))
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

    /// Targets the immutable session ID from an inventory observation. A
    /// replacement session with the same name will not be terminated.
    pub async fn kill_session_id(
        &self,
        id: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, TmuxError> {
        if !id
            .strip_prefix('$')
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit()))
        {
            return Err(TmuxError::MalformedOutput {
                operation: "session identity",
                line: id.to_owned(),
            });
        }
        self.kill_session_target(id, cancellation).await
    }

    /// Returns `false` when the session or server is already absent.
    pub async fn kill_session(
        &self,
        name: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, TmuxError> {
        let target = exact_session_target(name);
        self.kill_session_target(&target, cancellation).await
    }

    async fn kill_session_target(
        &self,
        target: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, TmuxError> {
        let result = self
            .run(
                "kill-session",
                ["kill-session", "-t", target],
                None,
                cancellation,
            )
            .await?;
        if result.status.success() {
            return Ok(true);
        }
        if session_presence_after_failure(&result, target, self.list_sessions(cancellation)).await?
            == Some(false)
        {
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
        observe_after_server_exit(operation, timeout, cancellation, |remaining| {
            let mut attempt = spec.clone();
            attempt.timeout = remaining;
            self.runner.run(attempt, cancellation)
        })
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

fn tmux_command_source(args: &[OsString]) -> Vec<u8> {
    let mut source = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if index != 0 {
            source.push(b' ');
        }
        source.push(b'"');
        for byte in arg.as_encoded_bytes() {
            match *byte {
                b'"' | b'\\' | b'$' | b'~' => source.extend_from_slice(&[b'\\', *byte]),
                // tmux treats a physical line beginning with '#' as a comment
                // even inside a multiline quoted argument. Octal escapes keep
                // the source on one line and preserve the original bytes.
                0..=31 | 127 => source.extend_from_slice(&[
                    b'\\',
                    b'0' + (byte >> 6),
                    b'0' + ((byte >> 3) & 7),
                    b'0' + (byte & 7),
                ]),
                _ => source.push(*byte),
            }
        }
        source.push(b'"');
    }
    source.push(b'\n');
    source
}

// A read can connect just as the last pane exits and the server shuts down.
// That transport error proves neither presence nor absence. Re-observe only
// read-only commands, within the original budget; callers classify the final
// response normally. Never replay a mutation whose outcome may be unknown.
async fn observe_after_server_exit<F, Fut>(
    operation: &str,
    timeout: Duration,
    cancellation: &CancellationToken,
    mut read: F,
) -> Result<ProcessOutput, ProcessError>
where
    F: FnMut(Duration) -> Fut,
    Fut: std::future::Future<Output = Result<ProcessOutput, ProcessError>>,
{
    let retryable = matches!(
        operation,
        "has-session"
            | "list-sessions"
            | "list-windows"
            | "list-panes"
            | "show-options"
            | "capture-pane"
    );
    let started = tokio::time::Instant::now();
    for attempt in 0..3 {
        let result = read(timeout.saturating_sub(started.elapsed())).await?;
        if !retryable
            || attempt == 2
            || result.status.success()
            || result.stderr_text().trim() != "server exited unexpectedly"
            || started.elapsed() + Duration::from_millis(20) >= timeout
        {
            return Ok(result);
        }
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ProcessError::Cancelled { program: "tmux".into() }),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
    unreachable!("the final attempt always returns")
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
            let titled = line.contains('\t');
            let fields = if titled {
                line.splitn(12, '\t').collect::<Vec<_>>()
            } else {
                line.splitn(11, FORMAT_SEPARATOR).collect::<Vec<_>>()
            };
            if fields.len() != if titled { 12 } else { 11 } {
                return Err(TmuxError::MalformedOutput {
                    operation: "list-panes",
                    line: line.into(),
                });
            }
            Ok(PaneInfo {
                title: if titled {
                    decode_title(fields[10])
                } else {
                    String::new()
                },
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
                current_path: PathBuf::from(fields[if titled { 11 } else { 10 }]),
            })
        })
        .collect()
}

// `q:` prevents control characters in user-configurable titles from breaking
// the line-oriented inventory. Undo shell quoting only for visible characters;
// octal/control escapes remain text and never become terminal controls.
fn decode_title(quoted: &str) -> String {
    let mut chars = quoted.chars();
    let mut title = String::new();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                title.push(next);
            }
        } else {
            title.push(ch);
        }
    }
    title
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
        // Option commands resolve their -t through a pane/window target even
        // when the option belongs to the session. The trailing colon keeps
        // '=name' an exact session selector instead of a literal window name.
        OptionScope::Session(session) => (None, Some(exact_pane_target(session))),
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

/// Older tmux resolves a current session before an explicit target. During
/// last-session teardown it can report `no current target` without inspecting
/// our target. Reconcile with target-free inventory; never infer absence from
/// that error or replay a failed mutation. Other ambiguous failures stay errors.
async fn session_presence_after_failure(
    result: &ProcessOutput,
    target: &str,
    inventory: impl std::future::Future<Output = Result<Vec<SessionInfo>, TmuxError>>,
) -> Result<Option<bool>, TmuxError> {
    let stderr = result.stderr_text();
    if tmux_server_definitely_absent(&stderr) || tmux_session_absent(&stderr) {
        return Ok(Some(false));
    }
    if result.status.code() == Some(1) && stderr.trim() == "no current target" {
        let sessions = inventory.await?;
        let present = sessions.iter().any(|session| {
            if let Some(name) = target.strip_prefix('=') {
                session.name == name
            } else {
                session.id == target
            }
        });
        return Ok(Some(present));
    }
    Ok(None)
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
