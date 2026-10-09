use std::{
    fs,
    os::{fd::AsRawFd, unix::fs::FileTypeExt},
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_tmux::{CreateSession, PaneTarget, TmuxClient, TmuxError};

use super::{
    ClaudeHarness, ClaudePaths, RegistrySession, RegistryStatus, claude_tmux_name,
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
        let sessions = self.tmux.list_sessions(cancel).await?;
        let live: std::collections::HashSet<_> =
            sessions.into_iter().map(|session| session.name).collect();
        let dir = self.paths.cache_dir.join("insp");
        let mut removed = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(removed),
            Err(error) => return Err(error.into()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("sock") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|x| x.to_str()) else {
                continue;
            };
            if !live.contains(name) && fs::remove_file(&path).is_ok() {
                removed.push(path);
            }
        }
        Ok(removed)
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
        let started = if existed {
            true
        } else {
            match self.create(target, &name, cancel).await {
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
            self.tmux.kill_session(&name, cancel).await?;
            self.prepare_inspector(&name)?;
            self.create(target, &name, cancel).await?;
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

    async fn create(
        &self,
        target: &ClaudeSessionTarget,
        tmux_name: &str,
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
        ensure_inspect_shims_at(&self.paths.cache_dir.join("shims"), &path)?;
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
        command.push(format!("PATH={shim_path}"));
        command.push(format!("HOME={}", self.paths.home.to_string_lossy()));
        command.push(format!("WT_AGENT={}", target.slug));
        if let Some(url) = inspector_url {
            command.push(format!("BUN_INSPECT={url}"));
        }
        command.extend([
            "bash".into(),
            "-c".into(),
            "p=$1; shift; exec \"$@\" 2> \"$p\"".into(),
            "_".into(),
            stderr_path.to_string_lossy().into_owned(),
            spawn.program.to_string_lossy().into_owned(),
        ]);
        command.extend(spawn.args);
        let create = CreateSession {
            name: tmux_name.into(),
            cwd: canonical(&target.cwd),
            command,
            width: Some(200),
            height: Some(50),
        };
        self.tmux.create_session(&create, cancel).await?;
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
    format!(
        "{}{}{}",
        shims.to_string_lossy(),
        std::env::split_paths(path)
            .next()
            .map(|_| ":")
            .unwrap_or(":"),
        path
    )
}
fn url_path_safe(path: &str) -> bool {
    path.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
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

    #[test]
    fn claude_environment_url_safety_and_path_prepend_are_deterministic() {
        assert!(url_path_safe("/tmp/cache/insp/demo.sock"));
        assert!(!url_path_safe("/tmp/cache with spaces/insp/demo.sock"));
        assert_eq!(
            prepend_path(Path::new("/cache/shims"), "/usr/bin:/bin"),
            "/cache/shims:/usr/bin:/bin"
        );
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
        let exists = tmux
            .session_exists("repo-test~review", &cancel)
            .await
            .unwrap();
        manager.stop(&target, &cancel).await.unwrap();
        let _ = tmux.kill_server(&cancel).await;
        assert!(cold);
        assert_eq!(
            started_id,
            claude_session_id(&fs::canonicalize(tmp.path()).unwrap(), Some("review"))
        );
        assert_eq!(started_status, RegistryStatus::Idle);
        assert!(exists);
    }
}
