//! Stable, config-free launcher policy for immutable native installations.
//!
//! The launcher probes a build before giving it user arguments. Once the
//! application receives those arguments, its exit status is returned as-is;
//! it is never treated as evidence that replaying the command is safe.

use std::{
    ffi::OsString,
    process::{Command, ExitStatus, Stdio},
    time::Duration,
};

use thiserror::Error;
use wt_update::{
    BootTransitionError, InstallManager, InstallPaths, InstallState, StateStore, VersionId,
};

pub const BOOT_PROBE_ARGUMENT: &str = "--_boot-probe";
pub const BOOT_PROBE_PREFIX: &str = "wt-build-id:";
pub const INSTALL_ROOT_ENV: &str = "WT_INSTALL_ROOT";
pub const INSTALL_VERSION_ENV: &str = "WT_INSTALL_VERSION";
pub const BUILD_ID_ENV: &str = "WT_BUILD_ID";
pub const TARGET_ENV: &str = "WT_TARGET";
pub const BOOT_ATTEMPT_TOKEN_ENV: &str = "WT_BOOT_ATTEMPT_TOKEN";
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchPlan {
    pub version: VersionId,
    pub executable: std::path::PathBuf,
    pub is_pending: bool,
    pub attempt_token: Option<String>,
}

impl LaunchPlan {
    pub fn select(paths: &InstallPaths, state: &InstallState) -> Result<Self, LaunchError> {
        let version = state
            .current
            .clone()
            .or_else(|| state.last_good.clone())
            .ok_or(LaunchError::NoInstalledVersion)?;
        let pending = state
            .pending_boot
            .as_ref()
            .filter(|pending| pending.candidate == version);
        Ok(Self {
            executable: paths.app_binary(&version),
            version,
            is_pending: pending.is_some(),
            attempt_token: pending.map(|pending| pending.attempt_token.clone()),
        })
    }
}

/// Probe, then dispatch exactly once. Probe failure while a release is pending
/// restores the known-good version before dispatch. No user arguments are ever
/// used during a probe.
pub fn launch(
    paths: &InstallPaths,
    user_args: &[OsString],
    now_unix: u64,
) -> Result<ExitStatus, LaunchError> {
    let store = StateStore::new(paths.clone());
    let manager = InstallManager::new(paths.clone());
    let mut plan = LaunchPlan::select(paths, &load_snapshot(&store)?)?;
    if !probe_matches(&plan)? {
        if !plan.is_pending {
            return Err(LaunchError::ProbeFailed(plan.version));
        }
        let initial_candidate = plan.version.clone();
        let rejected = manager.reject_unlaunchable(
            &plan.version,
            plan.attempt_token
                .as_deref()
                .ok_or(LaunchError::NoPendingVersionToken)?,
            now_unix,
        );
        match rejected {
            Ok(_) => {}
            // Another launcher resolved this same pending attempt while this
            // process was probing. Re-read state rather than acting on stale
            // assumptions; the shared lock serialized the durable transition.
            Err(BootTransitionError::NoPendingBoot | BootTransitionError::StaleConfirmation) => {}
            Err(error) => return Err(error.into()),
        }
        plan = LaunchPlan::select(paths, &load_snapshot(&store)?)?;
        if !probe_matches(&plan)? {
            if plan.version == initial_candidate {
                return Err(LaunchError::ProbeFailed(plan.version));
            }
            return Err(LaunchError::FallbackProbeFailed(plan.version));
        }
    }
    // This is the only process call that receives the user's argv. In
    // particular, a non-zero result is returned without changing install
    // state or attempting a second command.
    let mut command = Command::new(&plan.executable);
    command
        .env(INSTALL_ROOT_ENV, paths.root())
        .env(INSTALL_VERSION_ENV, plan.version.release_version())
        .env(BUILD_ID_ENV, plan.version.build_id())
        .env(TARGET_ENV, plan.version.target())
        .env_remove(BOOT_ATTEMPT_TOKEN_ENV);
    if let Some(token) = &plan.attempt_token {
        command.env(BOOT_ATTEMPT_TOKEN_ENV, token);
    }
    command
        .args(user_args)
        .status()
        .map_err(|source| LaunchError::Spawn {
            path: plan.executable,
            source,
        })
}

fn load_snapshot(store: &StateStore) -> Result<InstallState, LaunchError> {
    let _lock = store.lock_wait(Duration::from_secs(3))?;
    Ok(store.load()?)
}

pub fn probe_matches(plan: &LaunchPlan) -> Result<bool, LaunchError> {
    probe_matches_timeout(plan, PROBE_TIMEOUT)
}

fn probe_matches_timeout(plan: &LaunchPlan, timeout: Duration) -> Result<bool, LaunchError> {
    let expected = format!(
        "{BOOT_PROBE_PREFIX}{}:{}\n",
        plan.version.build_id(),
        plan.version.target()
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| LaunchError::ProbeRuntime(error.to_string()))?;
    runtime.block_on(async {
        use tokio::io::AsyncReadExt;
        let mut command = tokio::process::Command::new(&plan.executable);
        command
            .arg(BOOT_PROBE_ARGUMENT)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => return Ok(false),
        };
        let Some(stdout) = child.stdout.take() else {
            return Ok(false);
        };
        let operation = async {
            let mut output = Vec::with_capacity(expected.len() + 1);
            stdout
                .take((expected.len() + 1) as u64)
                .read_to_end(&mut output)
                .await?;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, output))
        };
        match tokio::time::timeout(timeout, operation).await {
            Err(_) => Ok(false),
            Ok(Err(_)) => Ok(false),
            Ok(Ok((status, output))) => Ok(status.success() && output == expected.as_bytes()),
        }
    })
}

