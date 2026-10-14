//! An allowlisted controller service. Requests are never retried: a lost reply
//! cannot prove that a tracker mutation did not happen.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::Semaphore,
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use wt_platform::{
    lock::FileLock,
    process::{CommandSpec, ProcessRunner},
};

use crate::context::AppContext;

#[derive(clap::Args, Debug)]
pub struct ControllerArgs {
    #[command(subcommand)]
    pub command: ControllerCommand,
}

#[derive(clap::Subcommand, Debug)]
pub enum ControllerCommand {
    Serve,
    Status,
    Exec { service: String },
}

const INPUT_LIMIT: usize = 2 * 1024 * 1024;
const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
// JSON byte arrays need up to four bytes per raw byte, plus framing fields.
const WIRE_LIMIT: usize = 4 * (2 * OUTPUT_LIMIT) + 4096;
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(120);
const IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    service: String,
    input: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Reply {
    fn rejected(message: &str) -> Self {
        Self {
            exit_code: 75,
            stdout: vec![],
            stderr: format!("wt controller: not executed: {message}\n").into_bytes(),
        }
    }
    fn uncertain(message: &str) -> Self {
        Self { exit_code: 74, stdout: vec![], stderr: format!("wt controller: outcome uncertain: {message}; inspect service state before retrying\n").into_bytes() }
    }
}

pub fn socket_path(home: &Path, repo_id: &str) -> PathBuf {
    let digest = format!("{:x}", Sha256::digest(repo_id.as_bytes()));
    home.join(".local/state/wt/controller")
        .join(&digest[..16])
        .join("rpc.sock")
}

async fn resolve_socket(
    explicit: Option<std::ffi::OsString>,
    managed_root: Option<std::ffi::OsString>,
) -> Result<PathBuf> {
    if let Some(socket) = explicit {
        return Ok(PathBuf::from(socket));
    }
    let root = managed_root.context(
        "controller request not sent: WT_CONTROLLER_SOCKET and WT_MANAGED_ROOT are not configured",
    )?;
    let mut bytes = Vec::new();
    tokio::fs::File::open(Path::new(&root).join("controller-endpoint"))
        .await
        .context("controller request not sent: managed controller endpoint is unavailable")?
        .take(102)
        .read_to_end(&mut bytes)
        .await
        .context("controller request not sent: reading managed controller endpoint")?;
    endpoint_path(&bytes)
}

fn endpoint_path(bytes: &[u8]) -> Result<PathBuf> {
    let value =
        std::str::from_utf8(bytes).context("controller request not sent: endpoint is not UTF-8")?;
    if value.len() > 101
        || !value.starts_with('/')
        || value.contains(':')
        || value.chars().any(char::is_control)
    {
        bail!("controller request not sent: invalid managed controller endpoint");
    }
    Ok(PathBuf::from(value))
}

/// This entry point deliberately needs no repository configuration on the caller.
pub async fn exec(service: &str) -> Result<i32> {
    let socket = resolve_socket(
        std::env::var_os("WT_CONTROLLER_SOCKET"),
        std::env::var_os("WT_MANAGED_ROOT"),
    )
    .await?;
    let input = tokio::task::spawn_blocking(|| -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        let mut input = Vec::new();
        std::io::stdin()
            .take((INPUT_LIMIT + 1) as u64)
            .read_to_end(&mut input)?;
        Ok(input)
    })
    .await?
    .context("controller request not sent: reading stdin")?;
    if input.len() > INPUT_LIMIT {
        bail!("controller request not sent: input exceeds 2 MiB");
    }
    // Services consume JSON, but the original bytes (including whitespace) survive.
    serde_json::from_slice::<serde_json::Value>(&input)
        .context("controller request not sent: stdin must be JSON")?;
    let reply = call(
        Path::new(&socket),
        Request {
            service: service.into(),
            input,
        },
    )
    .await?;
    tokio::task::spawn_blocking(move || -> std::io::Result<i32> {
        use std::io::Write;
        std::io::stdout().write_all(&reply.stdout)?;
        std::io::stderr().write_all(&reply.stderr)?;
        Ok(reply.exit_code)
    })
    .await?
    .map_err(Into::into)
}

