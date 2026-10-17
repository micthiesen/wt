//! Launch-time account selection for one exact Claude slot.
//!
//! A configured helper prints a Claude subscription token. wt runs it only
//! when it creates the selected slot's Claude process, hands the token to
//! that pane through a one-shot private file, and exports it there as
//! `CLAUDE_CODE_OAUTH_TOKEN` immediately before `exec`ing Claude. The token
//! never enters argv, tmux's session environment, logs, or error text, and a
//! failure never falls back to the account of the normal login.

use std::{
    ffi::OsString,
    fmt, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessError, ProcessRunner};

use super::ClaudeSessionTarget;

/// The variable Claude reads a `claude setup-token` credential from.
pub const CLAUDE_OAUTH_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
/// Environment that Claude ranks above `CLAUDE_CODE_OAUTH_TOKEN`. The
/// selected launch removes these so an inherited work credential cannot win.
pub const OVERRIDING_AUTH_ENV: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
];
/// Environment that would send the selected token somewhere other than
/// Anthropic. A work gateway configured for every session must not receive
/// the selected account's credential.
pub const REDIRECTING_ENV: &[&str] = &["ANTHROPIC_BASE_URL", "ANTHROPIC_CUSTOM_HEADERS"];
/// Prefix of Claude's provider switches, which all outrank the token.
const PROVIDER_SWITCH_PREFIX: &str = "CLAUDE_CODE_USE_";
/// Session option recording that a live slot was started with the helper.
pub const LAUNCH_AUTH_OPTION: &str = "@wt-claude-auth";
pub(super) const LAUNCH_AUTH_TOKEN_COMMAND: &str = "token-command";
pub const TOKEN_HELPER_TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_OUTPUT_LIMIT: usize = 8 * 1024;
const CONFIG_KEY: &str = "harness.wt_session_claude_token_command";
/// Exit status of the pane when its token handoff is missing.
pub(super) const MISSING_TOKEN_EXIT: u8 = 78;

/// Selects the account of one exact Claude slot, identified by its slug and
/// canonical checkout, through a token helper.
#[derive(Clone)]
pub struct ClaudeLaunchAuth {
    slug: String,
    cwd: PathBuf,
    command: Vec<String>,
    timeout: Duration,
    runner: ProcessRunner,
}

