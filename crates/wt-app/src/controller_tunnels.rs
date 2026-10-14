//! Independently supervised, private SSH reverse forwards for controller services.
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::context::AppContext;

/// OpenSSH's client StreamLocalBindUnlink setting does not apply to a remote
/// -R listener. Reclaim an abandoned worker endpoint before asking sshd to bind
/// it, without requiring a server-wide unlink policy or disturbing a live peer.
pub(crate) async fn prepare_worker_socket(
    root: &Path,
    socket: &Path,
    cancel: &CancellationToken,
) -> Result<()> {
    prepare_worker_socket_with_probe(root, socket, cancel, SystemTime::now(), |path| async move {
        tokio::net::UnixStream::connect(path).await.map(|_| ())
    })
    .await
}

#[derive(Debug, Eq, PartialEq)]
struct SocketWitness {
    device: u64,
    inode: u64,
    changed_seconds: i64,
    changed_nanos: i64,
}

async fn owned_socket(path: &Path) -> Result<Option<SocketWitness>> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect worker controller endpoint"),
    };
    if !metadata.file_type().is_socket() {
        bail!("refusing to replace non-socket worker controller endpoint");
    }
    // SAFETY: geteuid has no arguments or memory preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!("worker controller socket belongs to another user");
    }
    Ok(Some(SocketWitness {
        device: metadata.dev(),
        inode: metadata.ino(),
        changed_seconds: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    }))
}

async fn prepare_worker_socket_with_probe<F, P>(
    root: &Path,
    socket: &Path,
    cancel: &CancellationToken,
    now: SystemTime,
    probe: F,
) -> Result<()>
where
    F: FnOnce(PathBuf) -> P,
    P: std::future::Future<Output = std::io::Result<()>>,
{
    use std::os::unix::fs::MetadataExt;
    let filename = socket
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid worker controller socket name")?;
    let identity = filename
        .strip_prefix("controller-")
        .and_then(|name| name.strip_suffix(".sock"));
    if !root.is_absolute()
        || socket.parent() != Some(root)
        || !identity
            .is_some_and(|id| id.len() == 16 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        bail!("worker controller socket is outside its managed endpoint namespace");
    }
    let directory = tokio::fs::symlink_metadata(root)
        .await
        .context("inspect managed controller directory")?;
    // SAFETY: geteuid has no arguments or memory preconditions.
    if !directory.is_dir()
        || directory.uid() != unsafe { libc::geteuid() }
        || directory.mode() & 0o077 != 0
    {
        bail!("managed controller directory must be a private directory owned by this user");
    }
    let _lock = wt_platform::lock::FileLock::acquire(
        root,
        &format!("{filename}.prepare"),
        "prepare controller tunnel endpoint",
        cancel,
    )
    .await?;
    let Some(before) = owned_socket(socket).await? else {
        return Ok(());
    };
    let connected = tokio::select! {
        _ = cancel.cancelled() => bail!("controller endpoint preparation cancelled"),
        result = tokio::time::timeout(Duration::from_secs(2), probe(socket.to_path_buf())) => result.context("worker controller socket probe timed out; endpoint retained")?,
    };
    match connected {
        // Do not even attempt another -R bind: sshd may have an unlink policy
        // enabled globally, independently of our client's setting.
        Ok(()) => bail!("worker controller endpoint has a live listener; endpoint retained"),
        Err(error) if error.raw_os_error() == Some(libc::ECONNREFUSED) => (),
        Err(error) => {
            return Err(error)
                .context("worker controller socket ownership is uncertain; endpoint retained");
        }
    }
    let changed = u64::try_from(before.changed_seconds)
        .ok()
        .zip(u32::try_from(before.changed_nanos).ok())
        .and_then(|(seconds, nanos)| UNIX_EPOCH.checked_add(Duration::new(seconds, nanos)));
    if !changed
        .and_then(|changed| now.duration_since(changed).ok())
        .is_some_and(|age| age >= Duration::from_secs(5))
    {
        bail!(
            "worker controller socket was recently created; retaining it for the next tunnel attempt"
        );
    }
    if cancel.is_cancelled() {
        bail!("controller endpoint preparation cancelled");
    }
    if owned_socket(socket).await?.as_ref() != Some(&before) {
        bail!("worker controller endpoint changed during its probe; endpoint retained");
    }
    // Other wt preparations hold the same lock. With remote unlink disabled,
    // sshd cannot bind a replacement while this witnessed socket still exists.
    tokio::fs::remove_file(socket)
        .await
        .context("remove abandoned worker controller socket")?;
    tracing::info!(path = %socket.display(), "reclaimed abandoned controller tunnel endpoint");
    Ok(())
}

pub async fn identity(home: &std::path::Path, cancel: &CancellationToken) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = home.join(".local/state/wt/controller");
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await?;
    let _lock =
        wt_platform::lock::FileLock::acquire(&dir, "identity", "controller identity", cancel)
            .await?;
    let path = dir.join("identity");
    match tokio::fs::read_to_string(&path).await {
        Ok(value) => {
            if value.len() != 16 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("invalid controller identity; refusing to replace it");
            }
            return Ok(value);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("read controller identity"),
    }
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("controller identity randomness: {error}"))?;
    let value: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let temporary = dir.join("identity.new");
    tokio::fs::write(&temporary, &value).await?;
    tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(value)
}

