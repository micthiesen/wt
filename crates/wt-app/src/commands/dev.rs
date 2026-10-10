use std::time::Duration;

use crate::{commands::resolve::resolve_worktree, context::AppContext, dev};
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::json;
use wt_dev::{DevServerStatus, DevStartOptions, DevWorktree, ReadyOutcome, WaitOutcome};

const SLOT_FULL: i32 = 75;

#[derive(Debug, Clone, Args)]
pub struct DevArgs {
    #[command(subcommand)]
    pub command: DevCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum DevCommand {
    /// Start (or join) this worktree's supervised dev server.
    Start {
        slug: Option<String>,
        #[arg(long)]
        wait: bool,
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
        #[arg(long)]
        rebuild: bool,
    },
    /// Stop, reset project state, then start the server.
    Reset {
        slug: Option<String>,
        #[arg(long)]
        wait: bool,
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
    },
    /// Stop this worktree's dev server.
    Stop { slug: Option<String> },
    /// Show server state, optionally for the fleet.
    Status {
        slug: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inspect or reprioritize the fleet wait queue.
    Queue {
        slug: Option<String>,
        #[arg(long, conflicts_with = "normal")]
        first: bool,
        #[arg(long)]
        normal: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show recent supervisor output.
    Logs {
        slug: Option<String>,
        #[arg(long, default_value_t = 200)]
        lines: u32,
    },
}

/// Entry point used by the hidden tmux-owned process. The supervisor is
/// resolved back to the current inventory so argv cannot name an arbitrary
/// directory for the child command.
#[derive(Debug, Clone, Args)]
pub struct DevSupervisorArgs {
    #[arg(long)]
    pub slug: String,
    #[arg(long)]
    pub path: String,
    #[arg(long)]
    pub port: u16,
}

pub async fn run_supervisor_command(context: &AppContext, args: &DevSupervisorArgs) -> Result<i32> {
    let record = resolve_worktree(context, Some(&args.slug)).await?;
    if record.target.path != args.path {
        bail!(
            "dev supervisor path no longer matches worktree {}",
            args.slug
        );
    }
    let service = dev::service(context)?;
    service
        .run_supervisor(
            &DevWorktree::from(&record),
            args.port,
            context.cancellation.clone(),
        )
        .await?;
    Ok(0)
}

pub async fn run(context: &AppContext, args: &DevArgs) -> Result<i32> {
    match &args.command {
        DevCommand::Start {
            slug,
            wait,
            timeout,
            rebuild,
        } => run_start(context, slug.as_deref(), *wait, *timeout, *rebuild).await,
        DevCommand::Reset {
            slug,
            wait,
            timeout,
        } => run_start(context, slug.as_deref(), *wait, *timeout, true).await,
        DevCommand::Stop { slug } => {
            let record = resolve_worktree(context, slug.as_deref()).await?;
            dev::service(context)?
                .stop(&DevWorktree::from(&record), &context.cancellation)
                .await?;
            println!("stopped dev server for {}", record.target.slug());
            Ok(0)
        }
        DevCommand::Status {
            slug,
            all,
            json: as_json,
        } => run_status(context, slug.as_deref(), *all, *as_json).await,
        DevCommand::Queue {
            slug,
            first,
            normal,
            json: as_json,
        } => run_queue(context, slug.as_deref(), *first, *normal, *as_json).await,
        DevCommand::Logs { slug, lines } => {
            let record = resolve_worktree(context, slug.as_deref()).await?;
            match dev::service(context)?
                .logs(
                    record.target.slug(),
                    (*lines).clamp(1, 5000),
                    &context.cancellation,
                )
                .await?
            {
                Some(logs) => print!("{logs}"),
                None => println!("no dev-server logs for {}", record.target.slug()),
            }
            Ok(0)
        }
    }
}

async fn run_start(
    context: &AppContext,
    slug: Option<&str>,
    wait: bool,
    timeout_seconds: Option<u64>,
    rebuild: bool,
) -> Result<i32> {
    let record = resolve_worktree(context, slug).await?;
    let worktree = DevWorktree::from(&record);
    let service = dev::service(context)?;
    let timeout = Duration::from_secs(timeout_seconds.unwrap_or(1800).max(1));
    let deadline = tokio::time::Instant::now() + timeout;
    let started_sha = if rebuild {
        None
    } else {
        service
            .status(&worktree.slug, &worktree.path, &context.cancellation)
            .await?
            .rebased_since
    };
    let outcome = loop {
        if wait {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let outcome = service
                .wait_for_slot(&worktree.slug, remaining, &context.cancellation, |rank| {
                    eprintln!("waiting for dev-server slot, queue position {}", rank + 1)
                })
                .await?;
            if outcome == WaitOutcome::TimedOut {
                eprintln!("no dev-server slot after {}s", timeout.as_secs());
                return Ok(SLOT_FULL);
            }
        }
        let result = if rebuild {
            service.reset(&worktree, &context.cancellation).await
        } else {
            service
                .start(&worktree, DevStartOptions::default(), &context.cancellation)
                .await
        };
        match result {
            Ok(outcome) => {
                if wait {
                    service.clear_waiter(&worktree.slug).await?;
                }
                break outcome;
            }
            Err(wt_dev::DevServerError::SlotFull { .. }) if wait => continue,
            Err(wt_dev::DevServerError::SlotFull { .. }) => return Ok(SLOT_FULL),
            Err(error) => {
                if wait {
                    service.clear_waiter(&worktree.slug).await?;
                }
                return Err(error.into());
            }
        }
    };
    let (port, url, adopted) = match outcome {
        wt_dev::DevStartOutcome::Started { port, url } => (port, url, false),
        wt_dev::DevStartOutcome::Adopted { port, url } => (port, url, true),
    };
    if !wait {
        println!(
            "{} dev server for {}",
            if adopted { "joining" } else { "launching" },
            worktree.slug
        );
        println!("port: {port}\nurl:  {url}");
        println!(
            "still starting; use `wt dev start {} --wait` to block until ready",
            worktree.slug
        );
        return Ok(0);
    }
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    match service
        .wait_ready(&worktree, remaining, &context.cancellation)
        .await?
    {
        ReadyOutcome::Ready { health } => {
            if started_sha == Some(true) {
                eprintln!(
                    "warning: the environment last started before a rebase; `wt dev reset {}` rebuilds it",
                    worktree.slug
                );
            }
            println!(
                "dev server ready for {}\nport: {port}\nurl:  {url}",
                worktree.slug
            );
            if let Some(health) = health {
                println!("health: {}", health.message);
            }
            Ok(0)
        }
        ReadyOutcome::Crashed => {
            eprintln!(
                "dev server crashed; inspect `wt dev logs {}`",
                worktree.slug
            );
            Ok(1)
        }
        ReadyOutcome::Timeout => {
            eprintln!(
                "dev server not ready after {}s; inspect `wt dev logs {}`",
                timeout.as_secs(),
                worktree.slug
            );
            Ok(1)
        }
        ReadyOutcome::Unhealthy(health) => {
            eprintln!("dev server health check failed: {}", health.message);
            Ok(1)
        }
    }
}

async fn run_status(
    context: &AppContext,
    slug: Option<&str>,
    all: bool,
    as_json: bool,
) -> Result<i32> {
    let service = dev::service(context)?;
    if all {
        let records = context
            .repository
            .inventory(&context.cancellation)
            .await?
            .into_iter()
            .filter(|record| !record.is_main)
            .collect::<Vec<_>>();
        let worktrees = records.iter().map(DevWorktree::from).collect::<Vec<_>>();
        let snapshot = service
            .status_all(&worktrees, &context.cancellation)
            .await?;
        let mut rows = Vec::new();
        for (record, row) in records.iter().zip(snapshot.worktrees.iter()) {
            let health = if row.status.as_ref().is_some_and(|status| status.running) {
                service
                    .health(&DevWorktree::from(record), &context.cancellation)
                    .await?
            } else {
                None
            };
            rows.push(json!({"slug":record.target.slug(),"branch":record.target.branch,"status":row.status,"error":row.error,"health":health}));
        }
        let value = json!({"worktrees":rows,"slots":snapshot.slots,"queue":snapshot.slots.waiters});
        if as_json {
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            for row in &rows {
                print_status_row(row);
            }
            if let Some(limit) = snapshot.slots.limit {
                println!(
                    "slots: {}/{} used",
                    limit.saturating_sub(snapshot.slots.free.unwrap_or(0)),
                    limit
                );
            }
            print_queue(&snapshot.slots.waiters);
        }
    } else {
        let record = resolve_worktree(context, slug).await?;
        let status = service
            .status(
                record.target.slug(),
                std::path::Path::new(&record.target.path),
                &context.cancellation,
            )
            .await?;
        let health = if status.running {
            service
                .health(&DevWorktree::from(&record), &context.cancellation)
                .await?
        } else {
            None
        };
        if as_json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"slug":record.target.slug(),"branch":record.target.branch,"status":status,"health":health})
                )?
            );
        } else {
            print_status(record.target.slug(), &status);
            if let Some(health) = health {
                println!("health: {}", health.message);
            }
        }
    }
    Ok(0)
}

