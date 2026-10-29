use std::{
    fs,
    os::{fd::AsRawFd, unix::fs::FileTypeExt},
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_tmux::{CreateSession, OptionScope, PaneTarget, TmuxClient, TmuxError};

use super::{
    ClaudeHarness, ClaudePaths, RegistrySession, RegistryStatus,
    auth::{
        self, CLAUDE_OAUTH_TOKEN_ENV, ClaudeAuthError, ClaudeLaunchAuth, LAUNCH_AUTH_OPTION,
        LAUNCH_AUTH_TOKEN_COMMAND, OVERRIDING_AUTH_ENV, REDIRECTING_ENV, TokenHandoff,
    },
    claude_tmux_name,
    identity::claude_session_id,
    inspector_socket_path,
    names::reap_claude_names,
    registry::read_registry,
    shims::{ensure_inspect_shims_at, stale_harness_shims},
};

const START_TIMEOUT: Duration = Duration::from_secs(20);
const START_POLL: Duration = Duration::from_millis(200);
const PANE_TAIL: u32 = 8;
const CLAUDE_ENV_TO_CLEAR: &[&str] = &[
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_PID",
    "CLAUDECODE",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeSessionTarget {
    pub slug: String,
    pub cwd: PathBuf,
    pub managed_name: Option<String>,
}

/// Extra `claude` arguments for one exact slot, appended each time a manager
/// creates its process. Live processes keep the arguments they started with.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeLaunchArgs {
    slug: String,
    cwd: PathBuf,
    args: Vec<String>,
}

impl ClaudeLaunchArgs {
    pub fn new(slug: impl Into<String>, cwd: &Path, args: Vec<String>) -> Self {
        Self {
            slug: slug.into(),
            cwd: canonical(cwd),
            args,
        }
    }

    /// Both identity parts must match, as for [`ClaudeLaunchAuth`].
    pub fn applies_to(&self, target: &ClaudeSessionTarget) -> bool {
        target.slug == self.slug && canonical(&target.cwd) == self.cwd
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeSessionInfo {
    pub session_id: String,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub pid: u32,
    pub status: RegistryStatus,
    pub waiting_for: Option<String>,
    pub started_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Error)]
pub enum ClaudeSessionManagerError {
    #[error("Claude session {operation}: {detail}")]
    Operation {
        operation: &'static str,
        detail: String,
    },
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    #[error("Claude session lock: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Auth(#[from] ClaudeAuthError),
}

/// Owns wt's deterministic Claude slots on one explicit tmux server.
/// Registration comes from Claude's per-process registry; tmux is only
/// consulted for exact slot existence and pane diagnostics.
#[derive(Clone)]
pub struct ClaudeSessionManager {
    harness: ClaudeHarness,
    paths: ClaudePaths,
    tmux: TmuxClient,
    start_timeout: Duration,
    spawn_path: Option<String>,
    launch_auth: Option<ClaudeLaunchAuth>,
    launch_args: Option<ClaudeLaunchArgs>,
}

impl ClaudeSessionManager {
    pub fn new(harness: ClaudeHarness, tmux: TmuxClient) -> Self {
        let paths = harness.paths().clone();
        Self {
            harness,
            paths,
            tmux,
            start_timeout: START_TIMEOUT,
            spawn_path: None,
            launch_auth: None,
            launch_args: None,
        }
    }

    pub fn with_start_timeout(mut self, timeout: Duration) -> Self {
        self.start_timeout = timeout;
        self
    }

    pub fn with_spawn_path(mut self, path: impl Into<String>) -> Self {
        self.spawn_path = Some(path.into());
        self
    }

    /// Select the account of the slot `auth` names for each process this
    /// manager creates there. Other slots and live processes are untouched.
    pub fn with_launch_auth(mut self, auth: Option<ClaudeLaunchAuth>) -> Self {
        self.launch_auth = auth;
        self
    }

    /// Append `args` to each `claude` this manager creates in their slot.
    pub fn with_launch_args(mut self, args: Option<ClaudeLaunchArgs>) -> Self {
        self.launch_args = args;
        self
    }

    /// Explain when a live slot's account differs from the configured
    /// selection. Auth is fixed when Claude starts, so the slot keeps its
    /// account until it is restarted; wt never restarts it to change that.
    pub async fn launch_auth_restart_notice(
        &self,
        target: &ClaudeSessionTarget,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, ClaudeSessionManagerError> {
        let name = self.tmux_name(target);
        if !self.tmux.session_exists(&name, cancel).await? {
            return Ok(None);
        }
        let selected = self
            .launch_auth
            .as_ref()
            .is_some_and(|auth| auth.applies_to(target));
        let started = self
            .tmux
            .get_option(
                &OptionScope::Session(name.clone()),
                LAUNCH_AUTH_OPTION,
                cancel,
            )
            .await?
            .is_some_and(|value| value == LAUNCH_AUTH_TOKEN_COMMAND);
        Ok(match (selected, started) {
            (true, false) => Some(format!(
                "{name} was started before harness.wt_session_claude_token_command applied and still uses the normal Claude login; stop and restart it to use the selected account"
            )),
            (false, true) if self.launch_auth.is_none() => Some(format!(
                "{name} still uses the token from harness.wt_session_claude_token_command, which is no longer configured; stop and restart it to use the normal Claude login"
            )),
            _ => None,
        })
    }

    pub(super) fn paths(&self) -> &ClaudePaths {
        &self.paths
    }
    pub(super) fn tmux(&self) -> &TmuxClient {
        &self.tmux
    }

    pub fn list(&self) -> Vec<ClaudeSessionInfo> {
        let mut sessions: Vec<_> = read_registry(&self.paths.sessions_dir())
            .into_iter()
            .map(info_from_registry)
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        sessions
    }

    pub fn find(
        &self,
        target: &ClaudeSessionTarget,
    ) -> Result<Option<ClaudeSessionInfo>, ClaudeSessionManagerError> {
        let cwd = canonical(&target.cwd);
        let session_id = claude_session_id(&cwd, target.managed_name.as_deref());
        let tmux_name = claude_tmux_name(&target.slug, target.managed_name.as_deref());
        let all = self.list();
        let exact: Vec<_> = all
            .iter()
            .filter(|s| s.session_id == session_id && s.cwd == cwd)
            .cloned()
            .collect();
        if exact.len() > 1 {
            return Err(operation(
                "find",
                format!(
                    "multiple live Claude processes share session {session_id} in {}",
                    cwd.display()
                ),
            ));
        }
        if let Some(found) = exact.into_iter().next() {
            return Ok(Some(found));
        }
        // A primary may adopt a pre-wt process only when cwd identifies it
        // unambiguously. Named slots always require their deterministic UUID.
        if target.managed_name.is_some() {
            return Ok(None);
        }
        let candidates: Vec<_> = all
            .into_iter()
            .filter(|s| {
                s.cwd == cwd
                    && (s.name.is_none()
                        || s.name.as_deref() == Some("primary")
                        || s.name.as_deref() == Some(&tmux_name))
            })
            .collect();
        if candidates.len() > 1 {
            return Err(operation(
                "find",
                format!(
                    "multiple live Claude processes are associated with {}",
                    cwd.display()
                ),
            ));
        }
        Ok(candidates.into_iter().next())
    }

    pub async fn ensure(
        &self,
        target: &ClaudeSessionTarget,
        cancel: &CancellationToken,
    ) -> Result<(ClaudeSessionInfo, bool), ClaudeSessionManagerError> {
        let _guard = SessionLock::acquire(
            &self.paths.lock_dir,
            &self.lock_key(target),
            Duration::from_secs(120),
            cancel,
        )
        .await?;
        if let Some(session) = self.find(target)? {
            return Ok((session, false));
        }
        let (session, _) = self.start_locked(target, cancel).await?;
        Ok((session, true))
    }

    pub async fn start(
        &self,
        target: &ClaudeSessionTarget,
        cancel: &CancellationToken,
    ) -> Result<ClaudeSessionInfo, ClaudeSessionManagerError> {
        let _guard = SessionLock::acquire(
            &self.paths.lock_dir,
            &self.lock_key(target),
            Duration::from_secs(120),
            cancel,
        )
        .await?;
        if self.find(target)?.is_some() {
            return Err(operation(
                "start",
                format!(
                    "Claude session {} is already running",
                    self.tmux_name(target)
                ),
            ));
        }
        self.start_locked(target, cancel)
            .await
            .map(|(session, _)| session)
    }

    pub async fn stop(
        &self,
        target: &ClaudeSessionTarget,
        cancel: &CancellationToken,
    ) -> Result<(), ClaudeSessionManagerError> {
        let _guard = SessionLock::acquire(
            &self.paths.lock_dir,
            &self.lock_key(target),
            Duration::from_secs(120),
            cancel,
        )
        .await?;
        let name = self.tmux_name(target);
        if self.tmux.session_exists(&name, cancel).await? {
            self.tmux.kill_session(&name, cancel).await?;
        }
        Ok(())
    }

    /// Stop only the selected deterministic Claude conversation. The UUID
    /// comparison and tmux kill share the normal per-session lock with start,
    /// so a name reused between picker preflight and confirmation is safe.
    pub async fn stop_exact(
        &self,
        target: &ClaudeSessionTarget,
        expected_session_id: &str,
        cancel: &CancellationToken,
    ) -> Result<bool, ClaudeSessionManagerError> {
        let _guard = SessionLock::acquire(
            &self.paths.lock_dir,
            &self.lock_key(target),
            Duration::from_secs(120),
            cancel,
        )
        .await?;
        let Some(session) = self.find(target)? else {
            return Ok(false);
        };
        if session.session_id != expected_session_id {
            return Err(operation(
                "stop",
                format!(
                    "Claude session {} no longer matches selected conversation {expected_session_id}",
                    self.tmux_name(target)
                ),
            ));
        }
        self.tmux
            .kill_session(&self.tmux_name(target), cancel)
            .await?;
        Ok(true)
    }

    /// Remove only stale Inspector sockets. A failed tmux inventory is an
    /// error, never evidence that sockets or sessions are absent.
    pub async fn reap_inspector_sockets(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<PathBuf>, ClaudeSessionManagerError> {
        let sessions = self.tmux.list_sessions(cancel).await?;
        let live: std::collections::HashSet<_> = sessions.into_iter().map(|s| s.name).collect();
        let dir = self.paths.cache_dir.join("insp");
        let mut removed = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(x) => x,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(removed),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
                continue;
            };
            if !live.contains(name) && fs::remove_file(&path).is_ok() {
                removed.push(path);
            }
        }
        Ok(removed)
    }

    pub async fn stale_shims(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<String>, ClaudeSessionManagerError> {
        let _ = cancel;
        Ok(stale_harness_shims(&self.paths.cache_dir))
    }

    pub async fn reap_state(
        &self,
        live_slugs: &std::collections::HashSet<String>,
        cancel: &CancellationToken,
    ) -> Result<Vec<PathBuf>, ClaudeSessionManagerError> {
        reap_claude_names(&self.paths.cache_dir, live_slugs)
            .map_err(|error| operation("reap", error.to_string()))?;
        // A handoff outlives its launch only if wt or tmux died before the
        // pane read it. Registration has long finished by this age.
        auth::reap_stale_handoffs(
            &self.paths.cache_dir.join("auth"),
            self.start_timeout.max(Duration::from_secs(60)) * 2,
        );
        self.reap_inspector_sockets(cancel).await
    }

    pub fn tmux_name(&self, target: &ClaudeSessionTarget) -> String {
        claude_tmux_name(&target.slug, target.managed_name.as_deref())
    }

    async fn start_locked(
        &self,
        target: &ClaudeSessionTarget,
        cancel: &CancellationToken,
    ) -> Result<(ClaudeSessionInfo, bool), ClaudeSessionManagerError> {
        let name = self.tmux_name(target);
        // Query the explicit tmux server before touching the socket path.
        // Errors propagate, preventing a duplicate cold start on uncertainty.
        let existed = self.tmux.session_exists(&name, cancel).await?;
        if !existed {
            self.prepare_inspector(&name)?;
        }
        // A token handoff lives until Claude registers or the start fails.
        let mut handoff = if existed {
            None
        } else {
            self.select_account(target, &name, cancel).await?
        };
        let started = if existed {
            true
        } else {
            match self.create(target, &name, handoff.as_ref(), cancel).await {
                Ok(()) => false,
                Err(error) => {
                    if self.tmux.session_exists(&name, cancel).await? {
                        true
                    } else {
                        return Err(error);
                    }
                }
            }
        };
        if let Some(session) = self.wait_for_registry(target, cancel).await? {
            return Ok((session, started));
        }
        if started {
            let before = self.pane_tail(&name, cancel).await;
            // Select the account before stopping the unregistered slot, so a
            // failing helper leaves it exactly as it was.
            handoff = self.select_account(target, &name, cancel).await?;
            self.tmux.kill_session(&name, cancel).await?;
            self.prepare_inspector(&name)?;
            self.create(target, &name, handoff.as_ref(), cancel).await?;
            if let Some(session) = self.wait_for_registry(target, cancel).await? {
                return Ok((session, false));
            }
            let after = self.pane_tail(&name, cancel).await;
            return Err(operation(
                "start",
                format!(
                    "Claude did not register within {}s after recycling the pre-existing {name} session. Before:\n{before}\nNow:\n{after}",
                    self.start_timeout.as_secs()
                ),
            ));
        }
        let pane = self.pane_tail(&name, cancel).await;
        Err(operation(
            "start",
            format!(
                "Claude started but did not register within {}s. Its pane ({name}) holds:\n{pane}",
                self.start_timeout.as_secs()
            ),
        ))
    }

    async fn wait_for_registry(
        &self,
        target: &ClaudeSessionTarget,
        cancel: &CancellationToken,
    ) -> Result<Option<ClaudeSessionInfo>, ClaudeSessionManagerError> {
        let end = tokio::time::Instant::now() + self.start_timeout;
        loop {
            if let Some(session) = self.find(target)? {
                return Ok(Some(session));
            }
            if tokio::time::Instant::now() >= end {
                return Ok(None);
            }
            tokio::select! { biased; _ = cancel.cancelled() => return Err(operation("wait", "cancelled while waiting for Claude registration")), _ = tokio::time::sleep(START_POLL) => {} }
        }
    }

    /// Run the slot's token helper, if one applies, and write the handoff
    /// before anything is created or stopped, so a failing helper leaves no
    /// session that could run with another login.
    async fn select_account(
        &self,
        target: &ClaudeSessionTarget,
        tmux_name: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<TokenHandoff>, ClaudeSessionManagerError> {
        let Some(auth) = self
            .launch_auth
            .as_ref()
            .filter(|auth| auth.applies_to(target))
        else {
            return Ok(None);
        };
        let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
        auth::check_settings(&auth::settings_files(
            &self.paths.home,
            &canonical(&target.cwd),
            config_dir.as_deref(),
        ))?;
        let token = auth.fetch(cancel).await?;
        Ok(Some(
            TokenHandoff::write(&self.paths.cache_dir.join("auth"), tmux_name, &token)
                .map_err(ClaudeAuthError::handoff)?,
        ))
    }

    async fn create(
        &self,
        target: &ClaudeSessionTarget,
        tmux_name: &str,
        handoff: Option<&TokenHandoff>,
        cancel: &CancellationToken,
    ) -> Result<(), ClaudeSessionManagerError> {
        let spawn = self
            .harness
            .build_spawn_command(&crate::HarnessSpawnRequest {
                worktree_path: canonical(&target.cwd),
                slug: target.slug.clone(),
                managed_name: target.managed_name.clone(),
                resume_session_id: None,
                display_label: None,
            });
        let path = self
            .spawn_path
            .clone()
            .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
        // Shims are advisory (see `ensure_inspect_shims`): without them a child
        // Bun program may contend for the Inspector socket, but Claude starts.
        let _ = ensure_inspect_shims_at(&self.paths.cache_dir.join("shims"), &path);
        let shim_path = prepend_path(&self.paths.cache_dir.join("shims"), &path);
        let socket = inspector_socket_path(&self.paths.cache_dir, tmux_name);
        fs::create_dir_all(socket.parent().unwrap_or(Path::new(".")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(socket.parent().unwrap(), fs::Permissions::from_mode(0o700))?;
        }
        let inspector_url = socket
            .to_str()
            .filter(|s| url_path_safe(s))
            .map(|s| format!("ws+unix://{s}"));
        let stderr_path = self
            .paths
            .cache_dir
            .join("sessions")
            .join(format!("{tmux_name}.err"));
        fs::create_dir_all(stderr_path.parent().unwrap())?;
        let mut command = vec!["env".to_owned()];
        for var in [
            "TMUX",
            "TMUX_PANE",
            "WT_AGENT",
            "BROWSER_CONTROL_SESSION",
            "NO_COLOR",
            "NO_COLOUR",
        ]
        .iter()
        .chain(CLAUDE_ENV_TO_CLEAR)
        {
            command.extend(["-u".to_owned(), (*var).to_owned()]);
        }
        // Only the selected launch drops credentials Claude ranks above the
        // token; every other session keeps its environment as it was.
        if handoff.is_some() {
            for var in OVERRIDING_AUTH_ENV
                .iter()
                .chain(REDIRECTING_ENV)
                .chain(&[CLAUDE_OAUTH_TOKEN_ENV])
            {
                command.extend(["-u".to_owned(), (*var).to_owned()]);
            }
        }
        command.push(format!("PATH={shim_path}"));
        command.push(format!("HOME={}", self.paths.home.to_string_lossy()));
        command.push(format!("WT_AGENT={}", target.slug));
        if let Some(url) = inspector_url {
            command.push(format!("BUN_INSPECT={url}"));
        }
        match handoff {
            Some(handoff) => command.extend([
                "bash".into(),
                "-c".into(),
                auth::pane_script(),
                "_".into(),
                stderr_path.to_string_lossy().into_owned(),
                handoff.path().to_string_lossy().into_owned(),
            ]),
            None => command.extend([
                "bash".into(),
                "-c".into(),
                "p=$1; shift; exec \"$@\" 2> \"$p\"".into(),
                "_".into(),
                stderr_path.to_string_lossy().into_owned(),
            ]),
        }
        command.push(spawn.program.to_string_lossy().into_owned());
        command.extend(spawn.args);
        if let Some(extra) = self
            .launch_args
            .as_ref()
            .filter(|extra| extra.applies_to(target))
        {
            command.extend(extra.args.iter().cloned());
        }
        let create = CreateSession {
            name: tmux_name.into(),
            cwd: canonical(&target.cwd),
            command,
            width: Some(200),
            height: Some(50),
        };
        self.tmux.create_session(&create, cancel).await?;
        if handoff.is_some() {
            // Recorded for the restart notice only; Claude has its token.
            let _ = self
                .tmux
                .set_option(
                    &OptionScope::Session(tmux_name.to_owned()),
                    LAUNCH_AUTH_OPTION,
                    Some(LAUNCH_AUTH_TOKEN_COMMAND),
                    cancel,
                )
                .await;
        }
        Ok(())
    }

    fn prepare_inspector(&self, tmux_name: &str) -> Result<(), ClaudeSessionManagerError> {
        let socket = inspector_socket_path(&self.paths.cache_dir, tmux_name);
        let parent = socket.parent().expect("Inspector socket has parent");
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        if let Ok(meta) = fs::symlink_metadata(&socket)
            && meta.file_type().is_socket()
        {
            fs::remove_file(&socket)?;
        }
        Ok(())
    }

    async fn pane_tail(&self, name: &str, cancel: &CancellationToken) -> String {
        match self
            .tmux
            .capture_pane(
                &PaneTarget::active_session_pane(name),
                Some(PANE_TAIL),
                cancel,
            )
            .await
        {
            Ok(text) => {
                let lines: Vec<_> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                if lines.is_empty() {
                    "  (pane is empty — nothing has run in it)".into()
                } else {
                    lines
                        .iter()
                        .rev()
                        .take(8)
                        .rev()
                        .map(|l| format!("  | {}", l.trim_end()))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            Err(error) => format!("  (pane could not be read: {error})"),
        }
    }

    fn lock_key(&self, target: &ClaudeSessionTarget) -> String {
        format!("__claude_session__{}", self.tmux_name(target))
    }
}

fn info_from_registry(r: RegistrySession) -> ClaudeSessionInfo {
    ClaudeSessionInfo {
        session_id: r.session_id,
        cwd: canonical(Path::new(&r.cwd)),
        name: r.name,
        pid: r.pid,
        status: r.status,
        waiting_for: r.waiting_for,
        started_at: r.started_at,
        updated_at: r.updated_at,
    }
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        }
    })
}
fn operation(operation: &'static str, detail: impl Into<String>) -> ClaudeSessionManagerError {
    ClaudeSessionManagerError::Operation {
        operation,
        detail: detail.into(),
    }
}
fn prepend_path(shims: &Path, path: &str) -> String {
    // A trailing ':' would add an empty entry, which shells read as the
    // current directory.
    if path.is_empty() {
        shims.to_string_lossy().into_owned()
    } else {
        format!("{}:{path}", shims.to_string_lossy())
    }
}
/// RFC 3986 unreserved characters plus `/`. `~` must pass: named sessions
/// (`manager~manager`) put it in their socket path, and a session launched
/// without `BUN_INSPECT` can only be reached by typing into its pane.
fn url_path_safe(path: &str) -> bool {
    path.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/-_.~".contains(&b))
}

pub(super) struct SessionLock {
    file: fs::File,
}
impl SessionLock {
    pub(super) async fn acquire(
        lock_dir: &Path,
        key: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self, ClaudeSessionManagerError> {
        fs::create_dir_all(lock_dir)?;
        let path = lock_dir.join(format!("{key}.lock"));
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // SAFETY: flock only uses the live descriptor and LOCK_NB never blocks.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return Ok(Self { file });
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK)
                && error.raw_os_error() != Some(libc::EAGAIN)
            {
                return Err(error.into());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(operation("lock", format!("timed out waiting for {key}")));
            }
            tokio::select! { biased; _ = cancel.cancelled() => return Err(operation("lock", "cancelled waiting for session lock")), _ = tokio::time::sleep(Duration::from_millis(40)) => {} }
        }
    }
}
impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;
    use tempfile::tempdir;
    use wt_platform::process::ProcessRunner;
    use wt_tmux::TmuxServer;

    use super::super::TOKEN_HELPER_TIMEOUT;

    #[test]
    fn claude_environment_url_safety_and_path_prepend_are_deterministic() {
        assert!(url_path_safe("/tmp/cache/insp/demo.sock"));
        assert!(!url_path_safe("/tmp/cache with spaces/insp/demo.sock"));
        let named = inspector_socket_path(Path::new("/tmp/cache"), "manager~manager");
        assert!(url_path_safe(named.to_str().unwrap()));
        assert_eq!(
            prepend_path(Path::new("/cache/shims"), "/usr/bin:/bin"),
            "/cache/shims:/usr/bin:/bin"
        );
        assert_eq!(prepend_path(Path::new("/cache/shims"), ""), "/cache/shims");
    }

    #[tokio::test]
    async fn lock_filenames_match_the_existing_typescript_lock_contract() {
        let tmp = tempdir().unwrap();
        let cancel = CancellationToken::new();
        let lock = SessionLock::acquire(
            tmp.path(),
            "__claude_session__repo~review",
            Duration::from_millis(50),
            &cancel,
        )
        .await
        .unwrap();
        assert!(
            tmp.path()
                .join("__claude_session__repo~review.lock")
                .exists()
        );
        assert!(
            SessionLock::acquire(
                tmp.path(),
                "__claude_session__repo~review",
                Duration::from_millis(20),
                &cancel
            )
            .await
            .is_err()
        );
        drop(lock);
        assert!(
            SessionLock::acquire(
                tmp.path(),
                "__claude_session__repo~review",
                Duration::from_millis(50),
                &cancel
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn manager_starts_and_stops_a_registered_session_on_an_isolated_server() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("home");
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(home.join(".claude/sessions")).unwrap();
        let fake = bin.join("claude");
        fs::write(&fake, r##"#!/bin/sh
id=
name=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --session-id) id="$2"; shift 2 ;;
    --name) name="$2"; shift 2 ;;
    --resume) id="$2"; shift 2 ;;
    *) shift ;;
  esac
done
mkdir -p "$HOME/.claude/sessions"
printf '{"pid":%s,"sessionId":"%s","cwd":"%s","name":"%s","status":"idle","startedAt":1,"updatedAt":2}\n' "$$" "$id" "$PWD" "$name" > "$HOME/.claude/sessions/$$.json"
exec /bin/sleep 30
"##).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let paths = ClaudePaths::new(home, tmp.path().join("cache"));
        let harness = ClaudeHarness::new(paths.clone());
        let tmux = TmuxClient::new(
            ProcessRunner::new(NonZeroUsize::new(4).unwrap()),
            TmuxServer::at(tmp.path().join("tmux.sock"))
                .with_cwd(tmp.path())
                .with_config_file("/dev/null"),
        );
        let manager = ClaudeSessionManager::new(harness, tmux.clone())
            .with_start_timeout(Duration::from_secs(3))
            .with_spawn_path(format!("{}:/usr/bin:/bin", bin.display()));
        let cancel = CancellationToken::new();
        let target = ClaudeSessionTarget {
            slug: "repo-test".into(),
            cwd: tmp.path().to_owned(),
            managed_name: Some("review".into()),
        };
        let (started, cold) = match manager.ensure(&target, &cancel).await {
            Ok(value) => value,
            Err(error) => {
                let stderr =
                    fs::read_to_string(paths.cache_dir.join("sessions/repo-test~review.err"))
                        .unwrap_or_default();
                let registry_files = fs::read_dir(paths.sessions_dir())
                    .map(|x| x.count())
                    .unwrap_or_default();
                let sessions = tmux.list_sessions(&cancel).await.unwrap_or_default();
                let _ = tmux.kill_server(&cancel).await;
                panic!(
                    "isolated Claude start failed: {error}; stderr: {stderr:?}; registry files: {registry_files}; sessions: {sessions:?}"
                );
            }
        };
        let started_id = started.session_id;
        let started_status = started.status;
        // Keep another session alive while checking exact removal. This also
        // checks that stopping the agent does not stop an unrelated session.
        tmux.create_session(
            &CreateSession {
                name: "fixture-keeper".into(),
                cwd: tmp.path().to_owned(),
                command: vec!["/bin/sleep".into(), "60".into()],
                width: None,
                height: None,
            },
            &cancel,
        )
        .await
        .unwrap();
        let exists = tmux
            .session_exists("repo-test~review", &cancel)
            .await
            .unwrap();
        assert!(
            manager
                .stop_exact(&target, "different-session", &cancel)
                .await
                .is_err()
        );
        assert!(
            tmux.session_exists("repo-test~review", &cancel)
                .await
                .unwrap()
        );
        assert!(
            manager
                .stop_exact(&target, &started_id, &cancel)
                .await
                .unwrap()
        );
        assert!(
            !tmux
                .session_exists("repo-test~review", &cancel)
                .await
                .unwrap()
        );
        let remaining = tmux.list_sessions(&cancel).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "fixture-keeper");
        tmux.kill_server(&cancel).await.unwrap();
        assert!(cold);
        assert_eq!(
            started_id,
            claude_session_id(&fs::canonicalize(tmp.path()).unwrap(), Some("review"))
        );
        assert_eq!(started_status, RegistryStatus::Idle);
        assert!(exists);
    }

    #[cfg(unix)]
    fn executable(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    fn tmux_output(socket: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("tmux")
            .arg("-S")
            .arg(socket)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr)
    }

    /// Exercises the real tmux spawn boundary: only the selected slot gets
    /// the helper's token, stronger inherited credentials are cleared there
    /// alone, the token never reaches argv or tmux state, and every helper
    /// failure refuses the launch without creating a session.
    #[cfg(unix)]
    #[tokio::test]
    async fn launch_auth_selects_one_slot_account_without_exposing_the_token() {
        const TOKEN: &str = "sk-ant-oat01-fake-token-123";
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("home");
        let bin = tmp.path().join("bin");
        let wt_source = tmp.path().join("wt-source");
        let other = tmp.path().join("other");
        for dir in [&bin, &wt_source, &other, &home.join(".claude/sessions")] {
            fs::create_dir_all(dir).unwrap();
        }
        executable(
            &bin.join("claude"),
            r##"#!/bin/sh
id=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --session-id|--resume) id="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf '%s|%s|%s\n' "${CLAUDE_CODE_OAUTH_TOKEN-unset}" "${ANTHROPIC_API_KEY-unset}" "${ANTHROPIC_AUTH_TOKEN-unset}" > "$HOME/auth-$WT_AGENT-${PWD##*/}"
printf '{"pid":%s,"sessionId":"%s","cwd":"%s","status":"idle","startedAt":1,"updatedAt":2}\n' "$$" "$id" "$PWD" > "$HOME/.claude/sessions/$$.json"
exec /bin/sleep 30
"##,
        );
        let runs = tmp.path().join("helper-runs");
        let helper = tmp.path().join("helper-ok");
        executable(
            &helper,
            &format!(
                "#!/bin/sh\necho run >> '{}'\necho 'debug {TOKEN}' >&2\nprintf '%s\\n' '{TOKEN}'\n",
                runs.display()
            ),
        );
        let failing = tmp.path().join("helper-fail");
        executable(
            &failing,
            &format!("#!/bin/sh\necho 'keychain said {TOKEN}' >&2\necho '{TOKEN}'\nexit 3\n"),
        );
        let slow = tmp.path().join("helper-slow");
        executable(&slow, "#!/bin/sh\nexec /bin/sleep 5\n");
        let empty = tmp.path().join("helper-empty");
        executable(&empty, "#!/bin/sh\nexit 0\n");

        let socket = tmp.path().join("tmux.sock");
        let runner = ProcessRunner::new(NonZeroUsize::new(4).unwrap());
        // Inherited work credentials reach every pane through the session
        // environment, as an exported shell variable would.
        let tmux = TmuxClient::new(
            runner.clone(),
            TmuxServer::at(&socket)
                .with_cwd(tmp.path())
                .with_config_file("/dev/null"),
        )
        .with_failure_retention()
        .with_session_environment([
            ("ANTHROPIC_API_KEY".to_owned(), "work-api-key".to_owned()),
            (
                "ANTHROPIC_AUTH_TOKEN".to_owned(),
                "work-auth-token".to_owned(),
            ),
        ]);
        let paths = ClaudePaths::new(&home, tmp.path().join("cache"));
        let manager_with = |command: &Path, timeout: Duration| {
            ClaudeSessionManager::new(ClaudeHarness::new(paths.clone()), tmux.clone())
                .with_start_timeout(Duration::from_secs(5))
                .with_spawn_path(format!("{}:/usr/bin:/bin", bin.display()))
                .with_launch_auth(Some(
                    ClaudeLaunchAuth::token_command(
                        "wt",
                        &wt_source,
                        vec![command.to_string_lossy().into_owned()],
                        runner.clone(),
                    )
                    .with_timeout(timeout),
                ))
        };
        let cancel = CancellationToken::new();
        let wt = ClaudeSessionTarget {
            slug: "wt".into(),
            cwd: wt_source.clone(),
            managed_name: None,
        };
        // Same slug, other checkout: an ordinary worktree, not the wt slot.
        let lookalike = ClaudeSessionTarget {
            slug: "wt".into(),
            cwd: other.clone(),
            managed_name: Some("review".into()),
        };
        let ordinary = ClaudeSessionTarget {
            slug: "repo-one".into(),
            cwd: other.clone(),
            managed_name: None,
        };

        let result: Result<(), String> = async {
            // Every failure refuses the launch, names the helper, and keeps
            // its output (including stderr that echoes the token) private.
            for (command, timeout, expected) in [
                (&failing, TOKEN_HELPER_TIMEOUT, "exited with status 3"),
                (&slow, Duration::from_millis(300), "did not finish within"),
                (&empty, TOKEN_HELPER_TIMEOUT, "printed no token"),
                (
                    &tmp.path().join("missing"),
                    TOKEN_HELPER_TIMEOUT,
                    "could not be run",
                ),
            ] {
                let error = manager_with(command, timeout)
                    .ensure(&wt, &cancel)
                    .await
                    .err()
                    .ok_or("helper failure started Claude")?
                    .to_string();
                if !error.contains(expected) || !error.contains("did not fall back") {
                    return Err(format!("{expected}: {error}"));
                }
                if error.contains(TOKEN) {
                    return Err(format!("error exposed the token: {error}"));
                }
                if tmux
                    .session_exists("wt", &cancel)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    return Err(format!("{expected}: a session was created"));
                }
            }

            // Settings that outrank the token refuse before the helper runs.
            let settings = home.join(".claude/settings.json");
            fs::write(&settings, r#"{"apiKeyHelper":"/bin/work-key"}"#).unwrap();
            let manager = manager_with(&helper, TOKEN_HELPER_TIMEOUT);
            let error = manager
                .ensure(&wt, &cancel)
                .await
                .err()
                .ok_or("conflict started")?;
            if !error.to_string().contains("apiKeyHelper") || runs.exists() {
                return Err(format!("settings conflict: {error}"));
            }
            fs::remove_file(&settings).unwrap();

            let (_, cold) = manager
                .ensure(&wt, &cancel)
                .await
                .map_err(|e| e.to_string())?;
            let (_, other_cold) = manager
                .ensure(&ordinary, &cancel)
                .await
                .map_err(|e| e.to_string())?;
            let (_, lookalike_cold) = manager
                .ensure(&lookalike, &cancel)
                .await
                .map_err(|e| e.to_string())?;
            if !(cold && other_cold && lookalike_cold) {
                return Err("expected three cold starts".into());
            }
            let seen = |agent: &str| {
                fs::read_to_string(home.join(format!("auth-{agent}")))
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            };
            if seen("wt-wt-source").split('|').collect::<Vec<_>>() != [TOKEN, "unset", "unset"] {
                return Err(format!("wt slot saw {:?}", seen("wt-wt-source")));
            }
            for other in ["repo-one-other", "wt-other"] {
                if seen(other) != "unset|work-api-key|work-auth-token" {
                    return Err(format!("{other} saw {:?}", seen(other)));
                }
            }
            // Adopting a live slot never reruns the helper.
            manager
                .ensure(&wt, &cancel)
                .await
                .map_err(|e| e.to_string())?;
            if fs::read_to_string(&runs)
                .unwrap_or_default()
                .lines()
                .count()
                != 1
            {
                return Err("the helper ran more than once for one launch".into());
            }

            let auth_dir = paths.cache_dir.join("auth");
            if fs::read_dir(&auth_dir)
                .map(|d| d.count())
                .unwrap_or_default()
                != 0
            {
                return Err("the token handoff outlived the launch".into());
            }
            let tmux_state = [
                tmux_output(&socket, &["show-environment", "-t", "=wt"]),
                tmux_output(&socket, &["show-environment", "-g"]),
                tmux_output(
                    &socket,
                    &["list-panes", "-a", "-F", "#{pane_start_command}"],
                ),
            ]
            .join("\n");
            if tmux_state.contains(TOKEN) || !tmux_state.contains("auth/wt.") {
                return Err(format!("tmux state: {tmux_state}"));
            }
            let argv = std::process::Command::new("ps")
                .args(["-axww", "-o", "args="])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&argv.stdout).contains(TOKEN) {
                return Err("the token appears in a process argv".into());
            }
            let stderr_log =
                fs::read_to_string(paths.cache_dir.join("sessions/wt.err")).unwrap_or_default();
            if stderr_log.contains(TOKEN) {
                return Err("the token reached Claude's stderr log".into());
            }

            // The live slot's account is reported, never changed.
            let notice = |manager: &ClaudeSessionManager, target: &ClaudeSessionTarget| {
                let manager = manager.clone();
                let target = target.clone();
                let cancel = cancel.clone();
                async move { manager.launch_auth_restart_notice(&target, &cancel).await }
            };
            if notice(&manager, &wt)
                .await
                .map_err(|e| e.to_string())?
                .is_some()
                || notice(&manager, &ordinary)
                    .await
                    .map_err(|e| e.to_string())?
                    .is_some()
            {
                return Err("matching sessions need no restart".into());
            }
            let unconfigured =
                ClaudeSessionManager::new(ClaudeHarness::new(paths.clone()), tmux.clone());
            let message = notice(&unconfigured, &wt)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("removing the selector must explain the restart")?;
            if !message.contains("restart") {
                return Err(message);
            }
            let unselected = notice(&manager, &ordinary)
                .await
                .map_err(|e| e.to_string())?;
            if unselected.is_some() {
                return Err("an ordinary slot never needs an auth restart".into());
            }
            Ok(())
        }
        .await;
        let _ = tmux.kill_server(&cancel).await;
        result.unwrap();
    }

    /// A slot started before the selector was configured keeps its login;
    /// wt explains the restart instead of killing it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_live_slot_started_without_the_selector_is_kept_and_explained() {
        let tmp = tempdir().unwrap();
        let wt_source = tmp.path().join("wt-source");
        fs::create_dir_all(&wt_source).unwrap();
        let socket = tmp.path().join("tmux.sock");
        let runner = ProcessRunner::new(NonZeroUsize::new(4).unwrap());
        let tmux = TmuxClient::new(
            runner.clone(),
            TmuxServer::at(&socket)
                .with_cwd(tmp.path())
                .with_config_file("/dev/null"),
        );
        let cancel = CancellationToken::new();
        tmux.create_session(
            &CreateSession {
                name: "wt".into(),
                cwd: wt_source.clone(),
                command: vec!["/bin/sleep".into(), "30".into()],
                width: None,
                height: None,
            },
            &cancel,
        )
        .await
        .unwrap();
        let manager = ClaudeSessionManager::new(
            ClaudeHarness::new(ClaudePaths::new(
                tmp.path().join("home"),
                tmp.path().join("cache"),
            )),
            tmux.clone(),
        )
        .with_start_timeout(Duration::from_millis(500))
        .with_launch_auth(Some(ClaudeLaunchAuth::token_command(
            "wt",
            &wt_source,
            vec!["/nonexistent-helper".into()],
            runner,
        )));
        let target = ClaudeSessionTarget {
            slug: "wt".into(),
            cwd: wt_source,
            managed_name: None,
        };
        let notice = manager.launch_auth_restart_notice(&target, &cancel).await;
        // The slot never registered, so `ensure` would recycle it. The helper
        // fails, and the slot must survive: the account is selected first.
        let recycle = manager
            .ensure(&target, &cancel)
            .await
            .err()
            .map(|e| e.to_string());
        let alive = tmux.session_exists("wt", &cancel).await;
        let _ = tmux.kill_server(&cancel).await;
        let notice = notice.unwrap().expect("restart notice");
        assert!(notice.contains("normal Claude login"), "{notice}");
        let recycle = recycle.expect("a failing helper refuses the recycle");
        assert!(recycle.contains("could not be run"), "{recycle}");
        assert!(alive.unwrap());
    }

    /// Extra arguments reach only the configured slot's new `claude`, after
    /// wt's own identity flags; other slots start exactly as before.
    #[cfg(unix)]
    #[tokio::test]
    async fn launch_args_are_appended_only_in_their_slot() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("home");
        let bin = tmp.path().join("bin");
        let wt_source = tmp.path().join("wt-source");
        let other = tmp.path().join("other");
        for dir in [&bin, &wt_source, &other, &home.join(".claude/sessions")] {
            fs::create_dir_all(dir).unwrap();
        }
        executable(
            &bin.join("claude"),
            r##"#!/bin/sh
printf '%s\n' "$*" > "$HOME/args-$WT_AGENT-${PWD##*/}"
id=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --session-id|--resume) id="$2"; shift 2 ;;
    *) shift ;;
  esac
