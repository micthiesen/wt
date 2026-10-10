//! Config-light native release operations. No source checkout or git metadata
//! participates in release discovery or installation.

use anyhow::{Context, Result, bail};
use std::{
    io::IsTerminal,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;
use wt_config::{Config, LoadOptions};
use wt_update::{
    Channel, InstallManager, InstallPaths, InstallState, ReleaseRepository, ReleaseSource,
    StateStore,
};

const NETWORK_TIMEOUT: Duration = Duration::from_secs(100);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    Current,
    Declined,
    Available { release: String, build_id: String },
    Installed { release: String, build_id: String },
}

pub fn install_paths(options: &LoadOptions) -> Result<InstallPaths> {
    let root = options
        .env
        .get(wt_launcher::INSTALL_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| options.home.join(".local/share/wt"));
    InstallPaths::new(root).context("resolve native wt installation root")
}
pub fn repository(options: &LoadOptions) -> Result<ReleaseRepository> {
    let slug = options
        .env
        .get("WT_RELEASE_REPOSITORY")
        .map(String::as_str)
        .unwrap_or("micthiesen/wt");
    let (owner, name) = slug
        .split_once('/')
        .filter(|(owner, name)| !owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .ok_or_else(|| anyhow::anyhow!("WT_RELEASE_REPOSITORY must be owner/name"))?;
    let repository = ReleaseRepository::new(owner, name).context("validate release repository")?;
    match options.env.get("WT_RELEASE_API_BASE") {
        Some(base) => repository
            .with_api_base(base.clone())
            .context("validate release API base (HTTP is allowed only for loopback fixtures)"),
        None => Ok(repository),
    }
}
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub async fn update_once(
    paths: InstallPaths,
    repository: ReleaseRepository,
    target: String,
    channel_override: Option<Channel>,
    release_tag: Option<String>,
    check_only: bool,
    cancel: &CancellationToken,
) -> Result<UpdateOutcome> {
    let store = StateStore::new(paths.clone());
    let state = tokio::task::spawn_blocking({
        let store = store.clone();
        move || store.load()
    })
    .await
    .context("join update state read")??;
    let channel = channel_override.unwrap_or(state.channel);
    let manager = InstallManager::new(paths);
    let at = now_unix();
    tokio::task::spawn_blocking({
        let manager = manager.clone();
        move || manager.record_check(channel, at)
    })
    .await
    .context("join update check stamp")??;
    let source = ReleaseSource::new(repository)?;
    let release = tokio::select! {
        biased;
        _=cancel.cancelled()=>bail!("update check cancelled"),
        result=tokio::time::timeout(NETWORK_TIMEOUT,async {
            match release_tag.as_deref() {
                Some(tag) => source.by_tag(tag).await,
                None => source.latest(channel).await,
            }
        })=>result.context("release metadata request timed out")??,
    };
    let version = release.version_id(&target)?;
    if state.current.as_ref() == Some(&version) {
        return Ok(UpdateOutcome::Current);
    }
    if check_only {
        return if state.declined_build_id.as_deref() == Some(version.build_id()) {
            Ok(UpdateOutcome::Declined)
        } else {
            Ok(UpdateOutcome::Available {
                release: version.release_version().into(),
                build_id: version.build_id().into(),
            })
        };
    }
    // An explicit `wt update` is the deliberate reapply path for a declined
    // build. Startup checks still honor the decline until the build changes.
    let verified = tokio::select! {
        biased;
        _=cancel.cancelled()=>bail!("update download cancelled"),
        result=tokio::time::timeout(NETWORK_TIMEOUT,source.download_verified(&release,&target))=>result.context("release archive download timed out")??,
    };
    // Another explicit updater may have completed while this request was
    // downloading. Recheck durable state before attempting activation.
    let latest_state = tokio::task::spawn_blocking({
        let store = StateStore::new(manager.store().paths().clone());
        move || store.load()
    })
    .await
    .context("join update state refresh")??;
    if latest_state.current.as_ref() == Some(&version) {
        return Ok(UpdateOutcome::Current);
    }
    let token = attempt_token();
    let installed_version = version.clone();
    wt_update::install_verified(manager.store().paths().clone(), verified, token, now_unix())
        .await?;
    Ok(UpdateOutcome::Installed {
        release: installed_version.release_version().into(),
        build_id: installed_version.build_id().into(),
    })
}

pub fn rollback_candidate(
    state: &InstallState,
    explicit: Option<&str>,
) -> Result<wt_update::VersionId> {
    let mut candidates = Vec::new();
    if let Some(pending) = &state.pending_boot
        && let Some(fallback) = &pending.fallback
    {
        candidates.push(fallback.clone())
    }
    if state.current != state.last_good
        && let Some(good) = &state.last_good
    {
        candidates.push(good.clone())
    }
    for entry in state.history.iter().rev() {
        if entry.operation == "boot-confirmed"
            && let Some(version) = &entry.to
            && state.current.as_ref() != Some(version)
        {
            candidates.push(version.clone())
        }
    }
    candidates.dedup();
    if let Some(reference) = explicit {
        let version = candidates
            .iter()
            .find(|v| {
                v.key() == reference
                    || v.release_version() == reference
                    || v.build_id() == reference
            })
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no installed prior version matches `{reference}`"))?;
        return Ok(version);
    }
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no prior known-good version is recorded"))
}

pub async fn rollback_once(
    paths: InstallPaths,
    explicit: Option<String>,
) -> Result<wt_update::VersionId> {
    let store = StateStore::new(paths.clone());
    let state = tokio::task::spawn_blocking({
        let store = store.clone();
        move || store.load()
    })
    .await
    .context("join rollback state read")??;
    let candidate = rollback_candidate(&state, explicit.as_deref())?;
    let plan = wt_launcher::LaunchPlan {
        version: candidate.clone(),
        executable: paths.app_binary(&candidate),
        is_pending: false,
        attempt_token: None,
    };
    if !tokio::task::spawn_blocking(move || wt_launcher::probe_matches(&plan))
        .await
        .context("join rollback candidate probe")??
    {
        bail!(
            "rollback candidate {} failed its config-free probe; active version is unchanged",
            candidate.build_id()
        )
    }
    wt_update::activate_existing(paths, candidate.clone(), attempt_token(), now_unix()).await?;
    Ok(candidate)
}

pub async fn record_decline(paths: InstallPaths, build_id: String) -> Result<()> {
    tokio::task::spawn_blocking(move || InstallManager::new(paths).decline(&build_id))
        .await
        .context("join update decline")??;
    Ok(())
}

/// Config-aware daily offer. Call only after config has loaded; this function
/// does not require the database, repository inventory or a source checkout.
pub async fn startup_check(
    options: &LoadOptions,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<()> {
    // The controller installs an exact worker runtime before connecting.
    // An interactive worker must not independently offer a different build.
    if config.instance.role == wt_config::InstanceRole::Worker
        || options
            .env
            .get("WT_UPDATE")
            .is_some_and(|value| value == "off")
        || !config.update.startup_check
    {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(());
    }
    let paths = install_paths(options)?;
    let store = StateStore::new(paths.clone());
    let state = tokio::task::spawn_blocking({
        let store = store.clone();
        move || store.load()
    })
    .await
    .context("join startup update state read")??;
    if !wt_update::update_check_due(state.last_check_unix, now_unix()) {
        return Ok(());
    }
    if state.current.is_none() {
        return Ok(());
    }
    let channel = state.channel;
    let outcome = match update_latest(
        paths.clone(),
        repository(options)?,
        env!("WT_TARGET").to_owned(),
        channel,
        None,
        cancel,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("wt: update check unavailable: {error:#}");
            return Ok(());
        }
    };
    let UpdateOutcome::Available { release, build_id } = outcome else {
        return Ok(());
    };
    if state.declined_build_id.as_deref() == Some(&build_id) {
        return Ok(());
    }
    if !crate::prompt::confirm(
        &format!("wt {release} ({build_id}) is available. Install it? [y/N] "),
        false,
        cancel,
    )
    .await?
    {
        record_decline(paths, build_id).await?;
        return Ok(());
    }
    match update_once(
        paths,
        repository(options)?,
        env!("WT_TARGET").to_owned(),
        Some(channel),
        None,
        false,
        cancel,
    )
    .await
    {
        Ok(UpdateOutcome::Installed { release, build_id }) => {
            eprintln!("wt: installed {release} ({build_id}); it will run next time wt starts")
        }
        Ok(_) => {}
        Err(error) => eprintln!("wt: update failed: {error:#}"),
    }
    Ok(())
}

pub fn require_stable_launcher(paths: &InstallPaths) -> Result<()> {
    if !paths.launcher().is_file() {
        bail!(
            "native wt installation is not active at {}; install a release before updating or rolling back",
            paths.root().display()
        )
    }
    Ok(())
}

async fn update_latest(
    paths: InstallPaths,
    repository: ReleaseRepository,
    target: String,
    channel: Channel,
    release_tag: Option<String>,
    cancel: &CancellationToken,
) -> Result<UpdateOutcome> {
    // The startup path shares the same check stamp and release source as the
    // explicit command but only downloads after the user accepts.
    update_once(
        paths,
        repository,
        target,
        Some(channel),
        release_tag,
        true,
        cancel,
    )
    .await
}

pub fn attempt_token() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_update::{PendingBoot, StateHistoryEntry, VersionId};

    fn version(tag: &str, id: &str) -> VersionId {
        VersionId::new(tag, id, "x86_64-unknown-linux-gnu").unwrap()
    }

    #[test]
    fn default_rollback_uses_pending_fallback_then_previous_confirmed_build() {
        let first = version("v1", "1111111");
        let second = version("v2", "2222222");
        let third = version("v3", "3333333");
        let mut state = InstallState::new(Channel::Stable);
        state.current = Some(third.clone());
        state.last_good = Some(second.clone());
        state.pending_boot = Some(PendingBoot {
            candidate: third.clone(),
            fallback: Some(second.clone()),
            attempt_token: "attempt".into(),
            started_unix: 1,
        });
        state.history = vec![
            StateHistoryEntry {
                at_unix: 1,
                operation: "boot-confirmed".into(),
                from: None,
                to: Some(first.clone()),
                detail: None,
            },
            StateHistoryEntry {
                at_unix: 2,
                operation: "boot-confirmed".into(),
                from: Some(first.clone()),
                to: Some(second.clone()),
                detail: None,
            },
        ];
        assert_eq!(rollback_candidate(&state, None).unwrap(), second);

        state.current = Some(second.clone());
        state.last_good = Some(second.clone());
        state.pending_boot = None;
        assert_eq!(rollback_candidate(&state, None).unwrap(), first);
    }

    #[test]
    fn explicit_rollback_only_accepts_a_known_prior_identity() {
        let current = version("v2", "2222222");
        let prior = version("v1", "1111111");
        let mut state = InstallState::new(Channel::Stable);
        state.current = Some(current);
        state.last_good = Some(prior.clone());
        assert_eq!(
            rollback_candidate(&state, Some(prior.build_id())).unwrap(),
            prior
        );
        assert!(rollback_candidate(&state, Some("../external")).is_err());
    }
}
