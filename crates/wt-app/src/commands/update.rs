use anyhow::{Result, bail};
use clap::{Args, ValueEnum};
use wt_config::LoadOptions;
use wt_update::{Channel, StateStore};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum UpdateChannel {
    Stable,
    Preview,
}
impl From<UpdateChannel> for Channel {
    fn from(value: UpdateChannel) -> Self {
        match value {
            UpdateChannel::Stable => Self::Stable,
            UpdateChannel::Preview => Self::Preview,
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct UpdateArgs {
    /// Print the durable update/rollback journal.
    #[arg(value_name = "ACTION")]
    pub action: Option<String>,
    /// Check release metadata without downloading or installing.
    #[arg(long)]
    pub check: bool,
    /// Select and persist the release channel.
    #[arg(long, value_enum, conflicts_with = "release")]
    pub channel: Option<UpdateChannel>,
    /// Explicitly select a GitHub release tag, including test-only releases.
    #[arg(long, conflicts_with = "channel")]
    pub release: Option<String>,
    /// Legacy source-updater option; native releases always require CI metadata.
    #[arg(long)]
    pub head: bool,
}

pub async fn run(
    options: &LoadOptions,
    args: &UpdateArgs,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<i32> {
    if options
        .env
        .get("WT_UPDATE")
        .is_some_and(|value| value == "off")
    {
        bail!("native update system is disabled for this run by WT_UPDATE=off")
    }
    if args.head {
        bail!(
            "`wt update --head` is not supported for native releases; updates only use CI-published release manifests"
        )
    }
    if args.action.as_deref() == Some("log") {
        if args.check || args.channel.is_some() || args.release.is_some() {
            bail!("`wt update log` does not accept --check, --channel, or --release")
        }
        let paths = crate::updates::install_paths(options)?;
        let store = StateStore::new(paths);
        let state = tokio::task::spawn_blocking(move || store.load()).await??;
        println!("channel: {:?}", state.channel);
        println!(
            "current: {}",
            state.current.as_ref().map_or("none".into(), |v| format!(
                "{} ({})",
                v.release_version(),
                v.build_id()
            ))
        );
        println!(
            "last good: {}",
            state.last_good.as_ref().map_or("none".into(), |v| format!(
                "{} ({})",
                v.release_version(),
                v.build_id()
            ))
        );
        println!(
            "declined build: {}",
            state.declined_build_id.as_deref().unwrap_or("none")
        );
        for entry in state.history.iter().rev() {
            println!(
                "{} {}: {} -> {}{}",
                entry.at_unix,
                entry.operation,
                entry.from.as_ref().map_or("none", |v| v.build_id()),
                entry.to.as_ref().map_or("none", |v| v.build_id()),
                entry
                    .detail
                    .as_ref()
                    .map_or(String::new(), |d| format!(" ({d})"))
            );
        }
        return Ok(0);
    }
    if args.action.is_some() {
        bail!("unknown `wt update` action; use `wt update log`")
    }
    let paths = crate::updates::install_paths(options)?;
    if !args.check {
        crate::updates::require_stable_launcher(&paths)?;
    }
    let repository = crate::updates::default_repository()?;
    let result = crate::updates::update_once(
        paths,
        repository,
        env!("WT_TARGET").to_owned(),
        args.channel.map(Into::into),
        args.release.clone(),
        args.check,
        cancel,
    )
    .await?;
    match result {
        crate::updates::UpdateOutcome::Current => println!("wt is up to date"),
        crate::updates::UpdateOutcome::Declined => {
            println!("latest release was declined; run `wt update` to explicitly reapply it")
        }
        crate::updates::UpdateOutcome::Available { release, build_id } => {
            println!("update available: {release} ({build_id})")
        }
        crate::updates::UpdateOutcome::Installed { release, build_id } => {
            println!("installed {release} ({build_id}); it will run next time wt starts")
        }
    }
    Ok(0)
}
