use anyhow::{Result, bail};
use wt_config::InstanceRole;

use crate::context::AppContext;

pub async fn run(context: &AppContext, args: &[String]) -> Result<i32> {
    if args.len() != 1 {
        eprintln!("usage: wt _remote <encoded-argv>");
        return Ok(2);
    }
    let decoded = match wt_remote::decode_remote_args(&args[0]) {
        Ok(decoded) => decoded,
        Err(error) => {
            eprintln!("{error}");
            return Ok(2);
        }
    };
    if decoded.is_empty() {
        bail!("remote argv must contain a command");
    }
    if decoded[0] != "_hello" && context.config.instance.role != InstanceRole::Worker {
        eprintln!("remote execution requires [instance] role = \"worker\" on this host");
        return Ok(1);
    }
    Box::pin(crate::dispatch_worker_args(context, &decoded)).await
}
