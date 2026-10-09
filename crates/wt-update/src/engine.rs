use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
};

use thiserror::Error;

use crate::{
    InstallPaths, PendingBoot, StateHistoryEntry, StateStore, StoreError, VerifiedRelease,
    VersionId,
};

/// Filesystem and durable-state operations for verified native releases.
/// Every mutating operation holds the short-lived state lock; it never owns an
/// application process or waits for one to exit.
#[derive(Clone, Debug)]
pub struct InstallManager {
    store: StateStore,
}

impl InstallManager {
    pub fn new(paths: InstallPaths) -> Self {
        Self {
            store: StateStore::new(paths),
        }
    }

    pub fn store(&self) -> &StateStore {
        &self.store
    }

    /// Install already-verified bytes into a new immutable version directory
    /// and mark it pending. The launcher decides whether it can boot it.
    pub fn install_verified(
        &self,
        release: &VerifiedRelease,
        attempt_token: String,
        now_unix: u64,
    ) -> Result<(), InstallError> {
        if !crate::validate_component(&attempt_token) || !release.version.is_valid() {
            return Err(InstallError::InvalidAttemptToken);
        }
        let paths = self.store.paths();
        let version_dir = paths.version_dir(&release.version);
        let staging =
            paths
                .staging_dir()
                .join(format!("{}-{}", release.version.key(), attempt_token));
        fs::create_dir_all(paths.staging_dir())
            .map_err(|source| io_error(&paths.staging_dir(), source))?;
        fs::create_dir(&staging).map_err(|source| io_error(&staging, source))?;
        let result = (|| {
            let bin = staging.join("bin");
            fs::create_dir(&bin).map_err(|source| io_error(&bin, source))?;
            write_executable(&bin.join("wt"), &release.app_binary)?;
            write_executable(&bin.join("wt-launcher"), &release.launcher_binary)?;
            sync_directory(&bin).map_err(|source| io_error(&bin, source))?;
            let parent = version_dir.parent().expect("version path has parent");
            fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
            let _lock = self.store.lock()?;
            let previous_state = self.store.load()?;
            if version_dir.exists() {
                if previous_state.current.as_ref() == Some(&release.version) {
                    return Err(InstallError::VersionAlreadyInstalled(
                        release.version.clone(),
                    ));
                }
                let matches = fs::read(paths.app_binary(&release.version))
                    .is_ok_and(|bytes| bytes == release.app_binary)
                    && fs::read(paths.launcher_binary(&release.version))
                        .is_ok_and(|bytes| bytes == release.launcher_binary);
                if !matches {
                    return Err(InstallError::VersionAlreadyInstalled(
                        release.version.clone(),
                    ));
                }
                fs::remove_dir_all(&staging).map_err(|source| io_error(&staging, source))?;
            }
            let mut state = previous_state.clone();
            let fallback = state.last_good.clone().or_else(|| state.current.clone());
            let previous = state.current.clone();
            state.current = Some(release.version.clone());
            state.pending_boot = Some(PendingBoot {
                candidate: release.version.clone(),
                fallback: fallback.clone(),
                attempt_token,
                started_unix: now_unix,
            });
            state.declined_build_id = None;
            state.push_history(StateHistoryEntry {
                at_unix: now_unix,
                operation: "install".into(),
                from: previous,
                to: Some(release.version.clone()),
                detail: Some(release.archive_sha256.clone()),
            });
            // The immutable directory becomes durable before state points at
            // it. A crash can leave an unreferenced version, never a current
            // identity whose files are missing.
            if !version_dir.exists() {
                if let Err(source) = fs::rename(&staging, &version_dir) {
                    return Err(io_error(&version_dir, source));
                }
                sync_directory(parent).map_err(|source| io_error(parent, source))?;
            }
            self.store.save(&state)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }

    /// Confirmation is tied to both the build and unique pending token, so a
    /// delayed process cannot bless a later candidate.
    pub fn confirm_boot(
        &self,
        version: &VersionId,
        token: &str,
        now_unix: u64,
    ) -> Result<(), BootTransitionError> {
        let _lock = self.store.lock_wait(std::time::Duration::from_secs(3))?;
        let mut state = self.store.load()?;
        if state.pending_boot.is_none()
            && state.current.as_ref() == Some(version)
            && state.last_good.as_ref() == Some(version)
            && state.history.last().is_some_and(|entry| {
                entry.operation == "boot-confirmed"
                    && entry.to.as_ref() == Some(version)
                    && entry.detail.as_deref() == Some(token)
            })
        {
            return Ok(());
        }
        let pending = state
            .pending_boot
            .as_ref()
            .ok_or(BootTransitionError::NoPendingBoot)?;
        if &pending.candidate != version || pending.attempt_token != token {
            return Err(BootTransitionError::StaleConfirmation);
        }
        let fallback = pending.fallback.clone();
        state.current = Some(version.clone());
        state.last_good = Some(version.clone());
        state.pending_boot = None;
        state.push_history(StateHistoryEntry {
            at_unix: now_unix,
            operation: "boot-confirmed".into(),
            from: fallback,
            to: Some(version.clone()),
            detail: Some(token.into()),
        });
        self.store.save(&state)?;
        Ok(())
    }

    /// Explicit early-startup failure restores the prior immutable version.
    /// Ordinary command exit statuses must never call this method.
    pub fn report_boot_failure(
        &self,
        version: &VersionId,
        token: &str,
        detail: String,
        now_unix: u64,
    ) -> Result<Option<VersionId>, BootTransitionError> {
        let _lock = self.store.lock()?;
        let mut state = self.store.load()?;
        let pending = state
            .pending_boot
            .as_ref()
            .ok_or(BootTransitionError::NoPendingBoot)?;
        if &pending.candidate != version || pending.attempt_token != token {
            return Err(BootTransitionError::StaleConfirmation);
        }
        let fallback = pending.fallback.clone();
        state.current = fallback.clone();
        state.pending_boot = None;
        state.declined_build_id = Some(version.build_id().to_owned());
        state.push_history(StateHistoryEntry {
            at_unix: now_unix,
            operation: "boot-failed".into(),
            from: Some(version.clone()),
            to: fallback.clone(),
            detail: Some(detail.chars().take(512).collect()),
        });
        self.store.save(&state)?;
        Ok(fallback)
    }

    /// A candidate that cannot execute its config-free probe is known bad.
    /// This is safe before the launcher has dispatched user arguments.
    pub fn reject_unlaunchable(
        &self,
        version: &VersionId,
        token: &str,
        now_unix: u64,
    ) -> Result<Option<VersionId>, BootTransitionError> {
        let _lock = self.store.lock()?;
        let mut state = self.store.load()?;
        let pending = state
            .pending_boot
            .as_ref()
            .ok_or(BootTransitionError::NoPendingBoot)?;
        if &pending.candidate != version || pending.attempt_token != token {
            return Err(BootTransitionError::StaleConfirmation);
        }
        let fallback = pending.fallback.clone();
        state.current = fallback.clone();
        state.pending_boot = None;
        state.declined_build_id = Some(version.build_id().to_owned());
        state.push_history(StateHistoryEntry {
            at_unix: now_unix,
            operation: "probe-failed".into(),
            from: Some(version.clone()),
            to: fallback.clone(),
            detail: None,
        });
        self.store.save(&state)?;
        Ok(fallback)
    }

    pub fn decline(&self, build_id: &str) -> Result<(), InstallError> {
        if !crate::validate_component(build_id) {
            return Err(InstallError::InvalidBuildId);
        }
        let _lock = self.store.lock()?;
        let mut state = self.store.load()?;
        state.declined_build_id = Some(build_id.into());
        self.store.save(&state)?;
        Ok(())
    }

    /// Record the channel and the last attempted check in one durable write.
    pub fn record_check(&self, channel: crate::Channel, now_unix: u64) -> Result<(), InstallError> {
        let _lock = self.store.lock()?;
        let mut state = self.store.load()?;
        state.channel = channel;
        state.last_check_unix = Some(now_unix);
        self.store.save(&state)?;
        Ok(())
    }

    /// Activate a previously installed, pre-probed immutable version as a
    /// pending boot. The current build is declined so startup does not offer
    /// the known-bad release again immediately.
    pub fn activate_existing(
        &self,
        version: &VersionId,
        attempt_token: String,
        now_unix: u64,
    ) -> Result<(), InstallError> {
        if !crate::validate_component(&attempt_token) || !version.is_valid() {
            return Err(InstallError::InvalidAttemptToken);
        }
        let paths = self.store.paths();
        if !paths.app_binary(version).is_file() || !paths.launcher_binary(version).is_file() {
            return Err(InstallError::VersionFilesMissing(version.clone()));
        }
        let _lock = self.store.lock()?;
        let mut state = self.store.load()?;
        let from = state.current.clone();
        if from.as_ref() == Some(version) {
            return Err(InstallError::AlreadyCurrent(version.clone()));
        }
        let fallback = from.clone();
        if let Some(current) = &from {
            state.declined_build_id = Some(current.build_id().to_owned());
        }
        state.current = Some(version.clone());
        state.pending_boot = Some(PendingBoot {
            candidate: version.clone(),
            fallback,
            attempt_token,
            started_unix: now_unix,
        });
        state.push_history(StateHistoryEntry {
            at_unix: now_unix,
            operation: "rollback".into(),
            from,
            to: Some(version.clone()),
            detail: Some("pre-probed installed version".into()),
        });
        self.store.save(&state)?;
        Ok(())
    }
}

fn write_executable(path: &PathBuf, bytes: &[u8]) -> Result<(), InstallError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    file.write_all(bytes)
        .map_err(|source| io_error(path, source))?;
    file.sync_all().map_err(|source| io_error(path, source))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .map_err(|source| io_error(path, source))?;
    }
    Ok(())
}

