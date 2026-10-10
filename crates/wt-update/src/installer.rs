//! Native installation entry-point management.
//!
//! The stable launcher is replaced only after both executables from a
//! checksum-verified artifact pass their config-free probes. Its path is kept
//! at `<install-root>/bin/wt`; release launchers must remain compatible with
//! the supported state format and with fallback application builds.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use thiserror::Error;
use tokio::{io::AsyncReadExt, process::Command, task::JoinError, time::timeout};

use crate::{
    InstallError, InstallManager, InstallPaths, InstallState, StateStore, VerifiedRelease,
    VersionId,
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_PROBE_OUTPUT: u64 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    AlreadyCurrent,
}

/// Probe the candidate application and candidate launcher, then atomically
/// install the stable launcher and activate the immutable release directory.
/// The caller must have verified the archive through `ReleaseSource` first.
pub async fn install_verified(
    paths: InstallPaths,
    release: VerifiedRelease,
    attempt_token: String,
    now_unix: u64,
) -> Result<InstallOutcome, InstallerError> {
    probe_bytes(
        &paths,
        &release.app_binary,
        &release.version,
        "candidate application",
    )
    .await?;
    verify_candidate_launcher(&paths, &release).await?;
    tokio::task::spawn_blocking(move || {
        install_verified_sync(&paths, &release, attempt_token, now_unix)
    })
    .await
    .map_err(InstallerError::Join)?
}

/// Probe an already-installed version, verify its launcher against an
/// isolated state snapshot, then activate it for the next stable launch.
pub async fn activate_existing(
    paths: InstallPaths,
    version: VersionId,
    attempt_token: String,
    now_unix: u64,
) -> Result<(), InstallerError> {
    let app = paths.app_binary(&version);
    probe_path(&app, &version, "rollback candidate").await?;
    let launcher = paths.launcher_binary(&version);
    let candidate = read_regular_file(&launcher)?;
    verify_launcher_compatibility(&paths, &version, &read_regular_file(&app)?, &candidate).await?;
    tokio::task::spawn_blocking(move || {
        activate_existing_sync(&paths, &version, &candidate, attempt_token, now_unix)
    })
    .await
    .map_err(InstallerError::Join)?
}

async fn verify_candidate_launcher(
    paths: &InstallPaths,
    release: &VerifiedRelease,
) -> Result<(), InstallerError> {
    verify_launcher_compatibility(
        paths,
        &release.version,
        &release.app_binary,
        &release.launcher_binary,
    )
    .await
}

/// Execute the candidate launcher in a throwaway install tree. For an upgrade,
/// this tree contains the last-good app and the current durable state snapshot;
/// for a first install, it contains the candidate itself. This proves the new
/// launcher understands the old state/probe protocol before replacing the
/// stable entry point.
async fn verify_launcher_compatibility(
    paths: &InstallPaths,
    candidate_version: &VersionId,
    candidate_app: &[u8],
    candidate_launcher: &[u8],
) -> Result<(), InstallerError> {
    let paths = paths.clone();
    let candidate_version = candidate_version.clone();
    let candidate_app = candidate_app.to_vec();
    let launcher_bytes = candidate_launcher.to_vec();
    let install_root = paths.clone();
    let fixture = tokio::task::spawn_blocking(move || {
        let state = StateStore::new(paths.clone()).load()?;
        let installed = state
            .last_good
            .as_ref()
            .filter(|version| paths.app_binary(version).is_file())
            .or_else(|| {
                state
                    .current
                    .as_ref()
                    .filter(|version| paths.app_binary(version).is_file())
            });
        let (selected, app_bytes, fixture_state) = if let Some(version) = installed {
            let mut fixture_state = state.clone();
            fixture_state.current = Some(version.clone());
            fixture_state.last_good = Some(version.clone());
            fixture_state.pending_boot = None;
            (
                version.clone(),
                read_regular_file(&paths.app_binary(version))?,
                fixture_state,
            )
        } else {
            let mut fixture_state = InstallState::new(state.channel);
            fixture_state.current = Some(candidate_version.clone());
            fixture_state.last_good = Some(candidate_version.clone());
            fixture_state.extra = state.extra;
            (candidate_version.clone(), candidate_app, fixture_state)
        };
        Ok::<_, InstallerError>((fixture_state, selected, app_bytes, launcher_bytes))
    })
    .await
    .map_err(InstallerError::Join)??;

    let (mut state, selected, app_bytes, launcher_bytes) = fixture;
    let root = new_compatibility_root(install_root.root())?;
    let result = (|| {
        let fixture_paths = InstallPaths::new(root.clone())?;
        write_executable(&fixture_paths.launcher(), &launcher_bytes)?;
        let app_path = fixture_paths.app_binary(&selected);
        write_executable(&app_path, &app_bytes)?;
        state.current = Some(selected.clone());
        state.last_good = Some(selected.clone());
        state.pending_boot = None;
        let store = StateStore::new(fixture_paths.clone());
        store.save(&state)?;
        Ok(fixture_paths.launcher())
    })();
    let launcher_path = match result {
        Ok(path) => path,
        Err(error) => {
            let _ = fs::remove_dir_all(&root);
            return Err(error);
        }
    };
    let probe = run_bounded_probe(&launcher_path, &selected).await;
    let _ = fs::remove_dir_all(&root);
    probe
}

