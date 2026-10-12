use std::process::Stdio;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use tokio::process::Command;
use wt_config::{HarnessId, InstanceRole};
use wt_tui::{SessionSelection, SessionTarget};

use crate::{
    context::AppContext,
    harness::{prepare_session, ui_session_with_harness},
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum RemoteTarget {
    Shell,
    Diff,
    Harness,
    Manager,
    Main,
    Wt,
    Dotfiles,
}

pub async fn run(context: &AppContext, args: &[String]) -> Result<i32> {
    let result = run_inner(context, args).await;
    if let Err(error) = &result {
        // SSH allocates a remote PTY, merging stderr into stdout. Retain the
        // cause in the worker activity feed before the controller redraws.
        tracing::error!(error = %format!("{error:#}"), "session attachment failed");
    }
    result
}

async fn run_inner(context: &AppContext, args: &[String]) -> Result<i32> {
    if context.config.instance.role != InstanceRole::Worker {
        bail!("remote session requires [instance] role = \"worker\" on this host");
    }
    if args.len() == 1 {
        let selection: SessionSelection =
            serde_json::from_str(&args[0]).context("parse structured session selection")?;
        let prepared = prepare_session(context, &selection).await?;
        return attach(context, prepared).await;
    }
    if !(2..=3).contains(&args.len()) {
        eprintln!(
            "usage: wt _session <SessionSelection JSON> | <slug> <shell|diff|harness|manager|main|wt|dotfiles> [harness]"
        );
        return Ok(2);
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
        RemoteTarget::Manager => SessionTarget::Manager,
        RemoteTarget::Main => SessionTarget::Main,
        RemoteTarget::Wt => SessionTarget::WtSource,
        RemoteTarget::Dotfiles => SessionTarget::Dotfiles,
    };
    let key = if matches!(
        target,
        SessionTarget::Harness | SessionTarget::Shell | SessionTarget::Diff
    ) {
        let inventory = context.repository.inventory(&context.cancellation).await?;
        let record = inventory
            .iter()
            .find(|record| !record.is_main && record.target.slug() == args[0])
            .with_context(|| format!("remote worktree not found: {}", args[0]))?;
        Some(wt_core::worktree_target_key(&record.target))
    } else {
        None
    };
    let prepared = ui_session_with_harness(context, key, target, harness).await?;
    attach(context, prepared).await
}

async fn attach(context: &AppContext, prepared: crate::harness::PreparedSession) -> Result<i32> {
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
    if status.success() || matches!(status.code(), Some(110..=112)) {
        Ok(0)
    } else {
        tracing::error!(error = %format!("tmux client exited with {status}"), "session attachment failed");
        Ok(status.code().unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    #[derive(Clone)]
    struct LogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn preparation_failure_is_retained_in_the_worker_log() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer({
                let output = output.clone();
                move || LogWriter(output.clone())
            })
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let error = super::run(&fixture.ctx, &[]).await.unwrap_err();
        drop(guard);
        let log = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(log.contains("session attachment failed"), "{log}");
        assert!(log.contains(&error.to_string()), "{log}");
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn remote_attach_logs_failures_but_accepts_legacy_detach_status() {
        use tracing::instrument::WithSubscriber;

        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer({
                let output = output.clone();
                move || LogWriter(output.clone())
            })
            .finish();
        async {
            for (exit, expected) in [(1, 1), (110, 0), (111, 0), (112, 0)] {
                let prepared = crate::harness::PreparedSession {
                    program: "sh".into(),
                    args: vec!["-c".into(), format!("exit {exit}").into()],
                    cwd: std::env::temp_dir(),
                };
                assert_eq!(
                    super::attach(&fixture.ctx, prepared).await.unwrap(),
                    expected
                );
            }
        }
        .with_subscriber(subscriber)
        .await;
        let log = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(log.matches("session attachment failed").count(), 1, "{log}");
        assert!(log.contains("exit status: 1"), "{log}");
        fixture.close().await.unwrap();
    }
}
