use std::io::IsTerminal;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use clap::Args;
use tokio::process::Command;

use crate::{context::AppContext, remote};

#[derive(Debug, Clone, Args, Default)]
pub struct RemoteArgs {
    /// SSH host or configured label (required when more than one is configured).
    #[arg(long)]
    pub host: Option<String>,
    #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
    pub args: Vec<String>,
}

pub async fn run(context: &AppContext, args: &RemoteArgs) -> Result<i32> {
    let endpoint = select_host(&context.config.remotes, args.host.as_deref())?;
    if args.args.is_empty() {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            bail!("wt remote without arguments requires an interactive terminal");
        }
        let (client, _) = remote::prepare_remote_client(context, endpoint).await?;
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
        let (client, worker) = remote::prepare_remote_client(context, endpoint).await?;
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
        return remote::remote_admin_with_client(context, &client, &worker, &args.args).await;
    }
    remote::remote_admin_command(context, endpoint, &args.args).await
}

fn select_host<'a>(
    hosts: &'a [wt_config::RemoteConfig],
    selector: Option<&str>,
) -> Result<&'a wt_config::RemoteConfig> {
    match selector {
        Some(selector) => {
            if let Some(host) = hosts.iter().find(|host| host.host == selector) {
                return Ok(host);
            }
            let matches = hosts
                .iter()
                .filter(|host| host.label == selector)
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [host] => Ok(host),
                [] => bail!("no remote host matches {selector:?}"),
                _ => bail!("remote label {selector:?} is ambiguous; use its SSH host"),
            }
        }
        None => match hosts {
            [host] => Ok(host),
            [] => bail!("no remote hosts are configured; add [[remotes]] or [remote]"),
            _ => bail!(
                "several remote hosts are configured; choose one with wt remote --host <host>"
            ),
        },
    }
}

fn forward_output(output: &wt_remote::WorkerCommandOutput) {
    if !output.stdout.is_empty() {
        print!("{}", output.stdout);
    }
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
    }
}
