use std::{io::IsTerminal, path::Path, process::Stdio};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use tokio_util::sync::CancellationToken;

use wt_app_core::{codex_reauth, context::AppContext};

#[derive(Debug, Clone, Args)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum AuthCommand {
    /// Coordinate independent Codex logins on managed workers.
    Codex {
        #[command(subcommand)]
        command: CodexAuthCommand,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum CodexAuthCommand {
    /// Record a successful local account change for every managed worker using this CODEX_HOME.
    Changed,
    /// Show the durable controller account-switch generation without contacting hosts.
    Status,
    /// Sign in independently on this configuration's managed workers.
    Relogin {
        /// Resume pending sign-ins without requesting another account switch.
        #[arg(long)]
        pending: bool,
        /// Only visit this configured SSH host or label.
        #[arg(long)]
        host: Option<String>,
    },
}

/// Changed/status are config-free: codex-relogin works from any directory.
pub async fn run_global(
    home: &Path,
    args: &AuthArgs,
    cancel: &CancellationToken,
) -> Result<Option<i32>> {
    let AuthCommand::Codex { command } = &args.command;
    let codex_home = codex_reauth::controller_home(home);
    match command {
        CodexAuthCommand::Changed => {
            let generation = codex_reauth::request_generation(&codex_home, cancel).await?;
            println!(
                "Managed Codex account switch recorded ({generation}). Offline hosts remain pending; wt requests their own sign-in when they reconnect."
            );
            Ok(Some(0))
        }
        CodexAuthCommand::Status => {
            match codex_reauth::controller_generation(&codex_home)? {
                Some(generation) => println!(
                    "Controller account-switch generation: {generation}. Each managed host acknowledges this after its own login and daemon refresh."
                ),
                None => println!("No managed Codex account switch has been requested."),
            }
            Ok(Some(0))
        }
        CodexAuthCommand::Relogin { .. } => Ok(None),
    }
}

pub async fn run(context: &AppContext, args: &AuthArgs) -> Result<i32> {
    if let Some(code) = run_global(&context.home, args, &context.cancellation).await? {
        return Ok(code);
    }
    let AuthCommand::Codex {
        command: CodexAuthCommand::Relogin { pending, host },
    } = &args.command
    else {
        unreachable!()
    };
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "managed Codex sign-in requires an interactive terminal; use `wt auth codex changed` to record a pending account switch"
        );
    }
    let hosts: Vec<_> = context
        .config
        .remotes
        .iter()
        .filter(|remote| {
            remote.managed
                && host
                    .as_ref()
                    .is_none_or(|selector| remote.host == *selector || remote.label == *selector)
        })
        .collect();
    if host.is_some() && hosts.len() != 1 {
        bail!("select exactly one configured managed host by SSH host or unique label");
    }
    if !pending {
        codex_reauth::request_generation(
            &codex_reauth::controller_home(&context.home),
            &context.cancellation,
        )
        .await?;
    }
    if hosts.is_empty() {
        println!(
            "No managed remotes in this configuration. The account-switch request remains available to other configurations using this CODEX_HOME."
        );
    }
    let mut failed = false;
    for remote in hosts {
        if context.cancellation.is_cancelled() {
            return Ok(1);
        }
        println!("Checking Codex sign-in on {}…", remote.label);
        match login_host(context, remote, *pending).await {
            Ok(()) => {}
            Err(error) => {
                failed = true;
                eprintln!(
                    "{}: {error:#}. Sign-in remains pending; retry `wt auth codex relogin --pending` or open Codex there from wt.",
                    remote.label
                );
            }
        }
    }
    Ok(i32::from(failed))
}

async fn login_host(
    context: &AppContext,
    remote: &wt_config::RemoteConfig,
    pending_only: bool,
) -> Result<()> {
    let client = crate::remote::stage_remote_runtime(context, remote).await?;
    let report = crate::managed::provision(context, &client).await?;
    require_codex_available(&report)?;
    if pending_only
        && !report
            .sign_ins()
            .contains(&wt_remote::managed::SignIn::Codex)
    {
        println!("{}: Codex sign-in is current.", remote.label);
        return Ok(());
    }
    let prepared = client.interactive_managed(
        &["login".into(), "codex".into()],
        Some(wt_remote::managed::CODEX_LOGIN_PORT),
    );
    let mut child = tokio::process::Command::new(prepared.program)
        .args(prepared.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("start managed Codex sign-in")?;
    let status = tokio::select! {
        _ = context.cancellation.cancelled() => {
            child.kill().await.context("stop managed sign-in transport")?;
            bail!("sign-in cancelled");
        }
        status = child.wait() => status.context("wait for managed Codex sign-in")?,
    };
    if !status.success() {
        bail!("managed sign-in exited with {status}");
    }
    let report = crate::managed::provision(context, &client).await?;
    require_codex_available(&report)?;
    if report
        .sign_ins()
        .contains(&wt_remote::managed::SignIn::Codex)
    {
        bail!("Codex sign-in is still required");
    }
    println!(
        "{}: Codex login and daemon refresh completed.",
        remote.label
    );
    Ok(())
}

fn require_codex_available(report: &wt_remote::managed::ManagedReport) -> Result<()> {
    if let Some(check) = report.checks.iter().find(|check| {
        matches!(check.id.as_str(), "codex" | "codex-login")
            && check.state == wt_remote::managed::CheckState::Failed
    }) {
        bail!("{}", check.detail);
    }
    if !report.checks.iter().any(|check| check.id == "codex-login") {
        bail!("managed host has not confirmed its Codex login state");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        auth: AuthArgs,
    }

    #[test]
    fn pending_retry_does_not_request_another_switch() {
        let cli =
            Cli::try_parse_from(["auth", "codex", "relogin", "--pending", "--host", "MaxBook"])
                .unwrap();
        assert!(matches!(
            cli.auth.command,
            AuthCommand::Codex {
                command: CodexAuthCommand::Relogin {
                    pending: true,
                    host: Some(_)
                }
            }
        ));
    }

    #[test]
    fn missing_or_failed_codex_is_not_reported_as_current() {
        use wt_remote::managed::{CheckState, ManagedCheck, ManagedReport};
        assert!(require_codex_available(&ManagedReport::default()).is_err());
        assert!(
            require_codex_available(&ManagedReport {
                checks: vec![ManagedCheck::new(
                    "codex",
                    CheckState::Failed,
                    "not installed",
                    None
                )],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            require_codex_available(&ManagedReport {
                checks: vec![ManagedCheck::ok("codex-login", "signed in")],
                ..Default::default()
            })
            .is_ok()
        );
    }
}
