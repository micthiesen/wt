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
        if version.build_id() != env!("WT_BUILD_ID") || version.target() != env!("WT_TARGET") {
            bail!("native launcher identity does not match the running binary");
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

    pub async fn confirm(self) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            wt_launcher::confirm_boot(&self.paths, &self.version, &self.token, now())
                .map_err(anyhow::Error::from)
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
}
