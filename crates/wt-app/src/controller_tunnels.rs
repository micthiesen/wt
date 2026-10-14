//! Independently supervised, private SSH reverse forwards for controller services.
use std::{path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::context::AppContext;

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
            &["bridge-path".into()],
            Vec::new(),
            Duration::from_secs(10),
            cancel,
        )
        .await?;
    if !output.status.success() {
        bail!("worker could not resolve its private controller socket");
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
            "StreamLocalBindUnlink=yes",
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
