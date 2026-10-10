use anyhow::Result;
use clap::Args;
use wt_config::LoadOptions;
use wt_update::Channel;

use super::update::UpdateChannel;

#[derive(Clone, Debug, Args)]
pub struct InstallArgs {
    /// Select the latest stable or preview release.
    #[arg(long, value_enum)]
    pub channel: Option<UpdateChannel>,
    /// Install an exact immutable GitHub release tag, including test tags.
    #[arg(long)]
    pub release: Option<String>,
    /// Ensure `~/.local/bin/wt` points to this install's stable launcher.
    #[arg(long)]
    pub path: bool,
    /// Verify that the downloaded release matches the bootstrap binary.
    #[arg(long, hide = true)]
    pub expected_build_id: Option<String>,
}

pub async fn run(
    options: &LoadOptions,
    args: &InstallArgs,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<i32> {
    let result = crate::install::install_once(
        options,
        args.channel.map(Channel::from),
        args.release.clone(),
        args.expected_build_id.clone(),
        args.path,
        cancel,
    )
    .await?;
    match result {
        crate::install::InstallResult::Installed { release, build_id } => {
            println!("installed wt {release} ({build_id})")
        }
        crate::install::InstallResult::AlreadyCurrent { release, build_id } => {
            println!("wt {release} ({build_id}) is already installed")
        }
    }
    Ok(0)
}