done
printf '{"pid":%s,"sessionId":"%s","cwd":"%s","status":"idle","startedAt":1,"updatedAt":2}\n' "$$" "$id" "$PWD" > "$HOME/.claude/sessions/$$.json"
exec /bin/sleep 30
"##,
        );
        let tmux = TmuxClient::new(
            ProcessRunner::new(NonZeroUsize::new(4).unwrap()),
            TmuxServer::at(tmp.path().join("tmux.sock"))
                .with_cwd(tmp.path())
                .with_config_file("/dev/null"),
        );
        let manager = ClaudeSessionManager::new(
            ClaudeHarness::new(ClaudePaths::new(&home, tmp.path().join("cache"))),
            tmux.clone(),
        )
        .with_start_timeout(Duration::from_secs(5))
        .with_spawn_path(format!("{}:/usr/bin:/bin", bin.display()))
        .with_launch_args(Some(ClaudeLaunchArgs::new(
            "wt",
            &wt_source,
            vec!["--permission-mode".into(), "accept edits".into()],
        )));
        let cancel = CancellationToken::new();
        let target = |slug: &str, cwd: &Path| ClaudeSessionTarget {
            slug: slug.into(),
            cwd: cwd.to_owned(),
            managed_name: None,
        };
        let started = async {
            manager.ensure(&target("wt", &wt_source), &cancel).await?;
            manager.ensure(&target("repo-one", &other), &cancel).await
        }
        .await;
        let _ = tmux.kill_server(&cancel).await;
        started.unwrap();
        let wt = fs::read_to_string(home.join("args-wt-wt-source")).unwrap();
        let ordinary = fs::read_to_string(home.join("args-repo-one-other")).unwrap();
        assert!(wt.starts_with("--name wt "), "{wt}");
        assert!(
            wt.trim_end().ends_with("--permission-mode accept edits"),
            "{wt}"
        );
        assert!(!ordinary.contains("--permission-mode"), "{ordinary}");
    }
}