/// Called by the application only after config-free startup has completed and
/// its normal initialization is healthy. A stale token always fails closed.
pub fn confirm_boot(
    paths: &InstallPaths,
    version: &VersionId,
    token: &str,
    now_unix: u64,
) -> Result<(), LaunchError> {
    InstallManager::new(paths.clone()).confirm_boot(version, token, now_unix)?;
    Ok(())
}

/// Called by early startup failure handling, never by ordinary command exit
/// handling. The returned version is safe for the stable launcher to probe.
pub fn report_boot_failure(
    paths: &InstallPaths,
    version: &VersionId,
    token: &str,
    detail: String,
    now_unix: u64,
) -> Result<Option<VersionId>, LaunchError> {
    Ok(InstallManager::new(paths.clone()).report_boot_failure(version, token, detail, now_unix)?)
}

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("install state error: {0}")]
    Store(#[from] wt_update::StoreError),
    #[error("boot state transition error: {0}")]
    Transition(#[from] BootTransitionError),
    #[error("no native wt version is installed")]
    NoInstalledVersion,
    #[error("pending version is missing its boot attempt token")]
    NoPendingVersionToken,
    #[error("candidate {0:?} failed its config-free boot probe")]
    ProbeFailed(VersionId),
    #[error("fallback {0:?} failed its config-free boot probe")]
    FallbackProbeFailed(VersionId),
    #[error("could not create boot probe runtime: {0}")]
    ProbeRuntime(String),
    #[error("could not start wt at {path}: {source}")]
    Spawn {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    use tempfile::tempdir;
    use wt_update::{Channel, PendingBoot};

    #[cfg(unix)]
    fn fake_app(path: &std::path::Path, version: &VersionId, log: &std::path::Path, code: i32) {
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"{BOOT_PROBE_ARGUMENT}\" ]; then printf '%s\\n' '{BOOT_PROBE_PREFIX}{}:{}'; exit 0; fi\nprintf '%s\\n' \"$*\" >> '{}'\nexit {code}\n",
            version.build_id(),
            version.target(),
            log.display()
        );
        fs::write(path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_user_command_is_dispatched_once_and_does_not_rollback() {
        let temp = tempdir().unwrap();
        let paths = InstallPaths::new(temp.path().join("install")).unwrap();
        let version = VersionId::new("v1", "deadbeef", "x86_64-unknown-linux-gnu").unwrap();
        let app = paths.app_binary(&version);
        fs::create_dir_all(app.parent().unwrap()).unwrap();
        let log = temp.path().join("argv.log");
        fake_app(&app, &version, &log, 7);
        let store = StateStore::new(paths.clone());
        let mut state = InstallState::new(Channel::Stable);
        state.current = Some(version.clone());
        state.last_good = Some(version.clone());
        store.save(&state).unwrap();
        let status = launch(&paths, &["write-state".into()], 8).unwrap();
        assert_eq!(status.code(), Some(7));
        assert_eq!(fs::read_to_string(&log).unwrap(), "write-state\n");
        assert_eq!(store.load().unwrap(), state);
    }

    #[cfg(unix)]
    #[test]
    fn failed_pending_probe_rolls_back_before_any_user_argv_is_seen() {
        let temp = tempdir().unwrap();
        let paths = InstallPaths::new(temp.path().join("install")).unwrap();
        let old = VersionId::new("v1", "deadbeef", "x86_64-unknown-linux-gnu").unwrap();
        let bad = VersionId::new("v2", "01234567", "x86_64-unknown-linux-gnu").unwrap();
        let log = temp.path().join("argv.log");
        for (version, probe) in [(&old, true), (&bad, false)] {
            let app = paths.app_binary(version);
            fs::create_dir_all(app.parent().unwrap()).unwrap();
            if probe {
                fake_app(&app, version, &log, 0);
            } else {
                fs::write(&app, "#!/bin/sh\nexit 9\n").unwrap();
                fs::set_permissions(&app, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let store = StateStore::new(paths.clone());
        let mut state = InstallState::new(Channel::Stable);
        state.current = Some(bad.clone());
        state.last_good = Some(old.clone());
        state.pending_boot = Some(PendingBoot {
            candidate: bad,
            fallback: Some(old.clone()),
            attempt_token: "try2".into(),
            started_unix: 1,
        });
        store.save(&state).unwrap();
        let status = launch(&paths, &["once".into()], 2).unwrap();
        assert_eq!(status.code(), Some(0));
        assert_eq!(fs::read_to_string(&log).unwrap(), "once\n");
        let state = store.load().unwrap();
        assert_eq!(state.current, Some(old));
        assert!(state.pending_boot.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn hanging_probe_is_bounded_and_fails_closed() {
        let temp = tempdir().unwrap();
        let executable = temp.path().join("hang");
        fs::write(&executable, "#!/bin/sh\nexec sleep 60\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let plan = LaunchPlan {
            version: VersionId::new("v1", "deadbeef", "x86_64-unknown-linux-gnu").unwrap(),
            executable,
            is_pending: true,
            attempt_token: Some("attempt1".into()),
        };
        assert!(!probe_matches_timeout(&plan, Duration::from_millis(25)).unwrap());
    }
}