pub async fn run(context: AppContext, socket: PathBuf, cancel: CancellationToken) {
    let mut tasks = tokio::task::JoinSet::new();
    for endpoint in context.config.remotes.iter().filter(|r| r.managed) {
        let endpoint = endpoint.clone();
        let mut context = context.clone();
        context.cancellation = cancel.clone();
        let socket = socket.clone();
        let cancel = cancel.clone();
        tasks.spawn(async move {
            while !cancel.is_cancelled() {
                if let Err(error) = forward(&context, &endpoint, &socket, &cancel).await {
                    tracing::warn!(host = %endpoint.label, %error, "controller service connection unavailable");
                }
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(15)) => {}
                }
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "controller tunnel task failed");
        }
    }
}

fn forward_spec(remote: &str, local: &std::path::Path) -> Result<String> {
    let local = local
        .to_str()
        .context("controller socket path is not UTF-8")?;
    // OpenSSH uses colon as a separator; neither side is shell-interpolated.
    for path in [remote, local] {
        if !path.starts_with('/')
            || path.len() > 100
            || path.contains(':')
            || path.chars().any(char::is_control)
        {
            bail!(
                "controller socket must be an absolute Unix path of at most 100 bytes without colon or control characters"
            );
        }
    }
    Ok(format!("{remote}:{local}"))
}

