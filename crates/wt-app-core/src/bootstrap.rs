//! Acknowledge repository-independent startup before configuration or state
//! migration. A broken config in one repository must not reject a binary that
//! other repositories are using. Command failures never replay an operation.
use anyhow::{Context, Result, bail};
use wt_config::LoadOptions;
use wt_update::{InstallPaths, VersionId};

#[derive(Clone)]
pub struct BootAttempt {
    paths: InstallPaths,
    version: VersionId,
    token: String,
}

impl BootAttempt {
    pub fn from_options(options: &LoadOptions) -> Result<Option<Self>> {
        let Some(token) = options.env.get(wt_launcher::BOOT_ATTEMPT_TOKEN_ENV) else {
            return Ok(None);
        };
        let required = |key: &str| {
            options
                .env
                .get(key)
                .with_context(|| format!("native launcher omitted {key}"))
        };
        let paths = InstallPaths::new(required(wt_launcher::INSTALL_ROOT_ENV)?)?;
        let version = VersionId::new(
            required(wt_launcher::INSTALL_VERSION_ENV)?,
            required(wt_launcher::BUILD_ID_ENV)?,
            required(wt_launcher::TARGET_ENV)?,
        )?;
        if version.build_id() != crate::build_info::build_id()
            || version.target() != crate::build_info::target()
        {
            // The launcher probes the binary it starts for exactly this
            // identity, so a token naming another build was issued for
            // another launch and inherited, typically from a tmux server
            // started by a build that still exported it. Nothing is owed.
            return Ok(None);
        }
        let running = std::env::current_exe()?.canonicalize()?;
        let expected = paths.app_binary(&version).canonicalize()?;
        if running != expected {
            bail!("boot acknowledgement refused outside the immutable installation");
        }
        Ok(Some(Self {
            paths,
            version,
            token: token.clone(),
        }))
    }

    /// Confirm the pending launch this process is. A token that is not the
    /// pending attempt (inherited from an earlier launch, or already settled
    /// by another process) leaves the install state alone and lets the
    /// command run: failing it would break every worker a long-lived tmux
    /// server starts, and it is not the launch being confirmed.
    pub async fn confirm(self) -> Result<()> {
        use wt_launcher::LaunchError::Transition;
        use wt_update::BootTransitionError::{NoPendingBoot, StaleConfirmation};
        tokio::task::spawn_blocking(move || {
            match wt_launcher::confirm_boot(&self.paths, &self.version, &self.token, now()) {
                Err(Transition(NoPendingBoot | StaleConfirmation)) => Ok(()),
                result => result.map_err(anyhow::Error::from),
            }
        })
        .await?
    }

    pub async fn failed(self, detail: String) -> Result<()> {
        tokio::task::spawn_blocking(move || self.failed_sync(detail)).await?
    }

    pub fn failed_sync(self, detail: String) -> Result<()> {
        wt_launcher::report_boot_failure(&self.paths, &self.version, &self.token, detail, now())?;
        Ok(())
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn boot_confirmation_is_idempotent_for_the_same_installed_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let paths = InstallPaths::new(directory.path()).unwrap();
        let version = VersionId::new("test", "abc123", "test-target").unwrap();
        let mut state = wt_update::InstallState::new(wt_update::Channel::Preview);
        state.current = Some(version.clone());
        state.pending_boot = Some(wt_update::PendingBoot {
            candidate: version.clone(),
            fallback: None,
            attempt_token: "attempt".into(),
            started_unix: 0,
        });
        let store = wt_update::StateStore::new(paths.clone());
        store.save(&state).unwrap();
        let attempt = BootAttempt {
            paths,
            version: version.clone(),
            token: "attempt".into(),
        };
        let (first, second) = tokio::join!(attempt.clone().confirm(), attempt.clone().confirm());
        first.unwrap();
        second.unwrap();
        attempt.confirm().await.unwrap();
        let state = store.load().unwrap();
        assert!(state.pending_boot.is_none());
        assert_eq!(state.last_good, Some(version));
    }

    /// A tmux server started by a build that still exported the token hands
    /// it to every pane it runs, long after that launch was confirmed.
    #[tokio::test]
    async fn a_token_that_is_not_the_pending_attempt_runs_without_touching_state() {
        let directory = tempfile::tempdir().unwrap();
        let paths = InstallPaths::new(directory.path()).unwrap();
        let old = VersionId::new("old", "abc123", "test-target").unwrap();
        let new = VersionId::new("new", "def456", "test-target").unwrap();
        let store = wt_update::StateStore::new(paths.clone());
        let stale = BootAttempt {
            paths,
            version: old.clone(),
            token: "attempt-old".into(),
        };

        let mut state = wt_update::InstallState::new(wt_update::Channel::Preview);
        state.current = Some(new.clone());
        state.last_good = Some(old.clone());
        state.pending_boot = Some(wt_update::PendingBoot {
            candidate: new.clone(),
            fallback: Some(old.clone()),
            attempt_token: "attempt-new".into(),
            started_unix: 2,
        });
        store.save(&state).unwrap();
        stale.clone().confirm().await.unwrap();
        assert_eq!(store.load().unwrap(), state);

        state.last_good = Some(new);
        state.pending_boot = None;
        store.save(&state).unwrap();
        stale.confirm().await.unwrap();
        assert_eq!(store.load().unwrap(), state);
    }

    #[test]
    fn a_token_issued_for_another_build_is_not_this_launch() {
        let env = [
            (wt_launcher::BOOT_ATTEMPT_TOKEN_ENV, "attempt-old"),
            (wt_launcher::INSTALL_ROOT_ENV, "/nonexistent/wt"),
            (wt_launcher::INSTALL_VERSION_ENV, "old"),
            (wt_launcher::BUILD_ID_ENV, "not-this-build"),
            (wt_launcher::TARGET_ENV, crate::build_info::target()),
        ];
        let options = LoadOptions {
            cwd: "/".into(),
            home: "/".into(),
            env: env
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
        };
        assert!(BootAttempt::from_options(&options).unwrap().is_none());
    }
}
