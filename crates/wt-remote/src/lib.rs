//! SSH transport and versioned worker protocol for native remote wt hosts.

use std::{ffi::OsString, path::PathBuf, time::Duration};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_config::RemoteConfig;
use wt_core::WorkStatusRecord;
use wt_platform::process::{CommandSpec, ProcessError, ProcessOutput, ProcessRunner};

pub const WORKER_PROTOCOL_VERSION: u32 = 3;
const SSH_TIMEOUT: Duration = Duration::from_secs(15);
const PLATFORM_TIMEOUT: Duration = Duration::from_secs(15);
const PROVISION_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_CANDIDATE_BYTES: u64 = 192 * 1024 * 1024;
const RUNTIME_CACHE: &str = ".cache/wt/native-runtimes";

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

/// A native worker executable with its release identity and content digest.
/// The target and build are checked again by the worker before publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryCandidate {
    pub local_path: PathBuf,
    pub target: String,
    pub sha256: String,
    pub build_id: String,
}

impl BinaryCandidate {
    pub async fn from_path(
        local_path: impl Into<PathBuf>,
        target: impl Into<String>,
        build_id: impl Into<String>,
    ) -> Result<Self, RemoteError> {
        let local_path = local_path.into();
        let target = target.into();
        let build_id = build_id.into();
        validate_target(&target)?;
        validate_build_id(&build_id)?;
        let metadata = tokio::fs::metadata(&local_path).await.map_err(|error| {
            RemoteError::Candidate(format!("read {}: {error}", local_path.display()))
        })?;
        if !metadata.is_file() {
            return Err(RemoteError::Candidate(format!(
                "{} is not a regular file",
                local_path.display()
            )));
        }
        if metadata.len() > MAX_CANDIDATE_BYTES {
            return Err(RemoteError::Candidate(format!(
                "{} is larger than the {} MiB native-binary limit",
                local_path.display(),
                MAX_CANDIDATE_BYTES / (1024 * 1024)
            )));
        }
        let bytes = tokio::fs::read(&local_path).await.map_err(|error| {
            RemoteError::Candidate(format!("read {}: {error}", local_path.display()))
        })?;
        if bytes.is_empty() {
            return Err(RemoteError::Candidate(format!(
                "{} is empty",
                local_path.display()
            )));
        }
        let sha256 = hex_sha256(&bytes);
        Ok(Self {
            local_path,
            target,
            sha256,
            build_id,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotePlatform {
    pub os: String,
    pub architecture: String,
    pub target: String,
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
    #[error(
        "remote worker {host} runs build {actual}; expected the verified native build {expected}"
    )]
    BuildMismatch {
        host: String,
        actual: String,
        expected: String,
    },
    #[error("remote worker command failed: {detail}")]
    Command { detail: String },
    #[error(
        "native remote worker platform is unsupported: {os} {architecture}; supported targets are aarch64-apple-darwin, x86_64-apple-darwin, aarch64-unknown-linux-gnu, and x86_64-unknown-linux-gnu"
    )]
    UnsupportedPlatform { os: String, architecture: String },
    #[error("invalid native worker binary candidate: {0}")]
    Candidate(String),
    #[error("native worker runtime provisioning failed: {0}")]
    Provision(String),
}

#[derive(Clone)]
pub struct RemoteClient {
    runner: ProcessRunner,
    remote: RemoteConfig,
    ssh_program: OsString,
    runtime_path: Option<String>,
    expected_build_id: Option<String>,
}

