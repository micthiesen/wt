use anyhow::Result;
use wt_config::Config;

use crate::remote;

pub fn run(config: &Config, args: &[String]) -> Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: wt _hello");
        return Ok(2);
    }
    println!(
        "{}",
        serde_json::to_string(&remote::worker_info(config.instance.role))?
    );
    Ok(0)
}
