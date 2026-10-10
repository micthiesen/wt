use std::io::IsTerminal;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use clap::Args;
use tokio::process::Command;
use wt_remote::RemoteClient;

use crate::{context::AppContext, remote};

#[derive(Debug, Clone, Args, Default)]
pub struct RemoteArgs {
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

pub async fn run(context: &AppContext, args: &RemoteArgs) -> Result<i32> {
    let endpoint = context
        .config
        .remote
        .as_ref()
        .context("no [remote] host is configured")?;
    let client = RemoteClient::new(context.processes.clone(), endpoint.clone());
    if args.args.is_empty() {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            bail!("wt remote without arguments requires an interactive terminal");
        }
        let prepared = client.interactive_tui();
        let mut child = Command::new(prepared.program)
            .args(prepared.args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("start remote wt interface")?;
        let status = tokio::select! {
            _ = context.cancellation.cancelled() => {
                child.kill().await.context("terminate remote wt interface")?;
                return Ok(0);
            }
            status = child.wait() => status.context("wait for remote wt interface")?,
        };
        return Ok(status.code().unwrap_or(1));
    }
    if args.args.first().is_some_and(|arg| arg == "agent")
        && args.args.get(1).is_some_and(|arg| arg == "start")
    {
        let output = client
            .run_worker(
                &["skills".into(), "sync".into(), "--yes".into()],
                &context.cancellation,
            )
            .await?;
        forward_output(&output);
        if output.exit_code != Some(0) {
            return Ok(output.exit_code.unwrap_or(1));
        }
    }
    remote::remote_admin_command(context, endpoint, &args.args).await
}

fn forward_output(output: &wt_remote::WorkerCommandOutput) {
    if !output.stdout.is_empty() {
        print!("{}", output.stdout);
    }
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
    }
}