impl fmt::Debug for ClaudeLaunchAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaudeLaunchAuth")
            .field("slug", &self.slug)
            .field("cwd", &self.cwd)
            .field("command", &self.command)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl ClaudeLaunchAuth {
    /// `command` is executed directly (no shell) with no stdin.
    pub fn token_command(
        slug: impl Into<String>,
        cwd: &Path,
        command: Vec<String>,
        runner: ProcessRunner,
    ) -> Self {
        Self {
            slug: slug.into(),
            cwd: fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_owned()),
            command,
            timeout: TOKEN_HELPER_TIMEOUT,
            runner,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// A worktree that happens to share the slug has another checkout, so
    /// both identity parts must match.
    pub fn applies_to(&self, target: &ClaudeSessionTarget) -> bool {
        target.slug == self.slug
            && fs::canonicalize(&target.cwd).unwrap_or_else(|_| target.cwd.clone()) == self.cwd
    }

    fn program(&self) -> &str {
        self.command.first().map(String::as_str).unwrap_or_default()
    }

    /// Run the helper once and validate what it printed. Helper output is
    /// never included in an error: stdout is the secret, and stderr may
    /// echo it.
    pub async fn fetch(&self, cancel: &CancellationToken) -> Result<ClaudeToken, ClaudeAuthError> {
        let program = self.program().to_owned();
        let Some((first, args)) = self.command.split_first() else {
            return Err(ClaudeAuthError::new(
                &program,
                "no helper program is configured",
            ));
        };
        let mut spec = CommandSpec::new(first).args(args);
        spec.timeout = self.timeout;
        spec.output_limit = TOKEN_OUTPUT_LIMIT;
        // The helper must not receive a token that a parent process exported.
        spec.env
            .push((OsString::from(CLAUDE_OAUTH_TOKEN_ENV), None));
        let output = self.runner.run(spec, cancel).await.map_err(|error| {
            let reason = match error {
                ProcessError::Io { source, .. } => format!("could not be run: {source}"),
                ProcessError::Cancelled { .. } => "was cancelled".to_owned(),
                ProcessError::Timeout { timeout, .. } => {
                    format!("did not finish within {}s", timeout.as_secs_f32())
                }
                ProcessError::OutputLimit { stream, limit, .. } => {
                    format!("printed more than {limit} bytes on {stream}")
                }
                ProcessError::PipeDrain { .. } => "kept its output open after exiting".to_owned(),
                ProcessError::ObserverPanicked { .. } | ProcessError::Exit { .. } => {
                    "failed".to_owned()
                }
            };
            ClaudeAuthError::new(&program, reason)
        })?;
        if !output.status.success() {
            let status = output
                .status
                .code()
                .map(|code| format!("status {code}"))
                .unwrap_or_else(|| "a signal".to_owned());
            let withheld = if output.stderr.is_empty() {
                String::new()
            } else {
                format!("; {} bytes of stderr withheld", output.stderr.len())
            };
            return Err(ClaudeAuthError::new(
                &program,
                format!("exited with {status}{withheld}"),
            ));
        }
        parse_token(output.stdout).map_err(|reason| ClaudeAuthError::new(&program, reason))
    }
}

/// A validated token. Its value is reachable only to write the handoff file.
pub struct ClaudeToken(String);

impl fmt::Debug for ClaudeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClaudeToken(<redacted>)")
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct ClaudeAuthError {
    message: String,
}

impl ClaudeAuthError {
    fn new(program: &str, reason: impl fmt::Display) -> Self {
        Self {
            message: format!(
                "the wt session's Claude token helper ({program}) {reason}. Claude was not started, and wt did not fall back to the normal login; fix the helper named by {CONFIG_KEY} (or remove that setting) and try again"
            ),
        }
    }

    pub(super) fn handoff(error: std::io::Error) -> Self {
        Self {
            message: format!(
                "wt could not prepare the wt session's selected Claude token ({error}). Claude was not started, and wt did not fall back to the normal login"
            ),
        }
    }

    fn conflict(path: &Path, key: &str) -> Self {
        Self {
            message: format!(
                "Claude settings {} set {key}, which takes precedence over the wt session's selected token ({CONFIG_KEY}). Claude was not started; remove {key} from those settings or remove {CONFIG_KEY}",
                path.display()
            ),
        }
    }
}

/// One trailing newline (LF or CRLF) is the normal end of a line. Anything
/// else that is not visible ASCII is a malformed token.
fn parse_token(mut stdout: Vec<u8>) -> Result<ClaudeToken, &'static str> {
    if stdout.last() == Some(&b'\n') {
        stdout.pop();
        if stdout.last() == Some(&b'\r') {
            stdout.pop();
        }
    }
    if stdout.is_empty() {
        return Err("printed no token");
    }
    if stdout.contains(&b'\n') {
        return Err("printed more than one line; it must print only the token");
    }
    if !stdout.iter().all(|byte| (0x21..=0x7e).contains(byte)) {
        return Err("printed a token containing spaces, control characters, or non-ASCII bytes");
    }
    String::from_utf8(stdout)
        .map(ClaudeToken)
        .map_err(|_| "printed a token that is not ASCII")
}

/// Settings Claude reads for this checkout. `apiKeyHelper` and an `env`
/// entry for a stronger credential (or for the token itself) would replace
/// the selected account inside Claude, after wt can clear anything.
pub fn settings_files(home: &Path, cwd: &Path, config_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut files = vec![home.join(".claude").join("settings.json")];
    if let Some(dir) = config_dir.filter(|dir| !dir.as_os_str().is_empty()) {
        files.push(dir.join("settings.json"));
    }
    files.push(cwd.join(".claude").join("settings.json"));
    files.push(cwd.join(".claude").join("settings.local.json"));
    let managed = if cfg!(target_os = "macos") {
        Path::new("/Library/Application Support/ClaudeCode")
    } else {
        Path::new("/etc/claude-code")
    };
    files.push(managed.join("managed-settings.json"));
    files.extend(json_files(&managed.join("managed-settings.d")));
    files
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
}