async fn forward(
    context: &AppContext,
    endpoint: &wt_config::RemoteConfig,
    socket: &std::path::Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let (client, _) = crate::remote::prepare_remote_client(context, endpoint).await?;
    let output = client
        .run_managed(
            &["bridge-prepare".into()],
            Vec::new(),
            Duration::from_secs(10),
            cancel,
        )
        .await?;
    if !output.status.success() {
        bail!(
            "worker could not prepare its private controller socket: {}",
            output.stderr_text().trim()
        );
    }
    let spec = forward_spec(output.stdout_text().trim(), socket)?;
    let mut child = Command::new("ssh")
        .args([
            "-NT",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "StreamLocalBindUnlink=no",
            "-o",
            "StreamLocalBindMask=0177",
            "-R",
            &spec,
            "--",
            &endpoint.host,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("start controller SSH tunnel")?;
    let result = async {
        let mut ready = false;
        for _ in 0..5 {
            if cancel.is_cancelled() { return Ok(()); }
            let probe = client.run_managed(&["bridge-probe".into()], b"{}".to_vec(), Duration::from_secs(3), cancel).await;
            if probe.is_ok_and(|output| output.status.success()) { ready = true; break; }
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
        if !ready { bail!("SSH forwarding did not pass the controller round-trip probe"); }
        tracing::info!(host = %endpoint.label, "controller service tunnel ready");
        let home = crate::codex_reauth::controller_home(&context.home);
        // Re-read and send once after provisioning: an account switch may have
        // arrived while that earlier request was in flight.
        let mut synced = None;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                status = child.wait() => bail!("controller SSH tunnel ended: {}", status?),
                _ = tick.tick() => {
                    let desired = crate::codex_reauth::controller_generation(&home)?;
                    if desired != synced && let Some(generation) = desired {
                        let output = client.run_managed(&["auth-sync".into(), generation.to_string()], Vec::new(), Duration::from_secs(10), cancel).await?;
                        if !output.status.success() { bail!("worker could not persist the pending Codex account switch"); }
                        synced = desired;
                        tracing::info!(host = %endpoint.label, "managed Codex account switch pending");
                    }
                }
            }
        }
    }.await;
    let _ = child.start_kill();
    let _ = child.wait().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn endpoint_fixture() -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::Builder::new()
            .prefix("wt-tunnel-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.path().join("controller-0123456789abcdef.sock");
        (root, socket)
    }
    fn aged_probe_time() -> SystemTime {
        SystemTime::now() + Duration::from_secs(10)
    }
    async fn connect_only(path: PathBuf) -> std::io::Result<()> {
        tokio::net::UnixStream::connect(path).await.map(|_| ())
    }
    #[tokio::test]
    async fn worker_preparation_reclaims_only_abandoned_socket_and_keeps_live_listener() {
        let (root, socket) = endpoint_fixture();
        let cancel = CancellationToken::new();
        prepare_worker_socket(root.path(), &socket, &cancel)
            .await
            .unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        assert!(
            prepare_worker_socket(root.path(), &socket, &cancel)
                .await
                .is_err()
        );
        assert!(socket.exists());
        drop(listener);
        // The bind-before-listen window is deliberately not reclaimable.
        assert!(
            prepare_worker_socket(root.path(), &socket, &cancel)
                .await
                .is_err()
        );
        assert!(socket.exists());
        prepare_worker_socket_with_probe(
            root.path(),
            &socket,
            &cancel,
            aged_probe_time(),
            connect_only,
        )
        .await
        .unwrap();
        assert!(!socket.exists());
        // The replacement can bind immediately without any sshd unlink policy.
        let replacement = tokio::net::UnixListener::bind(&socket).unwrap();
        assert!(tokio::net::UnixStream::connect(&socket).await.is_ok());
        drop(replacement);
    }
    #[tokio::test]
    async fn worker_preparation_refuses_non_socket_symlink_and_outside_namespace() {
        let (root, socket) = endpoint_fixture();
        let cancel = CancellationToken::new();
        std::fs::write(&socket, "retain this file").unwrap();
        assert!(
            prepare_worker_socket(root.path(), &socket, &cancel)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&socket).unwrap(),
            "retain this file"
        );
        std::fs::remove_file(&socket).unwrap();
        let target = root.path().join("target");
        std::fs::write(&target, "retain symlink target").unwrap();
        std::os::unix::fs::symlink(&target, &socket).unwrap();
        assert!(
            prepare_worker_socket(root.path(), &socket, &cancel)
                .await
                .is_err()
        );
        assert!(
            std::fs::symlink_metadata(&socket)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(target).unwrap(),
            "retain symlink target"
        );
        for path in [
            root.path().join("other.sock"),
            root.path().join("../controller-0123456789abcdef.sock"),
        ] {
            assert!(
                prepare_worker_socket(root.path(), &path, &cancel)
                    .await
                    .is_err()
            );
        }
    }
    #[tokio::test]
    async fn worker_preparation_retains_uncertain_or_replaced_socket() {
        let (root, socket) = endpoint_fixture();
        drop(tokio::net::UnixListener::bind(&socket).unwrap());
        let cancel = CancellationToken::new();
        for code in [libc::EACCES, libc::ENOENT, libc::EIO] {
            assert!(
                prepare_worker_socket_with_probe(
                    root.path(),
                    &socket,
                    &cancel,
                    aged_probe_time(),
                    |_| async move { Err(std::io::Error::from_raw_os_error(code)) }
                )
                .await
                .is_err()
            );
            assert!(socket.exists());
        }
        let replacement = std::sync::Arc::new(std::sync::Mutex::new(None));
        let retained = replacement.clone();
        assert!(
            prepare_worker_socket_with_probe(
                root.path(),
                &socket,
                &cancel,
                aged_probe_time(),
                |path| async move {
                    std::fs::remove_file(&path)?;
                    *retained.lock().unwrap() = Some(std::os::unix::net::UnixListener::bind(path)?);
                    Err(std::io::Error::from_raw_os_error(libc::ECONNREFUSED))
                }
            )
            .await
            .is_err()
        );
        assert!(tokio::net::UnixStream::connect(&socket).await.is_ok());
        drop(replacement);
    }
    #[tokio::test(start_paused = true)]
    async fn worker_preparation_timeout_retains_endpoint() {
        let (root, socket) = endpoint_fixture();
        drop(tokio::net::UnixListener::bind(&socket).unwrap());
        assert!(
            prepare_worker_socket_with_probe(
                root.path(),
                &socket,
                &CancellationToken::new(),
                aged_probe_time(),
                |_| std::future::pending()
            )
            .await
            .is_err()
        );
        assert!(socket.exists());
    }
    #[tokio::test]
    async fn concurrent_worker_preparations_do_not_race_the_replacement_listener() {
        let (root, socket) = endpoint_fixture();
        drop(tokio::net::UnixListener::bind(&socket).unwrap());
        let cancel = CancellationToken::new();
        let (first, second) = tokio::join!(
            prepare_worker_socket_with_probe(
                root.path(),
                &socket,
                &cancel,
                aged_probe_time(),
                connect_only
            ),
            prepare_worker_socket_with_probe(
                root.path(),
                &socket,
                &cancel,
                aged_probe_time(),
                connect_only
            )
        );
        first.unwrap();
        second.unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        assert!(
            prepare_worker_socket(root.path(), &socket, &cancel)
                .await
                .is_err()
        );
        assert!(tokio::net::UnixStream::connect(&socket).await.is_ok());
        drop(listener);
    }
    #[tokio::test]
    async fn controller_identity_is_stable_and_corruption_is_not_replaced() {
        let home = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let first = identity(home.path(), &cancel).await.unwrap();
        assert_eq!(identity(home.path(), &cancel).await.unwrap(), first);
        let path = home.path().join(".local/state/wt/controller/identity");
        std::fs::write(&path, "corrupt").unwrap();
        assert!(identity(home.path(), &cancel).await.is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "corrupt");
    }
    #[test]
    fn forwarding_paths_cannot_add_ssh_forwarding_fields() {
        assert_eq!(
            forward_spec(
                "/private/remote.sock",
                std::path::Path::new("/private/local.sock")
            )
            .unwrap(),
            "/private/remote.sock:/private/local.sock"
        );
        for bad in ["relative.sock", "/tmp/a:22", "/tmp/a\n"] {
            assert!(forward_spec(bad, std::path::Path::new("/tmp/local.sock")).is_err());
        }
    }
}