async fn run_queue(
    context: &AppContext,
    slug: Option<&str>,
    first: bool,
    normal: bool,
    as_json: bool,
) -> Result<i32> {
    let service = dev::service(context)?;
    if first || normal {
        let slug = if let Some(slug) = slug {
            slug.to_owned()
        } else {
            resolve_worktree(context, None)
                .await?
                .target
                .slug()
                .to_owned()
        };
        let agent = std::env::var("WT_AGENT")
            .ok()
            .is_some_and(|agent| agent == slug);
        if first && agent {
            bail!("a worktree cannot promote itself; ask the manager to prioritize it");
        }
        let priority = if first { 1 } else { 0 };
        let waiter = service
            .set_waiter_priority(&slug, priority, &context.cancellation)
            .await?
            .with_context(|| format!("{slug} is not waiting for a dev-server slot"))?;
        if !as_json {
            println!(
                "{} is {} in the dev-server queue",
                slug,
                if waiter.priority > 0 {
                    "prioritized"
                } else {
                    "back to normal"
                }
            );
        }
    }
    let report = service.queue_report(&context.cancellation).await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_queue(&report.waiters);
    }
    Ok(0)
}

fn print_queue(waiters: &[wt_dev::DevWaiter]) {
    if waiters.is_empty() {
        println!("dev-server queue: empty");
        return;
    }
    println!("dev-server queue:");
    for (rank, waiter) in waiters.iter().enumerate() {
        println!(
            "  #{} {}{}",
            rank + 1,
            waiter.slug,
            if waiter.priority > 0 { " (first)" } else { "" }
        );
    }
}

fn print_status_row(row: &serde_json::Value) {
    let slug = row.get("slug").and_then(|v| v.as_str()).unwrap_or("?");
    if let Ok(status) =
        serde_json::from_value::<DevServerStatus>(row.get("status").cloned().unwrap_or_default())
    {
        print_status(slug, &status);
    }
}

fn print_status(slug: &str, status: &DevServerStatus) {
    let state = if status.running {
        "running"
    } else if status.starting {
        "starting"
    } else if status.crashed {
        "crashed"
    } else {
        "stopped"
    };
    println!(
        "{slug}: {state}{}",
        status
            .port
            .map(|port| format!(" on :{port}"))
            .unwrap_or_default()
    );
    if let Some(url) = &status.url {
        println!("  {url}");
    }
    if status.rebased_since == Some(true) {
        println!("  warning: started before current history; consider `wt dev reset {slug}`");
    }
}