async fn probe_bytes(
    paths: &InstallPaths,
    bytes: &[u8],
    version: &VersionId,
    label: &'static str,
) -> Result<(), InstallerError> {
    let paths = paths.clone();
    let version = version.clone();
    let bytes = bytes.to_vec();
    let path = tokio::task::spawn_blocking(move || {
        let dir = paths.root().join("staging");
        fs::create_dir_all(&dir).map_err(|source| io_error(&dir, source))?;
        let id = next_id();
        let path = dir.join(format!("probe-{id}"));
        write_executable(&path, &bytes)?;
        Ok::<_, InstallerError>(path)
    })
    .await
    .map_err(InstallerError::Join)??;
    let result = run_bounded_probe(&path, &version)
        .await
        .map_err(|error| match error {
            InstallerError::ProbeFailed { .. } => InstallerError::ProbeFailed {
                label,
                version: version.clone(),
            },
            other => other,
        });
    let _ = fs::remove_file(&path);
    result
}

async fn probe_path(
    path: &Path,
    version: &VersionId,
    label: &'static str,
) -> Result<(), InstallerError> {
    run_bounded_probe(path, version)
        .await
        .map_err(|error| match error {
            InstallerError::ProbeFailed { .. } => InstallerError::ProbeFailed {
                label,
                version: version.clone(),
            },
            other => other,
        })
}

async fn run_bounded_probe(path: &Path, version: &VersionId) -> Result<(), InstallerError> {
    let expected = format!("wt-build-id:{}:{}\n", version.build_id(), version.target());
    let mut child = Command::new(path)
        .arg("--_boot-probe")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| io_error(path, source))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| InstallerError::ProbeFailed {
            label: "candidate executable",
            version: version.clone(),
        })?;
    let probe = async {
        let mut output = Vec::new();
        stdout
            .take(MAX_PROBE_OUTPUT)
            .read_to_end(&mut output)
            .await
            .map_err(|source| io_error(path, source))?;
        let status = child
            .wait()
            .await
            .map_err(|source| io_error(path, source))?;
        if status.success() && output == expected.as_bytes() {
            Ok(())
        } else {
            Err(InstallerError::ProbeFailed {
                label: "candidate executable",
                version: version.clone(),
            })
        }
    };
    match timeout(PROBE_TIMEOUT, probe).await {
        Ok(result) => result,
        Err(_) => Err(InstallerError::ProbeTimedOut(version.clone())),
    }
}

fn install_verified_sync(
    paths: &InstallPaths,
    release: &VerifiedRelease,
    attempt_token: String,
    now_unix: u64,
) -> Result<InstallOutcome, InstallerError> {
    let _install_lock = acquire_install_lock(paths)?;
    let store = StateStore::new(paths.clone());
    let before = store.load()?;
    let _prior_launcher = read_owned_stable_launcher(paths, &before, &release.launcher_binary)?;
    let launcher_path = paths.launcher();
    if before.current.as_ref() == Some(&release.version) {
        if !version_matches_release(paths, release)? {
            return Err(InstallerError::VersionCollision(release.version.clone()));
        }
        let prior_launcher = read_regular_file(&launcher_path).ok();
        if prior_launcher.as_deref() != Some(release.launcher_binary.as_slice()) {
            atomic_executable_write(&launcher_path, &release.launcher_binary)?;
        }
        return Ok(InstallOutcome::AlreadyCurrent);
    }

    let manager = InstallManager::new(paths.clone());
    match manager.install_verified(release, attempt_token, now_unix) {
        Ok(()) => {
            atomic_executable_write(&launcher_path, &release.launcher_binary).map_err(|error| {
                InstallerError::LauncherUpgradePending {
                    version: release.version.clone(),
                    reason: error.to_string(),
                }
            })?;
            Ok(InstallOutcome::Installed)
        }
        Err(error) => {
            match store.load() {
                Ok(after)
                    if after.current.as_ref() == Some(&release.version)
                        || after
                            .pending_boot
                            .as_ref()
                            .is_some_and(|pending| pending.candidate == release.version) =>
                {
                    // A durable activation may have committed despite an I/O
                    // error. Finish the launcher upgrade only after confirming
                    // candidate state; the old launcher can run this same
                    // state/probe protocol if this process dies first.
                    atomic_executable_write(&launcher_path, &release.launcher_binary).map_err(
                        |launcher_error| InstallerError::LauncherUpgradePending {
                            version: release.version.clone(),
                            reason: format!("{launcher_error}; activation also reported: {error}"),
                        },
                    )?;
                }
                Ok(_) => {}
                Err(_) => {
                    // State durability is ambiguous. Keep the old launcher;
                    // it is still the only version known to match the visible
                    // durable state.
                }
            }
            Err(InstallerError::Install(error))
        }
    }
}

