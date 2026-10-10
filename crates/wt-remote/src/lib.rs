//! SSH transport and versioned worker protocol for native remote wt hosts.

use std::ffi::OsString;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_config::RemoteConfig;
use wt_core::WorkStatusRecord;
use wt_platform::process::{CommandSpec, ProcessError, ProcessOutput, ProcessRunner};

pub const WORKER_PROTOCOL_VERSION: u32 = 3;
const SSH_TIMEOUT: Duration = Duration::from_secs(15);

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub role: WorkerRole,
    pub protocol: u32,
    pub build: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerRole {
    Controller,
    Worker,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerSnapshot {
    pub protocol: u32,
    pub worktrees: Vec<WorktreeSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorktreeSnapshot {
    pub slug: String,
    pub branch: String,
    pub base: String,
    pub path: String,
    pub stage: String,
    pub deployed: bool,
    pub exists: bool,
    pub status: WorktreeStatus,
    #[serde(deserialize_with = "required_nullable")]
    pub dev: Option<DevServerStatus>,
    #[serde(rename = "devError", default, skip_serializing_if = "Option::is_none")]
    pub dev_error: Option<String>,
    pub dirty: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub unpushed: Option<f64>,
    #[serde(deserialize_with = "required_nullable")]
    pub pushed: Option<bool>,
    #[serde(rename = "aheadOfBase")]
    #[serde(deserialize_with = "required_nullable")]
    pub ahead_of_base: Option<f64>,
    #[serde(rename = "issueId")]
    #[serde(deserialize_with = "required_nullable")]
    pub issue_id: Option<String>,
    #[serde(rename = "issueUrl")]
    #[serde(deserialize_with = "required_nullable")]
    pub issue_url: Option<String>,
    #[serde(rename = "githubIssue")]
    #[serde(deserialize_with = "required_nullable")]
    pub github_issue: Option<u64>,
    #[serde(rename = "githubIssueUrl")]
    #[serde(deserialize_with = "required_nullable")]
    pub github_issue_url: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub work: Option<WorkStatusRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeStatus {
    pub kind: StatusKind,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusKind {
    Busy,
    Missing,
    Gone,
    Merged,
    Dirty,
    Clean,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DevServerStatus {
    pub running: bool,
    pub starting: bool,
    pub crashed: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub port: Option<u16>,
    #[serde(deserialize_with = "required_nullable")]
    pub url: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub since: Option<f64>,
    #[serde(deserialize_with = "required_nullable")]
    pub waiting: Option<DevServerWaiting>,
    #[serde(rename = "rebasedSince")]
    #[serde(deserialize_with = "required_nullable")]
    pub rebased_since: Option<bool>,
    #[serde(deserialize_with = "required_nullable")]
    pub restarts: Option<DevServerRestarts>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DevServerWaiting {
    pub rank: i64,
    pub since: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevServerRestarts {
    pub count: i64,
    #[serde(rename = "lastExit")]
    pub last_exit: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerCommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedRemoteSession {
    pub program: OsString,
    pub args: Vec<OsString>,
}

#[derive(Debug, Error)]
pub enum RemoteError {
    #[error("remote SSH transport failed: {0}")]
    Transport(#[from] ProcessError),
    #[error("remote worker protocol error: {0}")]
    Protocol(String),
    #[error(
        "remote worker {host} is configured as {role:?}; set [instance] role = \"worker\" there"
    )]
    WrongRole { host: String, role: WorkerRole },
    #[error(
        "remote {host} uses protocol {actual}; install the matching wt release on the worker (this controller requires protocol {expected})"
    )]
    ProtocolMismatch {
        host: String,
        actual: u32,
        expected: u32,
    },
    #[error("remote worker command failed: {detail}")]
    Command { detail: String },
}

#[derive(Clone)]
pub struct RemoteClient {
    runner: ProcessRunner,
    remote: RemoteConfig,
    ssh_program: OsString,
}

impl RemoteClient {
    pub fn new(runner: ProcessRunner, remote: RemoteConfig) -> Self {
        Self {
            runner,
            remote,
            ssh_program: OsString::from("ssh"),
        }
    }

    /// Injects a test SSH executable. Production callers use `ssh` from PATH.
    pub fn with_ssh_program(mut self, program: impl Into<OsString>) -> Self {
        self.ssh_program = program.into();
        self
    }

    pub fn remote(&self) -> &RemoteConfig {
        &self.remote
    }

    pub async fn hello(&self, cancellation: &CancellationToken) -> Result<WorkerInfo, RemoteError> {
        let output = self.run_raw(&["_hello".to_owned()], cancellation).await?;
        let output = check_success(output, "remote worker handshake")?;
        parse_worker_info(&output.stdout_text())
    }

    pub async fn require_worker(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<WorkerInfo, RemoteError> {
        let info = self.hello(cancellation).await?;
        if info.role != WorkerRole::Worker {
            return Err(RemoteError::WrongRole {
                host: self.remote.label.clone(),
                role: info.role,
            });
        }
        if info.protocol != WORKER_PROTOCOL_VERSION {
            return Err(RemoteError::ProtocolMismatch {
                host: self.remote.label.clone(),
                actual: info.protocol,
                expected: WORKER_PROTOCOL_VERSION,
            });
        }
        Ok(info)
    }

    pub async fn snapshot(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<WorkerSnapshot, RemoteError> {
        self.require_worker(cancellation).await?;
        let output = self
            .run_raw(&["_snapshot".to_owned()], cancellation)
            .await?;
        let output = check_success(output, "remote worker snapshot")?;
        parse_worker_snapshot(&output.stdout_text())
    }

    pub async fn run_worker(
        &self,
        args: &[String],
        cancellation: &CancellationToken,
    ) -> Result<WorkerCommandOutput, RemoteError> {
        self.require_worker(cancellation).await?;
        let output = self.run_raw(args, cancellation).await?;
        Ok(command_output(output))
    }

    pub async fn run_raw(
        &self,
        args: &[String],
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, RemoteError> {
        let mut spec =
            CommandSpec::new(self.ssh_program.clone()).args(ssh_args(&self.remote, args));
        spec.timeout = SSH_TIMEOUT;
        Ok(self.runner.run(spec, cancellation).await?)
    }

    pub fn interactive_tui(&self) -> PreparedRemoteSession {
        PreparedRemoteSession {
            program: self.ssh_program.clone(),
            args: interactive_ssh_args(&self.remote, None),
        }
    }

    pub fn interactive_session(
        &self,
        slug: &str,
        target: &str,
        harness: Option<&str>,
    ) -> PreparedRemoteSession {
        let mut argv = vec!["_session".to_owned(), slug.to_owned(), target.to_owned()];
        if let Some(harness) = harness {
            argv.push(harness.to_owned());
        }
        PreparedRemoteSession {
            program: self.ssh_program.clone(),
            args: interactive_ssh_args(&self.remote, Some(&argv)),
        }
    }
}

pub fn encode_remote_args(argv: &[String]) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(argv).expect("string vector always serializes"))
}

pub fn decode_remote_args(payload: &str) -> Result<Vec<String>, RemoteError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| RemoteError::Protocol("invalid remote argv payload".into()))?;
    serde_json::from_slice(&bytes).map_err(|_| {
        RemoteError::Protocol("remote argv payload must be an array of strings".into())
    })
}

pub fn remote_wt_command(remote: &RemoteConfig, argv: &[String]) -> String {
    format!(
        "exec {} _remote {}",
        remote_executable(&remote.wt_path),
        encode_remote_args(argv)
    )
}

pub fn ssh_args(remote: &RemoteConfig, argv: &[String]) -> Vec<OsString> {
    let command = remote_wt_command(remote, argv);
    [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=3",
    ]
    .into_iter()
    .chain([remote.host.as_str(), command.as_str()])
    .map(OsString::from)
    .collect()
}

pub fn interactive_ssh_args(remote: &RemoteConfig, argv: Option<&[String]>) -> Vec<OsString> {
    let command = match argv {
        Some(argv) => remote_wt_command(remote, argv),
        None => format!("exec {}", remote_executable(&remote.wt_path)),
    };
    [
        "-t",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=3",
    ]
    .into_iter()
    .chain([remote.host.as_str(), command.as_str()])
    .map(OsString::from)
    .collect()
}

pub fn parse_worker_info(raw: &str) -> Result<WorkerInfo, RemoteError> {
    serde_json::from_str(raw.trim())
        .map_err(|error| RemoteError::Protocol(format!("invalid worker handshake JSON: {error}")))
}

pub fn parse_worker_snapshot(raw: &str) -> Result<WorkerSnapshot, RemoteError> {
    let mut value = parse_json_payload(raw)?;
    let protocol = value
        .get("protocol")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| RemoteError::Protocol("snapshot protocol must be an integer".into()))?;
    if protocol != WORKER_PROTOCOL_VERSION {
        return Err(RemoteError::Protocol(format!(
            "remote worker snapshot uses protocol {protocol}; expected {WORKER_PROTOCOL_VERSION}"
        )));
    }
    let rows = value
        .get_mut("worktrees")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            RemoteError::Protocol("remote worker snapshot worktrees is not an array".into())
        })?;
    let mut worktrees = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter_mut().enumerate() {
        let object = row.as_object_mut().ok_or_else(|| {
            RemoteError::Protocol(format!("remote worker snapshot {index} is invalid"))
        })?;
        match object.get("work") {
            Some(Value::Null) => {}
            Some(raw_work) => {
                let parsed = wt_core::parse_work_status(raw_work).ok_or_else(|| {
                    RemoteError::Protocol(format!("remote worker snapshot {index}.work is invalid"))
                })?;
                object.insert(
                    "work".into(),
                    serde_json::to_value(parsed).expect("work status serializes"),
                );
            }
            None => {
                return Err(RemoteError::Protocol(format!(
                    "remote worker snapshot {index}.work is invalid"
                )));
            }
        }
        worktrees.push(serde_json::from_value(row.clone()).map_err(|error| {
            RemoteError::Protocol(format!(
                "remote worker snapshot {index} is invalid: {error}"
            ))
        })?);
    }
    Ok(WorkerSnapshot {
        protocol,
        worktrees,
    })
}

fn parse_json_payload(raw: &str) -> Result<Value, RemoteError> {
    if let Ok(value) = serde_json::from_str(raw.trim()) {
        return Ok(value);
    }
    let start = raw.find('{').ok_or_else(|| no_snapshot_json(raw))?;
    let end = raw
        .rfind('}')
        .filter(|end| *end > start)
        .ok_or_else(|| no_snapshot_json(raw))?;
    serde_json::from_str(&raw[start..=end]).map_err(|_| no_snapshot_json(raw))
}

fn no_snapshot_json(raw: &str) -> RemoteError {
    RemoteError::Protocol(format!(
        "remote worker snapshot did not return JSON. Got: {}",
        raw.trim()
            .chars()
            .take(200)
            .collect::<String>()
            .if_empty("(empty)")
    ))
}

fn check_success(output: ProcessOutput, operation: &str) -> Result<ProcessOutput, RemoteError> {
    if output.status.success() {
        Ok(output)
    } else {
        let detail = if !output.stderr_text().trim().is_empty() {
            output.stderr_text().trim().to_owned()
        } else if !output.stdout_text().trim().is_empty() {
            output.stdout_text().trim().to_owned()
        } else {
            format!(
                "SSH exited with {}",
                output
                    .status
                    .code()
                    .map_or("unknown".into(), |v| v.to_string())
            )
        };
        Err(RemoteError::Command {
            detail: format!("{operation}: {detail}"),
        })
    }
}

fn command_output(output: ProcessOutput) -> WorkerCommandOutput {
    WorkerCommandOutput {
        exit_code: output.status.code(),
        stdout: output.stdout_text(),
        stderr: output.stderr_text(),
    }
}

fn remote_executable(path: &str) -> String {
    if path == "~" {
        return "\"$HOME\"".into();
    }
    if let Some(suffix) = path.strip_prefix("~/") {
        return format!("\"$HOME\"/{}", shell_quote(suffix));
    }
    shell_quote(path)
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.into()
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn remote() -> RemoteConfig {
        RemoteConfig {
            host: "worker.example".into(),
            label: "worker".into(),
            wt_path: "~/.wt/bin/wt".into(),
        }
    }

    #[test]
    fn argv_round_trips_shell_sensitive_text() {
        let argv = vec!["agent".into(), "a b 'c' $HOME && x".into()];
        assert_eq!(
            decode_remote_args(&encode_remote_args(&argv)).unwrap(),
            argv
        );
    }

    #[test]
    fn command_quotes_path_and_hides_argv() {
        let mut remote = remote();
        remote.wt_path = "~/odd path/wt's binary".into();
        let args = vec!["new".into(), "a&b".into()];
        let command = remote_wt_command(&remote, &args);
        assert!(command.starts_with("exec \"$HOME\"/'odd path/wt'\\''s binary' _remote "));
        assert!(!command.contains("a&b"));
    }

    #[test]
    fn interactive_session_allocates_tty_without_batch_mode() {
        let client = RemoteClient::new(ProcessRunner::default(), remote());
        let session = client.interactive_session("slug", "harness", Some("codex"));
        let args = session
            .args
            .iter()
            .map(|v| v.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(args.iter().any(|arg| arg == "-t"));
        assert!(!args.iter().any(|arg| arg == "BatchMode=yes"));
        assert!(args.last().unwrap().contains("_remote"));
        assert!(!args.last().unwrap().contains("slug"));
    }

    #[test]
    fn parses_worker_handshake_and_rejects_bad_contract() {
        let info = parse_worker_info(r#"{"role":"worker","protocol":3,"build":"abc"}"#).unwrap();
        assert_eq!(info.role, WorkerRole::Worker);
        assert!(parse_worker_info(r#"{"role":"worker","build":"abc"}"#).is_err());
    }

    #[test]
    fn parses_strict_snapshot_and_accepts_login_banner() {
        let raw = r#"banner
{"protocol":3,"worktrees":[{"slug":"one","branch":"a/one","base":"main","path":"/tmp/one","stage":"one","deployed":false,"exists":true,"status":{"kind":"clean","label":"clean"},"dev":null,"dirty":false,"unpushed":0,"pushed":true,"aheadOfBase":1,"issueId":null,"issueUrl":null,"githubIssue":null,"githubIssueUrl":null,"work":null}]}
"#;
        let parsed = parse_worker_snapshot(raw).unwrap();
        assert_eq!(parsed.worktrees.len(), 1);
        assert_eq!(parsed.worktrees[0].slug, "one");
    }

    #[test]
    fn rejects_version_shape_and_invalid_work() {
        let wrong = r#"{"protocol":2,"worktrees":[]}"#;
        assert!(
            parse_worker_snapshot(wrong)
                .unwrap_err()
                .to_string()
                .contains("uses protocol 2")
        );
        let future_field = r#"{"protocol":3,"worktrees":[],"newField":true}"#;
        assert!(parse_worker_snapshot(future_field).is_ok());
        let missing_nullable = r#"{"protocol":3,"worktrees":[{"slug":"x","branch":"x","base":"main","path":"/tmp/x","stage":"x","deployed":false,"exists":true,"status":{"kind":"clean","label":"clean"},"dev":{"running":false,"starting":false,"crashed":false,"url":null,"since":null,"waiting":null,"rebasedSince":null,"restarts":null},"dirty":false,"unpushed":0,"pushed":true,"aheadOfBase":null,"issueId":null,"issueUrl":null,"githubIssue":null,"githubIssueUrl":null,"work":null}]}"#;
        assert!(parse_worker_snapshot(missing_nullable).is_err());
        let invalid_work = r#"{"protocol":3,"worktrees":[{"slug":"x","branch":"x","base":"main","path":"/tmp/x","stage":"x","deployed":false,"exists":true,"status":{"kind":"clean","label":"clean"},"dev":null,"dirty":false,"unpushed":0,"pushed":true,"aheadOfBase":null,"issueId":null,"issueUrl":null,"githubIssue":null,"githubIssueUrl":null,"work":{"bad":true}}]}"#;
        assert!(
            parse_worker_snapshot(invalid_work)
                .unwrap_err()
                .to_string()
                .contains("work is invalid")
        );
    }

    #[tokio::test]
    async fn fake_ssh_runs_expected_worker_command() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("ssh-fake");
        fs::write(&script, "#!/bin/sh\nprintf '%s\\n' '{\"role\":\"worker\",\"protocol\":3,\"build\":\"fixture\"}'\n").unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let client = RemoteClient::new(ProcessRunner::default(), remote())
            .with_ssh_program(script.as_os_str());
        let info = client.hello(&CancellationToken::new()).await.unwrap();
        assert_eq!(info.build, "fixture");
        assert!(Path::new(&script).exists());
    }

    #[tokio::test]
    async fn require_worker_rejects_controller_role() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("ssh-fake");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' '{\"role\":\"controller\",\"protocol\":2,\"build\":\"fixture\"}'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let client = RemoteClient::new(ProcessRunner::default(), remote())
            .with_ssh_program(script.as_os_str());
        assert!(matches!(
            client.require_worker(&CancellationToken::new()).await,
            Err(RemoteError::WrongRole { .. })
        ));
    }
}
