//! `wt automations`: the automation queue as the running engine last saw it.

use anyhow::Result;
use clap::Args;
use serde::Deserialize;

use wt_app_core::context::AppContext;
use wt_sources::automation_source::queue_report_path;

/// The engine rewrites its report at least this often while it runs.
const STALE_AFTER_MS: i64 = 3 * 60 * 1000;

#[derive(Debug, Clone, Args, Default)]
pub struct AutomationsArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Deserialize)]
struct Report {
    pid: u32,
    updated_at_ms: i64,
    paused: bool,
    error: Option<String>,
    queued: Vec<Queued>,
}

#[derive(Debug, Deserialize)]
struct Queued {
    rule: String,
    target: String,
    detail: String,
    queued_at_ms: i64,
    waiting_for: String,
}

pub async fn run(ctx: &AppContext, args: &AutomationsArgs) -> Result<i32> {
    let path = queue_report_path(&ctx.config);
    if ctx.config.automations.is_empty() {
        println!("no automations are configured");
        return Ok(0);
    }
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "automations have not run for this repository yet: they are evaluated by the wt TUI's host while it is open"
            );
            return Ok(1);
        }
        Err(error) => return Err(error.into()),
    };
    if args.json {
        println!("{raw}");
        return Ok(0);
    }
    let report: Report = serde_json::from_str(&raw)?;
    let now = now_ms();
    let alive = wt_ops::events::pid_alive(report.pid);
    let age = now.saturating_sub(report.updated_at_ms);
    if !alive || age > STALE_AFTER_MS {
        println!(
            "automations are not running: the last report (pid {}) is {} old{}. Open the wt TUI for this repository to resume them",
            report.pid,
            ago(age),
            if alive {
                ""
            } else {
                " and that process has exited"
            }
        );
    } else if report.paused {
        println!(
            "automations are paused for this repository (pid {})",
            report.pid
        );
    } else {
        println!(
            "automations are running (pid {}, checked {} ago)",
            report.pid,
            ago(age)
        );
    }
    if let Some(error) = &report.error {
        println!("error: {error}");
    }
    if report.queued.is_empty() {
        println!("nothing queued");
        return Ok(0);
    }
    println!("{} queued:", report.queued.len());
    for fire in &report.queued {
        println!(
            "  {} for {}: waiting for {} (queued {} ago)",
            fire.rule,
            fire.target,
            fire.waiting_for,
            ago(now.saturating_sub(fire.queued_at_ms))
        );
        if !fire.detail.is_empty() {
            println!("    {}", fire.detail);
        }
    }
    Ok(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

fn ago(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}