fn configuration_fingerprint(config: &wt_config::Config, build: &str, target: &str) -> String {
    // Config is fully resolved and ordered. Its Debug representation may contain
    // credentials: only feed it into the digest, never log or return it.
    let encoded = format!("{build}\0{target}\0{config:?}");
    format!("{:x}", Sha256::digest(encoded.as_bytes()))
}

fn context_fingerprint(context: &AppContext) -> String {
    let base = configuration_fingerprint(&context.config, env!("WT_BUILD_ID"), env!("WT_TARGET"));
    let home = crate::codex_reauth::controller_home(&context.home);
    format!(
        "{:x}",
        Sha256::digest(format!("{base}\0{}", home.display()).as_bytes())
    )
}

/// Health checks only the local controller and its loaded configuration.
/// Remote tunnel health is reported separately by the supervisor.
pub async fn status(context: &AppContext) -> Result<i32> {
    let reply = call(
        &socket_path(&context.home, &context.config.repo_id),
        Request {
            service: "__wt_status".into(),
            input: b"{}".to_vec(),
        },
    )
    .await?;
    let expected = context_fingerprint(context);
    if reply.exit_code != 0 {
        bail!(
            "controller health probe failed: {}",
            String::from_utf8_lossy(&reply.stderr)
        );
    }
    if reply.stdout != expected.as_bytes() {
        bail!("controller build or configuration is stale; restart the controller daemon");
    }
    println!("Controller ready (local socket; configuration matches)");
    Ok(0)
}

