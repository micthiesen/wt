use anyhow::Result;
use wt_config::InstanceRole;

use crate::{context::AppContext, remote};

pub async fn run(context: &AppContext, args: &[String]) -> Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: wt _snapshot");
        return Ok(2);
    }
    if context.config.instance.role != InstanceRole::Worker {
        eprintln!("worker snapshot requires [instance] role = \"worker\" on this host");
        return Ok(1);
    }
    let snapshot = remote::collect_worker_snapshot(context).await?;
    println!("{}", serde_json::to_string(&snapshot)?);
    Ok(0)
}