fn activate_existing_sync(
    paths: &InstallPaths,
    version: &VersionId,
    candidate_launcher: &[u8],
    attempt_token: String,
    now_unix: u64,
) -> Result<(), InstallerError> {
    let _install_lock = acquire_install_lock(paths)?;
    let store = StateStore::new(paths.clone());
    let before = store.load()?;
    let _prior_launcher = read_owned_stable_launcher(paths, &before, candidate_launcher)?;
    let launcher_path = paths.launcher();
    let manager = InstallManager::new(paths.clone());
    match manager.activate_existing(version, attempt_token, now_unix) {
        Ok(()) => {
            atomic_executable_write(&launcher_path, candidate_launcher).map_err(|error| {
                InstallerError::LauncherUpgradePending {
                    version: version.clone(),
                    reason: error.to_string(),
                }
            })?;
            Ok(())
        }
        Err(error) => {
            match store.load() {
                Ok(after)
                    if after.current.as_ref() == Some(version)
                        || after
                            .pending_boot
                            .as_ref()
                            .is_some_and(|pending| &pending.candidate == version) =>
                {
                    atomic_executable_write(&launcher_path, candidate_launcher).map_err(
                        |launcher_error| InstallerError::LauncherUpgradePending {
                            version: version.clone(),
                            reason: format!("{launcher_error}; activation also reported: {error}"),
                        },
                    )?;
                }
                Ok(_) | Err(_) => {}
            }
            Err(InstallerError::Install(error))
        }
    }
}

fn read_owned_stable_launcher(
    paths: &InstallPaths,
    state: &InstallState,
    candidate_bytes: &[u8],
) -> Result<Option<Vec<u8>>, InstallerError> {
    let path = paths.launcher();
    let current = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(InstallerError::UnownedLauncher(path));
            }
            Some(read_regular_file(&path)?)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(source) => return Err(io_error(&path, source)),
    };
    let Some(current_bytes) = current else {
        return Ok(None);
    };
    let mut authorized = current_bytes == candidate_bytes;
    for version in [state.current.as_ref(), state.last_good.as_ref()]
        .into_iter()
        .flatten()
    {
        let versioned = paths.launcher_binary(version);
        if versioned.is_file() && read_regular_file(&versioned)? == current_bytes {
            authorized = true;
            break;
        }
    }
    if !authorized {
        return Err(InstallerError::UnownedLauncher(path));
    }
    Ok(Some(current_bytes))
}

fn version_matches_release(
    paths: &InstallPaths,
    release: &VerifiedRelease,
) -> Result<bool, InstallerError> {
    Ok(
        read_regular_file(&paths.app_binary(&release.version))? == release.app_binary
            && read_regular_file(&paths.launcher_binary(&release.version))?
                == release.launcher_binary,
    )
}

fn acquire_install_lock(paths: &InstallPaths) -> Result<File, InstallerError> {
    fs::create_dir_all(paths.root()).map_err(|source| io_error(paths.root(), source))?;
    let path = paths.root().join("install.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|source| io_error(&path, source))?;
    file.try_lock()
        .map_err(|error| InstallerError::InstallBusy(error.to_string()))?;
    Ok(file)
}

fn atomic_executable_write(path: &Path, bytes: &[u8]) -> Result<(), InstallerError> {
    let parent = path
        .parent()
        .ok_or_else(|| InstallerError::InvalidStablePath(path.to_owned()))?;
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(InstallerError::UnownedLauncher(path.to_owned()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(path, source)),
    }
    let temp = parent.join(format!(".wt-launcher-{}.tmp", next_id()));
    let write_result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o755);
        }
        let mut file = options
            .open(&temp)
            .map_err(|source| io_error(&temp, source))?;
        file.write_all(bytes)
            .map_err(|source| io_error(&temp, source))?;
        file.sync_all().map_err(|source| io_error(&temp, source))?;
        fs::rename(&temp, path).map_err(|source| io_error(path, source))?;
        sync_directory(parent).map_err(|source| io_error(parent, source))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result
}