fn sync_directory(path: &std::path::Path) -> io::Result<()> {
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

fn io_error(path: &std::path::Path, source: io::Error) -> InstallError {
    InstallError::Io {
        path: path.to_owned(),
        source,
    }
}

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("install state error: {0}")]
    Store(#[from] StoreError),
    #[error("install operation at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid boot attempt token")]
    InvalidAttemptToken,
    #[error("invalid build id")]
    InvalidBuildId,
    #[error("version is already installed: {0:?}")]
    VersionAlreadyInstalled(VersionId),
    #[error("version files are missing: {0:?}")]
    VersionFilesMissing(VersionId),
    #[error("version is already active: {0:?}")]
    AlreadyCurrent(VersionId),
    #[error(
        "activation at {path} failed ({activation}) and prior state could not be restored ({restore}); launcher recovery is required"
    )]
    ActivationAmbiguous {
        path: PathBuf,
        #[source]
        activation: io::Error,
        restore: String,
    },
}

#[derive(Debug, Error)]
pub enum BootTransitionError {
    #[error("install state error: {0}")]
    Store(#[from] StoreError),
    #[error("no candidate boot is pending")]
    NoPendingBoot,
    #[error("boot confirmation does not match the active candidate")]
    StaleConfirmation,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InstallState;
    use tempfile::tempdir;

    fn release(id: &str) -> VerifiedRelease {
        VerifiedRelease {
            version: VersionId::new(id, id, "x86_64-unknown-linux-gnu").unwrap(),
            app_binary: b"app".to_vec(),
            launcher_binary: b"launcher".to_vec(),
            archive_sha256: "abc".into(),
        }
    }

    #[test]
    fn install_is_immutable_and_boot_transitions_require_matching_token() {
        let dir = tempdir().unwrap();
        let manager = InstallManager::new(InstallPaths::new(dir.path().join("install")).unwrap());
        manager
            .install_verified(&release("v1"), "attempt1".into(), 1)
            .unwrap();
        assert!(
            manager
                .install_verified(&release("v1"), "attempt2".into(), 2)
                .is_err()
        );
        assert!(matches!(
            manager.confirm_boot(&release("v1").version, "wrong", 3),
            Err(BootTransitionError::StaleConfirmation)
        ));
        manager
            .confirm_boot(&release("v1").version, "attempt1", 3)
            .unwrap();
        let state = manager.store.load().unwrap();
        assert_eq!(state.last_good, Some(release("v1").version));
        assert!(state.pending_boot.is_none());
    }

    #[test]
    fn explicit_failure_restores_fallback_and_declines_candidate() {
        let dir = tempdir().unwrap();
        let manager = InstallManager::new(InstallPaths::new(dir.path().join("install")).unwrap());
        manager
            .install_verified(&release("v1"), "a1".into(), 1)
            .unwrap();
        manager
            .confirm_boot(&release("v1").version, "a1", 2)
            .unwrap();
        manager
            .install_verified(&release("v2"), "a2".into(), 3)
            .unwrap();
        let fallback = manager
            .report_boot_failure(&release("v2").version, "a2", "startup error".into(), 4)
            .unwrap();
        assert_eq!(fallback, Some(release("v1").version.clone()));
        let state = manager.store.load().unwrap();
        assert_eq!(state.current, fallback);
        assert_eq!(state.declined_build_id.as_deref(), Some("v2"));
    }

    #[test]
    fn rollback_activation_is_pending_and_atomically_declines_the_build_left_behind() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let paths = InstallPaths::new(dir.path().join("install")).unwrap();
        let current = VersionId::new("v2", "currentbuild", "x86_64-unknown-linux-gnu").unwrap();
        let prior = VersionId::new("v1", "priorbuild", "x86_64-unknown-linux-gnu").unwrap();
        for version in [&current, &prior] {
            let app = paths.app_binary(version);
            let launcher = paths.launcher_binary(version);
            fs::create_dir_all(app.parent().unwrap()).unwrap();
            fs::write(&app, "probe").unwrap();
            fs::write(&launcher, "launcher").unwrap();
            fs::set_permissions(app, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(launcher, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let store = StateStore::new(paths.clone());
        let mut state = InstallState::new(crate::Channel::Preview);
        state.current = Some(current.clone());
        state.last_good = Some(current.clone());
        store.save(&state).unwrap();
        InstallManager::new(paths)
            .activate_existing(&prior, "rollback-1".into(), 8)
            .unwrap();
        let state = store.load().unwrap();
        assert_eq!(state.current, Some(prior.clone()));
        assert_eq!(
            state.pending_boot.as_ref().unwrap().fallback,
            Some(current.clone())
        );
        assert_eq!(state.declined_build_id.as_deref(), Some(current.build_id()));
    }

    #[test]
    fn channel_and_daily_check_stamp_are_written_together() {
        let dir = tempdir().unwrap();
        let manager = InstallManager::new(InstallPaths::new(dir.path().join("install")).unwrap());
        manager.record_check(crate::Channel::Preview, 123).unwrap();
        let state = manager.store().load().unwrap();
        assert_eq!(state.channel, crate::Channel::Preview);
        assert_eq!(state.last_check_unix, Some(123));
    }
}
