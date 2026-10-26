use anyhow::{Result, bail};
use clap::Args;
use wt_config::LoadOptions;

#[derive(Clone, Debug, Args)]
pub struct RollbackArgs {
    /// A prior installed release version or build SHA.
    pub reference: Option<String>,
}

pub async fn run(options: &LoadOptions, args: &RollbackArgs) -> Result<i32> {
    if options
        .env
        .get("WT_UPDATE")
        .is_some_and(|value| value == "off")
    {
        bail!("native update system is disabled for this run by WT_UPDATE=off")
    }
    let paths = crate::updates::install_paths(options)?;
    crate::updates::require_stable_launcher(&paths)?;
    let candidate = crate::updates::rollback_once(paths, args.reference.clone()).await?;
    println!(
        "activated rollback candidate {} ({}) for the next start",
        candidate.release_version(),
        candidate.build_id()
    );
    Ok(0)
}