fn read_regular_file(path: &Path) -> Result<Vec<u8>, InstallerError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if !metadata.file_type().is_file() {
        return Err(InstallerError::UnownedLauncher(path.to_owned()));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|source| io_error(path, source))?;
    Ok(bytes)
}

fn write_executable(path: &Path, bytes: &[u8]) -> Result<(), InstallerError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o755);
    }
    let mut file = options
        .open(path)
        .map_err(|source| io_error(path, source))?;
    file.write_all(bytes)
        .map_err(|source| io_error(path, source))?;
    file.sync_all().map_err(|source| io_error(path, source))?;
    Ok(())
}

fn new_compatibility_root(root: &Path) -> Result<PathBuf, InstallerError> {
    let staging = root.join("staging");
    fs::create_dir_all(&staging).map_err(|source| io_error(&staging, source))?;
    let path = staging.join(format!("compat-{}", next_id()));
    fs::create_dir(&path).map_err(|source| io_error(&path, source))?;
    Ok(path)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn io_error(path: &Path, source: io::Error) -> InstallerError {
    InstallerError::Io {
        path: path.to_owned(),
        source,
    }
}

fn next_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

#[derive(Debug, Error)]
pub enum InstallerError {
    #[error("install state error: {0}")]
    Store(#[from] crate::StoreError),
    #[error("install failed: {0}")]
    Install(#[from] InstallError),
    #[error("invalid install path: {0}")]
    Path(#[from] crate::PathError),
    #[error("installation filesystem operation at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("installer is already running for this install root: {0}")]
    InstallBusy(String),
    #[error("stable launcher path is invalid: {0}")]
    InvalidStablePath(PathBuf),
    #[error("refusing to replace unowned stable launcher at {0}")]
    UnownedLauncher(PathBuf),
    #[error("candidate {0:?} differs from its immutable installed directory")]
    VersionCollision(VersionId),
    #[error("{version:?} is active, but its stable launcher was not upgraded: {reason}")]
    LauncherUpgradePending { version: VersionId, reason: String },
    #[error("{label} failed its config-free identity probe for {version:?}")]
    ProbeFailed {
        label: &'static str,
        version: VersionId,
    },
    #[error("candidate probe timed out for {0:?}")]
    ProbeTimedOut(VersionId),
    #[error("installer task failed: {0}")]
    Join(#[source] JoinError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateHistoryEntry;
    use tempfile::tempdir;

    const TARGET: &str = "x86_64-unknown-linux-gnu";
    const BUILD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn version(tag: &str, build: &str) -> VersionId {
        VersionId::new(tag, build, TARGET).unwrap()
    }

    fn fake_app(version: &VersionId) -> Vec<u8> {
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--_boot-probe\" ]; then printf '%s\\n' 'wt-build-id:{}:{}'; exit 0; fi\nexit 0\n",
            version.build_id(),
            version.target()
        )
        .into_bytes()
    }

    fn fake_launcher(version: &VersionId) -> Vec<u8> {
        let _ = version;
        b"#!/bin/sh\nroot=$(CDPATH= cd -P \"$(dirname \"$0\")/..\" && pwd)\nset -- \"$root\"/versions/*/bin/wt\nexec \"$1\" --_boot-probe\n".to_vec()
    }

    fn verified(version: VersionId) -> VerifiedRelease {
        VerifiedRelease {
            version: version.clone(),
            app_binary: fake_app(&version),
            launcher_binary: fake_launcher(&version),
            archive_sha256: "a".repeat(64),
        }
    }

    #[tokio::test]
    async fn first_install_probes_before_writing_stable_launcher_and_is_idempotent() {
        let temp = tempdir().unwrap();
        let paths = InstallPaths::new(temp.path().join("install")).unwrap();
        let release = verified(version("v1.2.3", BUILD));
        assert_eq!(
            install_verified(paths.clone(), release.clone(), "attempt-1".into(), 1)
                .await
                .unwrap(),
            InstallOutcome::Installed
        );
        assert_eq!(fs::read(paths.launcher()).unwrap(), release.launcher_binary);
        assert_eq!(
            StateStore::new(paths.clone()).load().unwrap().current,
            Some(release.version.clone())
        );
        assert_eq!(
            install_verified(paths.clone(), release, "attempt-2".into(), 2)
                .await
                .unwrap(),
            InstallOutcome::AlreadyCurrent
        );
    }

    #[tokio::test]
    async fn bad_candidate_probe_does_not_replace_existing_launcher_or_state() {
        let temp = tempdir().unwrap();
        let paths = InstallPaths::new(temp.path().join("install")).unwrap();
        let old = verified(version(
            "v1.2.2",
            "1111111111111111111111111111111111111111",
        ));
        install_verified(paths.clone(), old.clone(), "attempt-old".into(), 1)
            .await
            .unwrap();
        let old_launcher = fs::read(paths.launcher()).unwrap();
        let old_state = StateStore::new(paths.clone()).load().unwrap();
        let mut bad = verified(version("v1.2.3", BUILD));
        bad.app_binary = b"#!/bin/sh\nexit 1\n".to_vec();
        assert!(
            install_verified(paths.clone(), bad, "attempt-bad".into(), 2)
                .await
                .is_err()
        );
        assert_eq!(fs::read(paths.launcher()).unwrap(), old_launcher);
        assert_eq!(StateStore::new(paths).load().unwrap(), old_state);
    }

    #[tokio::test]
    async fn update_replaces_only_owned_launcher_and_preserves_previous_version() {
        let temp = tempdir().unwrap();
        let paths = InstallPaths::new(temp.path().join("install")).unwrap();
        let old = verified(version(
            "v1.2.2",
            "1111111111111111111111111111111111111111",
        ));
        install_verified(paths.clone(), old.clone(), "attempt-old".into(), 1)
            .await
            .unwrap();
        let new = verified(version("v1.2.3", BUILD));
        install_verified(paths.clone(), new.clone(), "attempt-new".into(), 2)
            .await
            .unwrap();
        assert_eq!(fs::read(paths.launcher()).unwrap(), new.launcher_binary);
        assert!(paths.app_binary(&old.version).is_file());
        assert_eq!(
            StateStore::new(paths.clone())
                .load()
                .unwrap()
                .pending_boot
                .unwrap()
                .fallback,
            Some(old.version)
        );

        let path = paths.launcher();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"unowned file").unwrap();
        let later = verified(version(
            "v1.2.4",
            "2222222222222222222222222222222222222222",
        ));
        assert!(matches!(
            install_verified(paths.clone(), later, "attempt-later".into(), 3).await,
            Err(InstallerError::UnownedLauncher(_))
        ));
        assert_eq!(fs::read(path).unwrap(), b"unowned file");
    }

    #[tokio::test]
    async fn rollback_switches_stable_launcher_and_keeps_fallback() {
        let temp = tempdir().unwrap();
        let paths = InstallPaths::new(temp.path().join("install")).unwrap();
        let old = verified(version(
            "v1.2.2",
            "1111111111111111111111111111111111111111",
        ));
        install_verified(paths.clone(), old.clone(), "attempt-old".into(), 1)
            .await
            .unwrap();
        let new = verified(version("v1.2.3", BUILD));
        install_verified(paths.clone(), new.clone(), "attempt-new".into(), 2)
            .await
            .unwrap();
        activate_existing(
            paths.clone(),
            old.version.clone(),
            "attempt-rollback".into(),
            3,
        )
        .await
        .unwrap();
        assert_eq!(fs::read(paths.launcher()).unwrap(), old.launcher_binary);
        let state = StateStore::new(paths).load().unwrap();
        assert_eq!(state.current, Some(old.version));
        assert_eq!(state.declined_build_id.as_deref(), Some(BUILD));
    }

    #[tokio::test]
    async fn candidate_launcher_is_checked_against_old_state_with_unknown_fields() {
        let temp = tempdir().unwrap();
        let paths = InstallPaths::new(temp.path().join("install")).unwrap();
        let old = verified(version(
            "v1.2.2",
            "1111111111111111111111111111111111111111",
        ));
        install_verified(paths.clone(), old.clone(), "attempt-old".into(), 1)
            .await
            .unwrap();
        let store = StateStore::new(paths.clone());
        let mut state = store.load().unwrap();
        state
            .extra
            .insert("legacyPolicy".into(), serde_json::json!({"kept": true}));
        state.history.push(StateHistoryEntry {
            at_unix: 1,
            operation: "boot-confirmed".into(),
            from: None,
            to: Some(old.version.clone()),
            detail: Some("old-launcher".into()),
        });
        store.save(&state).unwrap();
        let new = verified(version("v1.2.3", BUILD));
        assert!(verify_candidate_launcher(&paths, &new).await.is_ok());
    }
}