async fn call(socket: &Path, request: Request) -> Result<Reply> {
    let bytes = serde_json::to_vec(&request).context("controller request not sent: encoding")?;
    let mut stream = tokio::time::timeout(IO_TIMEOUT, UnixStream::connect(socket))
        .await
        .context("controller request not sent: connection timed out")?
        .context("controller request not sent: controller unavailable")?;
    // Admission precedes request bytes, so overload is unambiguously not sent.
    let admission = tokio::time::timeout(IO_TIMEOUT, read_frame(&mut stream))
        .await
        .context("controller request not sent: admission timed out")?
        .context("controller request not sent: admission unavailable")?;
    let admission: Reply = serde_json::from_slice(&admission)
        .context("controller request not sent: invalid admission")?;
    if admission.exit_code != 0 {
        return Ok(admission);
    }
    // From the first write attempt onward every transport error is ambiguous.
    tokio::time::timeout(IO_TIMEOUT, write_frame(&mut stream, &bytes))
        .await
        .context(
            "controller outcome uncertain: request write timed out; do not automatically retry",
        )?
        .context(
            "controller outcome uncertain: request write failed; do not automatically retry",
        )?;
    let bytes = tokio::time::timeout(
        EXECUTION_TIMEOUT + Duration::from_secs(15),
        read_frame(&mut stream),
    )
    .await
    .context(
        "controller outcome uncertain: reply timed out; inspect service state before retrying",
    )?
    .context("controller outcome uncertain: reply lost; inspect service state before retrying")?;
    let reply: Reply = serde_json::from_slice(&bytes)
        .context("controller outcome uncertain: malformed reply; do not automatically retry")?;
    if reply.stdout.len() > OUTPUT_LIMIT || reply.stderr.len() > OUTPUT_LIMIT {
        bail!("controller outcome uncertain: oversized reply; do not automatically retry");
    }
    Ok(reply)
}

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let length = reader.read_u32().await? as usize;
    if length > WIRE_LIMIT {
        bail!("controller frame exceeds bounded wire limit");
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> Result<()> {
    if bytes.len() > WIRE_LIMIT {
        bail!("controller frame exceeds bounded wire limit");
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn serve(context: &AppContext) -> Result<i32> {
    use std::os::unix::fs::PermissionsExt;
    let socket = socket_path(&context.home, &context.config.repo_id);
    let parent = socket.parent().context("controller socket has no parent")?;
    tokio::fs::create_dir_all(parent).await?;
    tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
    let _lock = FileLock::try_acquire(parent, "server", "controller services")
        .await?
        .context("controller service is already running")?;
    match tokio::fs::symlink_metadata(&socket).await {
        Ok(metadata) => {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() {
                bail!("refusing to replace non-socket controller path");
            }
            tokio::fs::remove_file(&socket).await?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    let _socket_guard = SocketGuard(socket.clone());
    tokio::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).await?;
    let tunnel_cancel = CancellationToken::new();
    let tunnels = tokio::spawn(crate::controller_tunnels::run(
        context.clone(),
        socket,
        tunnel_cancel.clone(),
    ));
    let result = serve_listener(
        listener,
        context.config.managed.services.clone(),
        context.config.paths.main_clone.clone(),
        context_fingerprint(context),
        context.cancellation.clone(),
    )
    .await;
    tunnel_cancel.cancel();
    tunnels
        .await
        .context("controller tunnel supervisor failed")?;
    result?;
    Ok(0)
}

async fn serve_listener(
    listener: UnixListener,
    services: BTreeMap<String, Vec<String>>,
    cwd: PathBuf,
    fingerprint: String,
    cancellation: CancellationToken,
) -> Result<()> {
    let services = Arc::new(services);
    let permits = Arc::new(Semaphore::new(4));
    let runner = ProcessRunner::default();
    let mut requests = JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => break Ok(()),
            Some(result) = requests.join_next(), if !requests.is_empty() => {
                if let Err(error) = result { tracing::error!(%error, "controller request task failed"); }
            }
            accepted = listener.accept() => {
                let (mut stream, _) = match accepted { Ok(value) => value, Err(error) => break Err(error.into()) };
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    // Bounded write on admission itself prevents unbounded rejection tasks.
                    let bytes = serde_json::to_vec(&Reply::rejected("all four service slots are busy"))?;
                    let _ = tokio::time::timeout(Duration::from_millis(100), write_frame(&mut stream, &bytes)).await;
                    continue;
                };
                let services = services.clone();
                let cwd = cwd.clone();
                let fingerprint = fingerprint.clone();
                let runner = runner.clone();
                requests.spawn(async move {
                    let _permit = permit;
                    let admission = serde_json::to_vec(&Reply { exit_code: 0, stdout: vec![], stderr: vec![] }).expect("serializing byte arrays");
                    if !matches!(tokio::time::timeout(IO_TIMEOUT, write_frame(&mut stream, &admission)).await, Ok(Ok(()))) { return; }
                    let reply = match tokio::time::timeout(IO_TIMEOUT, read_frame(&mut stream)).await {
                        Ok(Ok(bytes)) => match serde_json::from_slice::<Request>(&bytes) {
                            Ok(request) => execute_request(&runner, &services, &cwd, &fingerprint, request).await,
                            Err(_) => Reply::rejected("malformed request"),
                        },
                        _ => Reply::rejected("incomplete or oversized request"),
                    };
                    if let Ok(bytes) = serde_json::to_vec(&reply) {
                        let _ = tokio::time::timeout(IO_TIMEOUT, write_frame(&mut stream, &bytes)).await;
                    }
                });
            }
        }
    };
    drop(listener);
    // Admission stops immediately; accepted mutations retain their own bounded
    // process deadline and are reaped before the daemon exits.
    while let Some(result) = requests.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "controller request failed during drain");
        }
    }
    result
}

