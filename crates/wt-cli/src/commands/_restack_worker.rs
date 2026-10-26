use anyhow::Result;
use clap::Args;

use wt_app_core::context::AppContext;
use wt_ops::restack_action;

#[derive(Debug, Clone, Args)]
pub struct RestackWorkerArgs {
    #[arg(long)]
    pub branch: String,
}

pub async fn run(context: &AppContext, args: &RestackWorkerArgs) -> Result<i32> {
    restack_action::worker(context, &args.branch).await
}
