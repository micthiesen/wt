//! Pass the selected policy to the job boundary, without reloading config.

use std::ffi::OsString;

use anyhow::{Context, Result};
use clap::Args;
use wt_platform::resources::Limits;

use crate::context::AppContext;

fn limits(settings: &wt_config::ResourceLimits) -> Limits {
    Limits {
        cpu_quota_percent: settings.cpu_quota_percent,
        memory_high: settings
            .memory_high
            .as_deref()
            .and_then(wt_config::memory_bytes),
        memory_max: settings
            .memory_max
            .as_deref()
            .and_then(wt_config::memory_bytes),
        memory_swap_max: settings
            .memory_swap_max
            .as_deref()
            .and_then(wt_config::memory_bytes),
    }
}

pub fn prefix(context: &AppContext) -> Vec<String> {
    let settings = &context.config.resources.linux;
    if !cfg!(target_os = "linux") || !settings.active() {
        return Vec::new();
    }
    // The same immutable executable runs the helper and the selected job.
    let mut arguments = vec![
        std::env::current_exe()
            .expect("running executable has a path")
            .to_string_lossy()
            .into_owned(),
        "_workload".into(),
        "--group".into(),
        settings.group.clone(),
    ];
    arguments.extend(limits(&settings.shared).arguments("shared"));
    arguments.extend(limits(&settings.job).arguments("job"));
    arguments.push("--".into());
    arguments
}

#[derive(Debug, Args)]
pub struct WorkloadArgs {
    #[arg(long)]
    group: String,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    shared_cpu: Option<u32>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    job_cpu: Option<u32>,
    #[arg(long)]
    shared_memory_high: Option<u64>,
    #[arg(long)]
    shared_memory_max: Option<u64>,
    #[arg(long)]
    shared_memory_swap_max: Option<u64>,
    #[arg(long)]
    job_memory_high: Option<u64>,
    #[arg(long)]
    job_memory_max: Option<u64>,
    #[arg(long)]
    job_memory_swap_max: Option<u64>,
    #[arg(last = true, required = true, num_args = 1..)]
    command: Vec<OsString>,
}

pub async fn run(args: &WorkloadArgs, cancel: &tokio_util::sync::CancellationToken) -> Result<i32> {
    let shared = Limits {
        cpu_quota_percent: args.shared_cpu,
        memory_high: args.shared_memory_high,
        memory_max: args.shared_memory_max,
        memory_swap_max: args.shared_memory_swap_max,
    };
    let job = Limits {
        cpu_quota_percent: args.job_cpu,
        memory_high: args.job_memory_high,
        memory_max: args.job_memory_max,
        memory_swap_max: args.job_memory_swap_max,
    };
    wt_platform::resources::enter(
        &args.group,
        &shared,
        &job,
        &wt_platform::process::ProcessRunner::default(),
        cancel,
    )
    .await
    .context("apply configured Linux workload limits; the job was not started")?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(&args.command[0])
            .args(&args.command[1..])
            .exec();
        Err(error).context("execute limited workload")
    }
    #[cfg(not(unix))]
    anyhow::bail!("Linux workload limits are not available on this host")
}