/// Unreadable or malformed files are left to Claude, which reports them
/// itself; they cannot set a credential it would use.
pub fn check_settings(files: &[PathBuf]) -> Result<(), ClaudeAuthError> {
    for path in files {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(settings) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if settings
            .get("apiKeyHelper")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|helper| !helper.trim().is_empty())
        {
            return Err(ClaudeAuthError::conflict(path, "apiKeyHelper"));
        }
        if let Some(env) = settings.get("env").and_then(serde_json::Value::as_object) {
            for (key, value) in env {
                let refused = key.starts_with(PROVIDER_SWITCH_PREFIX)
                    || OVERRIDING_AUTH_ENV
                        .iter()
                        .chain(REDIRECTING_ENV)
                        .chain(&[CLAUDE_OAUTH_TOKEN_ENV])
                        .any(|name| name == key);
                let set = match value {
                    serde_json::Value::Null => false,
                    serde_json::Value::String(value) => !value.is_empty(),
                    _ => true,
                };
                if refused && set {
                    return Err(ClaudeAuthError::conflict(path, &format!("env.{key}")));
                }
            }
        }
    }
    Ok(())
}

/// Remove handoffs older than `max_age`. Each is consumed by its pane or
/// dropped by its launch within the start timeout, so an old one is left
/// over from a process that died mid-launch.
pub(super) fn reap_stale_handoffs(dir: &Path, max_age: Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_none_or(|age| age >= max_age);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The one-shot file that carries a token into one pane. The pane removes
/// it as soon as it has read it; dropping the handle removes it otherwise.
pub(super) struct TokenHandoff {
    path: PathBuf,
}

impl TokenHandoff {
    pub(super) fn write(dir: &Path, tmux_name: &str, token: &ClaudeToken) -> std::io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // The caller holds the slot's start lock, so any earlier handoff for
        // this slot belongs to a launch that is no longer running.
        let prefix = format!("{tmux_name}.");
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let name = format!(
            "{prefix}{}.{nanos}.{}.token",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let mut contents = Vec::with_capacity(token.0.len() + 1);
        contents.extend_from_slice(token.0.as_bytes());
        contents.push(b'\n');
        let path = crate::persist::write_private_launch_file(dir, &name, &contents)?;
        Ok(Self { path })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TokenHandoff {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The pane-side reader. It runs inside the pane, reads and deletes the
/// handoff, refuses to start Claude without a token, and exports the token
/// only for the `exec`ed Claude process. Provider switches added after this
/// list was written are cleared by prefix. `$1` is Claude's stderr file, `$2`
/// the handoff, and the rest Claude's argv.
pub(super) fn pane_script() -> String {
    format!(
        r#"p=$1; t=$2; shift 2
for v in $(compgen -e); do
  case $v in {PROVIDER_SWITCH_PREFIX}*) unset "$v" ;; esac
done
IFS= read -r {CLAUDE_OAUTH_TOKEN_ENV} < "$t"; s=$?
rm -f -- "$t"
if [ "$s" -ne 0 ] || [ -z "${CLAUDE_OAUTH_TOKEN_ENV}" ]; then
  echo "wt: the selected Claude account token for this session was not delivered; Claude was not started. Start the session again from wt." >&2
  exit {MISSING_TOKEN_EXIT}
fi
export {CLAUDE_OAUTH_TOKEN_ENV}
exec "$@" 2> "$p""#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_output_accepts_one_line_and_rejects_everything_else() {
        assert_eq!(
            parse_token(b"sk-ant-oat01-abc".to_vec()).unwrap().0,
            "sk-ant-oat01-abc"
        );
        assert_eq!(parse_token(b"tok\n".to_vec()).unwrap().0, "tok");
        assert_eq!(parse_token(b"tok\r\n".to_vec()).unwrap().0, "tok");
        for bad in [
            &b""[..],
            b"\n",
            b"\r\n",
            b"tok\n\n",
            b"tok\nsecond",
            b"to k",
            b" tok",
            b"tok\t",
            b"tok\r",
            b"t\x00k",
            "tök".as_bytes(),
        ] {
            assert!(parse_token(bad.to_vec()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn redacted_debug_never_prints_the_token() {
        let token = parse_token(b"super-secret-token".to_vec()).unwrap();
        assert!(!format!("{token:?}").contains("super-secret"));
    }

    #[test]
    fn settings_that_outrank_the_token_refuse_the_launch() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let missing = dir.path().join("missing.json");
        let check = |body: &str| {
            fs::write(&file, body).unwrap();
            check_settings(&[missing.clone(), file.clone()])
        };
        assert!(check(r#"{"env":{"FOO":"1"},"model":"opus"}"#).is_ok());
        assert!(check("not json").is_ok());
        assert!(check(r#"{"apiKeyHelper":""}"#).is_ok());
        assert!(check(r#"{"env":{"ANTHROPIC_API_KEY":""}}"#).is_ok());
        for (body, key) in [
            (r#"{"apiKeyHelper":"/bin/key"}"#, "apiKeyHelper"),
            (
                r#"{"env":{"ANTHROPIC_API_KEY":"sk-x"}}"#,
                "env.ANTHROPIC_API_KEY",
            ),
            (
                r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"x"}}"#,
                "env.ANTHROPIC_AUTH_TOKEN",
            ),
            (
                r#"{"env":{"CLAUDE_CODE_USE_BEDROCK":"1"}}"#,
                "env.CLAUDE_CODE_USE_BEDROCK",
            ),
            (
                r#"{"env":{"CLAUDE_CODE_OAUTH_TOKEN":"x"}}"#,
                "env.CLAUDE_CODE_OAUTH_TOKEN",
            ),
        ] {
            let error = check(body).unwrap_err().to_string();
            assert!(error.contains(key), "{body}: {error}");
            assert!(!error.contains("sk-x"), "{error}");
        }
    }

    #[test]
    fn settings_files_cover_user_config_dir_project_and_managed() {
        let files = settings_files(Path::new("/h"), Path::new("/w"), Some(Path::new("/cfg")));
        assert!(files.contains(&PathBuf::from("/h/.claude/settings.json")));
        assert!(files.contains(&PathBuf::from("/cfg/settings.json")));
        assert!(files.contains(&PathBuf::from("/w/.claude/settings.json")));
        assert!(files.contains(&PathBuf::from("/w/.claude/settings.local.json")));
        assert!(
            files
                .iter()
                .any(|file| file.ends_with("managed-settings.json"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn handoff_is_private_one_shot_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let auth = dir.path().join("auth");
        let token = parse_token(b"tok-1".to_vec()).unwrap();
        let stale = {
            fs::create_dir_all(&auth).unwrap();
            let stale = auth.join("wt-claude.old.token");
            fs::write(&stale, "old\n").unwrap();
            stale
        };
        let handoff = TokenHandoff::write(&auth, "wt-claude", &token).unwrap();
        assert!(!stale.exists(), "an earlier handoff for the slot is reaped");
        let mode = fs::metadata(handoff.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            fs::metadata(&auth).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(fs::read_to_string(handoff.path()).unwrap(), "tok-1\n");
        let path = handoff.path().to_owned();
        drop(handoff);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn pane_script_exports_only_a_delivered_token_and_deletes_the_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let token = parse_token(b"tok-2".to_vec()).unwrap();
        let handoff = TokenHandoff::write(dir.path(), "wt-claude", &token).unwrap();
        let err = dir.path().join("claude.err");
        let run = |handoff: &Path| {
            std::process::Command::new("bash")
                .args(["-c", &pane_script(), "_"])
                .arg(&err)
                .arg(handoff)
                .args([
                    "/bin/sh",
                    "-c",
                    "printf %s \"$CLAUDE_CODE_OAUTH_TOKEN${CLAUDE_CODE_USE_FUTURE-}\"",
                ])
                .env("CLAUDE_CODE_OAUTH_TOKEN", "inherited-work-token")
                .env("CLAUDE_CODE_USE_FUTURE", "1")
                .output()
                .unwrap()
        };
        let output = run(handoff.path());
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "tok-2");
        assert!(!handoff.path().exists(), "the pane deletes the handoff");
        // A missing handoff stops the launch instead of using any other token.
        let output = run(handoff.path());
        assert_eq!(output.status.code(), Some(MISSING_TOKEN_EXIT as i32));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("was not started"),
            "{output:?}"
        );
    }
}