async fn execute_request(
    runner: &ProcessRunner,
    services: &BTreeMap<String, Vec<String>>,
    cwd: &Path,
    fingerprint: &str,
    request: Request,
) -> Reply {
    if request.input.len() > INPUT_LIMIT {
        return Reply::rejected("input exceeds 2 MiB");
    }
    if serde_json::from_slice::<serde_json::Value>(&request.input).is_err() {
        return Reply::rejected("input must be JSON");
    }
    if request.service == "__wt_status" {
        return Reply {
            exit_code: 0,
            stdout: fingerprint.as_bytes().to_vec(),
            stderr: vec![],
        };
    }
    let Some(argv) = services.get(&request.service) else {
        return Reply::rejected("service is not configured");
    };
    let Some(program) = argv.first().filter(|s| !s.is_empty()) else {
        return Reply::rejected("service has no executable");
    };
    let mut spec = CommandSpec::new(program).args(&argv[1..]).cwd(cwd);
    spec.input = Some(request.input);
    spec.timeout = EXECUTION_TIMEOUT;
    spec.output_limit = OUTPUT_LIMIT;
    spec.env = vec![
        ("WT_CONTROLLER_BRIDGE".into(), None),
        ("WT_CONTROLLER_SOCKET".into(), None),
        ("WT_MANAGED_ROOT".into(), None),
        ("WT_MANAGED_FILES".into(), None),
    ];
    match runner.run(spec, &CancellationToken::new()).await {
        Ok(output) => Reply {
            exit_code: output.status.code().unwrap_or(1),
            stdout: output.stdout,
            stderr: output.stderr,
        },
        Err(error) => Reply::uncertain(&error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn executes_only_allowlisted_argv_and_preserves_bytes_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let services = BTreeMap::from([(
            "echo".into(),
            vec![
                "sh".into(),
                "-c".into(),
                "cat; printf error >&2; exit 7".into(),
            ],
        )]);
        let input = b" {\"message\":\"hello\"}\n".to_vec();
        let reply = execute_request(
            &ProcessRunner::default(),
            &services,
            dir.path(),
            "fixture-fingerprint",
            Request {
                service: "echo".into(),
                input: input.clone(),
            },
        )
        .await;
        assert_eq!(reply.exit_code, 7);
        assert_eq!(reply.stdout, input);
        assert_eq!(reply.stderr, b"error");
        let reply = execute_request(
            &ProcessRunner::default(),
            &services,
            dir.path(),
            "fixture-fingerprint",
            Request {
                service: "sh".into(),
                input: b"{}".to_vec(),
            },
        )
        .await;
        assert_eq!(reply.exit_code, 75);
        assert!(String::from_utf8_lossy(&reply.stderr).contains("not executed"));
    }

    #[tokio::test]
    async fn cancellation_drains_accepted_mutation_without_cancelling_it() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("rpc");
        let listener = UnixListener::bind(&socket).unwrap();
        let services = BTreeMap::from([("write".into(), vec!["sh".into(), "-c".into(), "touch accepted; while [ ! -f release ]; do sleep 0.01; done; printf done > completed; cat".into()])]);
        let cancel = CancellationToken::new();
        let server = tokio::spawn(serve_listener(
            listener,
            services,
            dir.path().into(),
            "fixture-fingerprint".into(),
            cancel.clone(),
        ));
        let client = tokio::spawn(async move {
            call(
                &socket,
                Request {
                    service: "write".into(),
                    input: b"{}".to_vec(),
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.path().join("accepted").exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        assert!(!server.is_finished());
        tokio::fs::write(dir.path().join("release"), b"")
            .await
            .unwrap();
        assert_eq!(client.await.unwrap().unwrap().exit_code, 0);
        server.await.unwrap().unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("completed")).unwrap(),
            b"done"
        );
    }

    #[tokio::test]
    async fn oversized_frames_are_rejected_before_allocation() {
        let (mut writer, mut reader) = tokio::io::duplex(4);
        writer.write_u32((WIRE_LIMIT + 1) as u32).await.unwrap();
        assert!(
            read_frame(&mut reader)
                .await
                .unwrap_err()
                .to_string()
                .contains("limit")
        );
    }

    #[tokio::test]
    async fn connection_failure_is_explicitly_not_sent() {
        let dir = tempfile::tempdir().unwrap();
        let error = call(
            &dir.path().join("missing"),
            Request {
                service: "a".into(),
                input: b"{}".to_vec(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("not sent"));
    }

    #[tokio::test]
    async fn overload_is_rejected_before_request_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("rpc");
        let listener = UnixListener::bind(&socket).unwrap();
        let cancel = CancellationToken::new();
        let server = tokio::spawn(serve_listener(
            listener,
            BTreeMap::new(),
            dir.path().into(),
            "fixture-fingerprint".into(),
            cancel.clone(),
        ));
        let mut held = Vec::new();
        for _ in 0..4 {
            let mut stream = UnixStream::connect(&socket).await.unwrap();
            let admission: Reply =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            assert_eq!(admission.exit_code, 0);
            held.push(stream);
        }
        let reply = call(
            &socket,
            Request {
                service: "never-run".into(),
                input: vec![b' '; INPUT_LIMIT],
            },
        )
        .await
        .unwrap();
        assert_eq!(reply.exit_code, 75);
        assert!(String::from_utf8_lossy(&reply.stderr).contains("not executed"));
        drop(held);
        cancel.cancel();
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn lost_reply_is_uncertain_and_never_retried() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("rpc");
        let listener = UnixListener::bind(&socket).unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            write_frame(
                &mut stream,
                &serde_json::to_vec(&Reply {
                    exit_code: 0,
                    stdout: vec![],
                    stderr: vec![],
                })
                .unwrap(),
            )
            .await
            .unwrap();
            let request: Request =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            assert_eq!(request.service, "mutate");
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let error = call(
            &socket,
            Request {
                service: "mutate".into(),
                input: b"{}".to_vec(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("outcome uncertain"));
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn health_probe_fingerprints_loaded_configuration_without_running_service() {
        let dir = tempfile::tempdir().unwrap();
        let services = BTreeMap::from([("tracker".into(), vec!["/does/not/exist".into()])]);
        let reply = execute_request(
            &ProcessRunner::default(),
            &services,
            dir.path(),
            "fixture-fingerprint",
            Request {
                service: "__wt_status".into(),
                input: b"{}".to_vec(),
            },
        )
        .await;
        assert_eq!(reply.exit_code, 0);
        assert_eq!(reply.stdout, b"fixture-fingerprint");
    }

    #[test]
    fn health_identity_changes_for_build_target_and_remote_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let raw = toml::from_str("[paths]\nmain_clone='/repo'\nworktree_root='/worktrees'\n[branch]\nprefix='test'\n[[remotes]]\nhost='worker'\nmanaged=true\n").unwrap();
        let options = wt_config::LoadOptions::new(dir.path(), dir.path(), BTreeMap::new());
        let config = wt_config::Config::from_raw(raw, &options).unwrap();
        let original = configuration_fingerprint(&config, "build-a", "target-a");
        assert_eq!(
            original,
            configuration_fingerprint(&config, "build-a", "target-a")
        );
        assert_ne!(
            original,
            configuration_fingerprint(&config, "build-b", "target-a")
        );
        assert_ne!(
            original,
            configuration_fingerprint(&config, "build-a", "target-b")
        );
        let mut changed = config.clone();
        changed.remotes[0].host = "other-worker".into();
        assert_ne!(
            original,
            configuration_fingerprint(&changed, "build-a", "target-a")
        );
        changed = config.clone();
        changed
            .managed
            .services
            .insert("tracker".into(), vec!["/new/service".into()]);
        assert_ne!(
            original,
            configuration_fingerprint(&changed, "build-a", "target-a")
        );
        changed = config;
        changed.paths.main_clone = "/other/clone".into();
        assert_ne!(
            original,
            configuration_fingerprint(&changed, "build-a", "target-a")
        );
    }

    #[tokio::test]
    async fn existing_sessions_resolve_provisioned_endpoint_without_new_environment() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("controller-endpoint"),
            b"/private/controller.sock",
        )
        .unwrap();
        let path = resolve_socket(None, Some(dir.path().as_os_str().to_owned()))
            .await
            .unwrap();
        assert_eq!(path, Path::new("/private/controller.sock"));
        let path = resolve_socket(
            Some("/explicit.sock".into()),
            Some("/does/not/exist".into()),
        )
        .await
        .unwrap();
        assert_eq!(path, Path::new("/explicit.sock"));
        for value in [
            b"relative.sock".as_slice(),
            b"/private/colon:socket",
            b"/private/newline\n",
            &[0xff],
        ] {
            assert!(endpoint_path(value).is_err());
        }
        std::fs::write(dir.path().join("controller-endpoint"), vec![b'/'; 102]).unwrap();
        assert!(
            resolve_socket(None, Some(dir.path().as_os_str().to_owned()))
                .await
                .is_err()
        );
    }

    #[test]
    fn socket_identity_is_stable_and_repository_scoped() {
        assert_eq!(
            socket_path(Path::new("/home/a"), "repo"),
            socket_path(Path::new("/home/a"), "repo")
        );
        assert_ne!(
            socket_path(Path::new("/home/a"), "repo"),
            socket_path(Path::new("/home/a"), "other")
        );
    }
}
