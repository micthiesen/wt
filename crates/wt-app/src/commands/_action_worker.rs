use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::context::AppContext;

#[derive(Debug, Clone, Args)]
pub struct ActionWorkerArgs {
    #[arg(long)]
    pub job: PathBuf,
}

pub async fn run(context: &AppContext, args: &ActionWorkerArgs) -> Result<i32> {
    let done = crate::actions::service(context)?
        .run_worker(&args.job, &context.cancellation)
        .await?;
    Ok(match done.status {
        wt_actions::ActionRunStatus::Succeeded => 0,
        wt_actions::ActionRunStatus::Killed => 130,
        _ => done.exit_code.filter(|code| *code > 0).unwrap_or(1),
    })
}