impl RemoteClient {
    pub fn new(runner: ProcessRunner, remote: RemoteConfig) -> Self {
        Self {
            runner,
            remote,
            ssh_program: OsString::from("ssh"),
            runtime_path: None,
            expected_build_id: None,
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

    /// Selects an immutable worker runtime for this client instance only.
    /// The configured stable launcher is never changed.
    pub fn with_runtime_path(mut self, path: impl Into<String>) -> Self {
        self.runtime_path = Some(path.into());
        self
    }

    fn with_expected_build_id(mut self, build_id: String) -> Self {
        self.expected_build_id = Some(build_id);
        self
    }

    pub fn selected_runtime_path(&self) -> &str {
        self.runtime_path.as_deref().unwrap_or(&self.remote.wt_path)
    }

    /// Determine the worker's target without invoking the configured wt
    /// executable, which may be absent or built for another platform.
    pub async fn probe_platform(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<RemotePlatform, RemoteError> {
        let output = self
            .run_remote_shell(
                "printf 'WT_REMOTE_PLATFORM\\n'; uname -s; uname -m",
                None,
                PLATFORM_TIMEOUT,
                cancellation,
            )
            .await?;
        let output = check_success(output, "remote platform probe")?;
        parse_remote_platform(&output.stdout_text())
    }

    /// Transfer, validate, and atomically publish an immutable native worker
    /// executable, then return a client bound to that exact runtime.
    pub async fn prepare_runtime(
        &self,
        candidate: &BinaryCandidate,
        cancellation: &CancellationToken,
    ) -> Result<Self, RemoteError> {
        validate_target(&candidate.target)?;
        validate_build_id(&candidate.build_id)?;
        if !is_sha256(&candidate.sha256) {
            return Err(RemoteError::Candidate(
                "sha256 must be 64 lowercase hexadecimal characters".into(),
            ));
        }
        let platform = self.probe_platform(cancellation).await?;
        if platform.target != candidate.target {
            return Err(RemoteError::Candidate(format!(
                "candidate target {} does not match worker target {}; use a verified release artifact for the worker platform",
                candidate.target, platform.target
            )));
        }
        let runtime_path = runtime_path(&candidate.target, &candidate.sha256);
        let cached = self
            .run_remote_shell(
                &cache_probe_script(&candidate.target, &candidate.sha256, &candidate.build_id),
                None,
                PROVISION_TIMEOUT,
                cancellation,
            )
            .await?;
        let cached = check_success(cached, "check cached native worker runtime")?;
        match cached.stdout_text().trim() {
            "WT_RUNTIME_REUSED" => {
                return Ok(self
                    .clone()
                    .with_runtime_path(runtime_path)
                    .with_expected_build_id(candidate.build_id.clone()));
            }
            "WT_RUNTIME_NEEDS_UPLOAD" => {}
            output => {
                return Err(RemoteError::Protocol(format!(
                    "remote runtime cache check returned unexpected output: {output:?}"
                )));
            }
        }
        let bytes = tokio::fs::read(&candidate.local_path)
            .await
            .map_err(|error| {
                RemoteError::Candidate(format!("read {}: {error}", candidate.local_path.display()))
            })?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_CANDIDATE_BYTES {
            return Err(RemoteError::Candidate(format!(
                "{} has an invalid size",
                candidate.local_path.display()
            )));
        }
        let actual_sha = hex_sha256(&bytes);
        if actual_sha != candidate.sha256 {
            return Err(RemoteError::Candidate(format!(
                "{} changed after candidate verification",
                candidate.local_path.display()
            )));
        }
        let nonce = random_nonce()?;
        let directory = runtime_directory(&candidate.target, &candidate.sha256);
        let upload = format!(
            "set -eu; umask 077; dir=\"$HOME/{directory}\"; mkdir -p \"$dir\"; stage=\"$dir/.wt-stage-{nonce}\"; cleanup() {{ rm -f \"$stage\"; }}; trap cleanup EXIT HUP INT TERM; cat > \"$stage\"; actual_size=$(wc -c < \"$stage\" | tr -d '[:space:]'); [ \"$actual_size\" = \"{size}\" ] || {{ echo 'uploaded runtime size is incomplete' >&2; exit 1; }}; trap - EXIT HUP INT TERM",
            size = bytes.len(),
        );
        let output = self
            .run_remote_shell(&upload, Some(bytes), PROVISION_TIMEOUT, cancellation)
            .await?;
        check_success(output, "upload native worker runtime")?;

        let publish = publish_script(
            &candidate.target,
            &candidate.sha256,
            &candidate.build_id,
            &nonce,
        );
        let output = self
            .run_remote_shell(&publish, None, PROVISION_TIMEOUT, cancellation)
            .await?;
        check_success(output, "validate and publish native worker runtime")?;

        let prepared = self
            .clone()
            .with_runtime_path(runtime_path)
            .with_expected_build_id(candidate.build_id.clone());
        Ok(prepared)
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
        if let Some(expected) = &self.expected_build_id
            && info.build != *expected
        {
            return Err(RemoteError::BuildMismatch {
                host: self.remote.label.clone(),
                actual: info.build,
                expected: expected.clone(),
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
        let command = remote_wt_command_at(&self.remote, self.selected_runtime_path(), args);
        let mut spec = CommandSpec::new(self.ssh_program.clone())
            .args(ssh_command_args(&self.remote, &command));
        spec.timeout = SSH_TIMEOUT;
        Ok(self.runner.run(spec, cancellation).await?)
    }

    pub fn interactive_tui(&self) -> PreparedRemoteSession {
        PreparedRemoteSession {
            program: self.ssh_program.clone(),
            args: interactive_ssh_args_at(&self.remote, self.selected_runtime_path(), None),
        }
    }

    /// One long-lived, non-PTY channel carries the same service used by the
    /// local controller. SSH keepalives bound detection of a lost connection.
    pub fn host_stream(&self) -> PreparedRemoteSession {
        let command = remote_wt_command_at(
            &self.remote,
            self.selected_runtime_path(),
            &["_host".into()],
        );
        PreparedRemoteSession {
            program: self.ssh_program.clone(),
            args: ssh_command_args(&self.remote, &command),
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
            args: interactive_ssh_args_at(&self.remote, self.selected_runtime_path(), Some(&argv)),
        }
    }

    pub fn interactive_selected_session(&self, selection: &str) -> PreparedRemoteSession {
        let argv = vec!["_session".to_owned(), selection.to_owned()];
        PreparedRemoteSession {
            program: self.ssh_program.clone(),
            args: interactive_ssh_args_at(&self.remote, self.selected_runtime_path(), Some(&argv)),
        }
    }

    async fn run_remote_shell(
        &self,
        command: &str,
        input: Option<Vec<u8>>,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, RemoteError> {
        let command = format!("{}{command}", remote_environment(&self.remote));
        let mut spec = CommandSpec::new(self.ssh_program.clone())
            .args(ssh_command_args(&self.remote, &command));
        spec.timeout = timeout;
        spec.input = input;
        Ok(self.runner.run(spec, cancellation).await?)
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
    remote_wt_command_at(remote, &remote.wt_path, argv)
}

fn remote_wt_command_at(remote: &RemoteConfig, path: &str, argv: &[String]) -> String {
    format!(
        "{}exec {} _remote {}",
        remote_environment(remote),
        remote_executable(path),
        encode_remote_args(argv)
    )
}

fn remote_environment(remote: &RemoteConfig) -> String {
    remote
        .config
        .as_ref()
        .map(|path| {
            format!(
                "export WT_CONFIG={}; export WT_REPO_CONFIG=\"$WT_CONFIG\"; ",
                remote_executable(path),
            )
        })
        .unwrap_or_default()
}

pub fn ssh_args(remote: &RemoteConfig, argv: &[String]) -> Vec<OsString> {
    let command = remote_wt_command(remote, argv);
    ssh_command_args(remote, &command)
}

fn ssh_command_args(remote: &RemoteConfig, command: &str) -> Vec<OsString> {
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
    .chain([remote.host.as_str(), command])
    .map(OsString::from)
    .collect()
}

pub fn interactive_ssh_args(remote: &RemoteConfig, argv: Option<&[String]>) -> Vec<OsString> {
    interactive_ssh_args_at(remote, &remote.wt_path, argv)
}

fn interactive_ssh_args_at(
    remote: &RemoteConfig,
    runtime_path: &str,
    argv: Option<&[String]>,
) -> Vec<OsString> {
    let command = match argv {
        Some(argv) => remote_wt_command_at(remote, runtime_path, argv),
        None => format!(
            "{}exec {}",
            remote_environment(remote),
            remote_executable(runtime_path)
        ),
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

pub fn parse_remote_platform(raw: &str) -> Result<RemotePlatform, RemoteError> {
    let lines = raw.lines().map(str::trim).collect::<Vec<_>>();
    let marker = lines
        .iter()
        .rposition(|line| *line == "WT_REMOTE_PLATFORM")
        .ok_or_else(|| RemoteError::Protocol("remote platform probe marker is missing".into()))?;
    let values = lines[marker + 1..]
        .iter()
        .copied()
        .filter(|line| !line.is_empty())
        .take(2)
        .collect::<Vec<_>>();
    if values.len() != 2 {
        return Err(RemoteError::Protocol(
            "remote platform probe did not return an OS and architecture".into(),
        ));
    }
    let os = values[0].to_owned();
    let architecture = values[1].to_owned();
    let target = match (os.as_str(), architecture.as_str()) {
        ("Darwin", "arm64" | "aarch64") => "aarch64-apple-darwin",
        ("Darwin", "x86_64" | "amd64") => "x86_64-apple-darwin",
        ("Linux", "aarch64" | "arm64") => "aarch64-unknown-linux-gnu",
        ("Linux", "x86_64" | "amd64") => "x86_64-unknown-linux-gnu",
        _ => {
            return Err(RemoteError::UnsupportedPlatform { os, architecture });
        }
    };
    Ok(RemotePlatform {
        os,
        architecture,
        target: target.into(),
    })
}

fn validate_target(target: &str) -> Result<(), RemoteError> {
    if matches!(
        target,
        "aarch64-apple-darwin"
            | "x86_64-apple-darwin"
            | "aarch64-unknown-linux-gnu"
            | "x86_64-unknown-linux-gnu"
    ) {
        Ok(())
    } else {
        Err(RemoteError::Candidate(format!(
            "unsupported release target `{target}`"
        )))
    }
}

fn validate_build_id(build_id: &str) -> Result<(), RemoteError> {
    if !build_id.is_empty()
        && build_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        Ok(())
    } else {
        Err(RemoteError::Candidate(
            "build id contains characters unsafe for a native release identity".into(),
        ))
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut result = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn random_nonce() -> Result<String, RemoteError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| RemoteError::Provision(format!("generate staging id: {error}")))?;
    let mut nonce = String::with_capacity(32);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(nonce, "{byte:02x}");
    }
    Ok(nonce)
}

fn runtime_directory(target: &str, sha256: &str) -> String {
    format!("{RUNTIME_CACHE}/{target}/{sha256}")
}

fn runtime_path(target: &str, sha256: &str) -> String {
    format!("~/{}/{target}/{sha256}/wt", RUNTIME_CACHE)
}

fn runtime_validation_functions() -> &'static str {
    r#"hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    return 1
  fi
}
validate() {
  file="$1"
  [ -f "$file" ] && [ ! -L "$file" ] && [ -x "$file" ] || return 1
  actual_hash=$(hash_file "$file") || return 1
  [ "$actual_hash" = "$expected_hash" ] || return 1
  probe=$("$file" --_boot-probe 2>&1) || return 1
  [ "$probe" = "$expected_probe" ] || return 1
  hello=$("$file" _hello 2>&1) || return 1
  [ "$hello" = "$expected_hello" ] || return 1
}"#
}

fn cache_probe_script(target: &str, sha256: &str, build_id: &str) -> String {
    let directory = runtime_directory(target, sha256);
    format!(
        r#"set -eu
dir="$HOME/{directory}"
final="$dir/wt"
expected_hash='{sha256}'
expected_probe='wt-build-id:{build_id}:{target}'
expected_hello='{{"role":"worker","protocol":{protocol},"build":"{build_id}"}}'
{validation_functions}
if validate "$final"; then
  echo WT_RUNTIME_REUSED
else
  echo WT_RUNTIME_NEEDS_UPLOAD
fi
"#,
        protocol = WORKER_PROTOCOL_VERSION,
        validation_functions = runtime_validation_functions(),
    )
}

fn publish_script(target: &str, sha256: &str, build_id: &str, nonce: &str) -> String {
    let directory = runtime_directory(target, sha256);
    format!(
        r#"set -eu
dir="$HOME/{directory}"
stage="$dir/.wt-stage-{nonce}"
final="$dir/wt"
expected_hash='{sha256}'
expected_probe='wt-build-id:{build_id}:{target}'
expected_hello='{{"role":"worker","protocol":{protocol},"build":"{build_id}"}}'
cleanup() {{ rm -f "$stage"; }}
trap cleanup EXIT HUP INT TERM
{validation_functions}
[ -f "$stage" ] || {{ echo 'uploaded runtime staging file is missing' >&2; exit 1; }}
chmod 700 "$stage"
if validate "$final"; then
  echo WT_RUNTIME_REUSED
  exit 0
fi
[ ! -d "$final" ] || {{ echo 'runtime cache path is a directory' >&2; exit 1; }}
mv -f "$stage" "$final"
validate "$final" || {{ echo 'published worker runtime failed verification' >&2; exit 1; }}
echo WT_RUNTIME_READY
"#,
        protocol = WORKER_PROTOCOL_VERSION,
        validation_functions = runtime_validation_functions(),
    )
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
            config: None,
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
    fn configured_worker_selectors_are_quoted_and_exported_for_batch_and_interactive_calls() {
        let mut remote = remote();
        remote.config = Some("~/odd path/wt's config.toml".into());

        let batch = remote_wt_command(&remote, &["_host".into()]);
        let expected = "export WT_CONFIG=\"$HOME\"/'odd path/wt'\\''s config.toml'; export WT_REPO_CONFIG=\"$WT_CONFIG\"; ";
        assert!(batch.starts_with(expected), "{batch}");
        assert!(batch.contains("exec \"$HOME\"/'.wt/bin/wt' _remote "));

        let interactive = interactive_ssh_args(&remote, None);
        let command = interactive.last().unwrap().to_string_lossy();
        assert!(command.starts_with(expected), "{command}");
        assert!(command.ends_with("exec \"$HOME\"/'.wt/bin/wt'"));
    }

    #[test]
    fn legacy_remote_calls_do_not_inject_worker_config_defaults() {
        let remote = remote();
        let command = remote_wt_command(&remote, &["status".into()]);
        assert!(!command.contains("WT_CONFIG"));
        assert!(!command.contains("WT_REPO_CONFIG"));
        assert!(command.starts_with("exec \"$HOME\"/'.wt/bin/wt' _remote "));
    }

    #[tokio::test]
    async fn configured_worker_selectors_reach_bootstrap_platform_probe() {
        let temp = tempfile::tempdir().unwrap();
        let capture = temp.path().join("ssh-command");
        let ssh = temp.path().join("ssh-fake");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nfor arg do command=\"$arg\"; done\nprintf '%s' \"$command\" > {capture}\nprintf '%s\\n' WT_REMOTE_PLATFORM Linux x86_64\n",
                capture = shell_quote(&capture.display().to_string()),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&ssh).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&ssh, permissions).unwrap();

        let mut configured = remote();
        configured.config = Some("/etc/wt/worker config.toml".into());
        let client = RemoteClient::new(ProcessRunner::default(), configured)
            .with_ssh_program(ssh.as_os_str());
        let platform = client
            .probe_platform(&CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(platform.target, "x86_64-unknown-linux-gnu");
        let command = fs::read_to_string(capture).unwrap();
        assert!(command.starts_with(
            "export WT_CONFIG='/etc/wt/worker config.toml'; export WT_REPO_CONFIG=\"$WT_CONFIG\"; "
        ));
        assert!(command.ends_with("printf 'WT_REMOTE_PLATFORM\\n'; uname -s; uname -m"));
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
    fn platform_probe_maps_only_supported_release_targets() {
        let platform =
            parse_remote_platform("login banner\nWT_REMOTE_PLATFORM\nLinux\naarch64\n").unwrap();
        assert_eq!(platform.target, "aarch64-unknown-linux-gnu");
        let platform = parse_remote_platform("WT_REMOTE_PLATFORM\nDarwin\narm64\n").unwrap();
        assert_eq!(platform.target, "aarch64-apple-darwin");
        assert!(matches!(
            parse_remote_platform("WT_REMOTE_PLATFORM\nFreeBSD\nx86_64\n"),
            Err(RemoteError::UnsupportedPlatform { .. })
        ));
        assert!(parse_remote_platform("Linux\nx86_64").is_err());
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
    async fn runtime_publish_is_content_addressed_verified_and_recovers_corrupt_cache() {
        let temp = tempfile::tempdir().unwrap();
        let worker_home = temp.path().join("remote home");
        fs::create_dir_all(&worker_home).unwrap();
        let target = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => "aarch64-apple-darwin",
            ("macos", "x86_64") => "x86_64-apple-darwin",
            ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
            ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
            other => panic!("test host target is unsupported: {other:?}"),
        };
        let build_id = "fixture-build-123";
        let candidate_path = temp.path().join("candidate wt");
        fs::write(
            &candidate_path,
            format!(
                concat!(
                    "#!/usr/bin/env python3\nimport base64, json, sys\n",
                    "build = {build_id:?}\n",
                    "if sys.argv[1:] == ['--_boot-probe']:\n",
                    " print('wt-build-id:' + build + ':' + {target:?})\n",
                    "elif sys.argv[1:] == ['_hello']:\n",
                    " print('{{\"role\":\"worker\",\"protocol\":3,\"build\":\"' + build + '\"}}')\n",
                    "elif len(sys.argv) == 3 and sys.argv[1] == '_remote':\n",
                    " args = json.loads(base64.urlsafe_b64decode(sys.argv[2] + '=' * (-len(sys.argv[2]) % 4)))\n",
                    " if args == ['_hello']:\n",
                    "  print('{{\"role\":\"worker\",\"protocol\":3,\"build\":\"' + build + '\"}}')\n",
                    " else:\n",
                    "  raise SystemExit('unexpected test argv')\n",
                    "else:\n",
                    " raise SystemExit('unexpected candidate argv')\n"
                ),
                build_id = build_id,
                target = target,
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&candidate_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&candidate_path, permissions).unwrap();
        let candidate = BinaryCandidate::from_path(&candidate_path, target, build_id)
            .await
            .unwrap();

        let ssh = temp.path().join("ssh-fake");
        fs::write(
            &ssh,
            format!(
                concat!(
                    "#!/bin/sh\ncommand=\"\"\nfor arg do command=\"$arg\"; done\n",
                    "case \"$command\" in\n",
                    "  *WT_REMOTE_PLATFORM*) printf '%s\\n' WT_REMOTE_PLATFORM {os:?} {arch:?} ;;\n",
                    "  *) HOME={home:?} exec /bin/sh -c \"$command\" ;;\n",
                    "esac\n",
                ),
                os = if target.contains("apple") {
                    "Darwin"
                } else {
                    "Linux"
                },
                arch = match target {
                    "aarch64-apple-darwin" | "aarch64-unknown-linux-gnu" => "aarch64",
                    _ => "x86_64",
                },
                home = worker_home.to_string_lossy(),
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&ssh).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&ssh, permissions).unwrap();

        let configured = RemoteConfig {
            host: "fixture".into(),
            label: "Fixture".into(),
            wt_path: "~/.local/bin/wt".into(),
            config: None,
        };
        let client = RemoteClient::new(ProcessRunner::default(), configured.clone())
            .with_ssh_program(ssh.as_os_str());
        let prepared = client
            .prepare_runtime(&candidate, &CancellationToken::new())
            .await
            .unwrap();
        let expected_path = runtime_path(target, &candidate.sha256);
        assert_eq!(prepared.selected_runtime_path(), expected_path);
        assert_eq!(prepared.remote().wt_path, configured.wt_path);
        let info = prepared
            .require_worker(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(info.build, build_id);
        let cached = worker_home
            .join(runtime_directory(target, &candidate.sha256))
            .join("wt");
        assert_eq!(
            fs::read(&cached).unwrap(),
            fs::read(&candidate_path).unwrap()
        );

        let candidate_bytes = fs::read(&candidate_path).unwrap();
        fs::remove_file(&candidate_path).unwrap();
        let reused = client
            .prepare_runtime(&candidate, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(reused.selected_runtime_path(), expected_path);
        fs::write(&candidate_path, &candidate_bytes).unwrap();
        let mut permissions = fs::metadata(&candidate_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&candidate_path, permissions).unwrap();

        fs::write(&cached, b"tampered cache entry").unwrap();
        let recovered = client
            .prepare_runtime(&candidate, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(recovered.selected_runtime_path(), expected_path);
        assert_eq!(
            fs::read(&cached).unwrap(),
            fs::read(&candidate_path).unwrap()
        );
        assert!(
            fs::read_dir(cached.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".wt-stage-"))
        );
    }

    #[tokio::test]
    async fn runtime_candidate_must_match_probed_worker_target_before_upload() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = temp.path().join("ssh-fake");
        fs::write(
            &ssh,
            "#!/bin/sh\nfor arg do command=\"$arg\"; done\ncase \"$command\" in *WT_REMOTE_PLATFORM*) printf '%s\\n' WT_REMOTE_PLATFORM Linux x86_64 ;; *) touch \"$HOME/should-not-write\" ;; esac\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&ssh).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&ssh, permissions).unwrap();
        let candidate_path = temp.path().join("candidate");
        fs::write(&candidate_path, b"candidate").unwrap();
        let candidate = BinaryCandidate::from_path(
            &candidate_path,
            "aarch64-unknown-linux-gnu",
            "fixture-build",
        )
        .await
        .unwrap();
        let client =
            RemoteClient::new(ProcessRunner::default(), remote()).with_ssh_program(ssh.as_os_str());
        let error = match client
            .prepare_runtime(&candidate, &CancellationToken::new())
            .await
        {
            Ok(_) => panic!("mismatched candidate unexpectedly prepared"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("does not match worker target"));
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
