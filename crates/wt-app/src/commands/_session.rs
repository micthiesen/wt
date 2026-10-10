use std::process::Stdio;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use tokio::process::Command;
use wt_config::{HarnessId, InstanceRole};
use wt_tui::SessionTarget;

use crate::{context::AppContext, harness::ui_session_with_harness};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum RemoteTarget {
    Shell,
    Diff,
    Harness,
}

pub async fn run(context: &AppContext, args: &[String]) -> Result<i32> {
    if !(2..=3).contains(&args.len()) {
        eprintln!("usage: wt _session <slug> <shell|diff|harness> [harness]");
        return Ok(2);
    }
    if context.config.instance.role != InstanceRole::Worker {
        bail!("remote session requires [instance] role = \"worker\" on this host");
    }
    let target = RemoteTarget::from_str(&args[1], false)
        .map_err(|_| anyhow::anyhow!("unknown remote session target: {}", args[1]))?;
    let harness = args
        .get(2)
        .map(|value| {
            HarnessId::ALL
                .into_iter()
                .find(|id| id.as_str() == value)
                .ok_or_else(|| anyhow::anyhow!("unknown harness: {value}"))
        })
        .transpose()?;
    let target = match target {
        RemoteTarget::Shell => SessionTarget::Shell,
        RemoteTarget::Diff => SessionTarget::Diff,
        RemoteTarget::Harness => SessionTarget::Harness,
    };
    let inventory = context.repository.inventory(&context.cancellation).await?;
    let record = inventory
        .iter()
        .find(|record| !record.is_main && record.target.slug() == args[0])
        .with_context(|| format!("remote worktree not found: {}", args[0]))?;
    let key = wt_core::worktree_target_key(&record.target);
    let prepared = ui_session_with_harness(context, Some(key), target, harness).await?;
    let mut child = Command::new(prepared.program)
        .args(prepared.args)
        .current_dir(prepared.cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("attach remote worker tmux session")?;
    let status = tokio::select! {
        _ = context.cancellation.cancelled() => {
            child.kill().await.context("terminate tmux client")?;
            return Ok(0);
        }
        status = child.wait() => status.context("wait for tmux client")?,
    };
    if status.success() {
        Ok(0)
    } else {
        Ok(status.code().unwrap_or(1))
    }
}
