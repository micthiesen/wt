use std::{path::PathBuf, time::Duration};

use anyhow::Result;
use clap::{Args, Subcommand};
use wt_platform::process::CommandSpec;

use crate::{context::AppContext, harness::AppHarness};

#[derive(Debug, Clone, Args)]
pub struct CodexArgs {
    #[command(subcommand)]
    pub command: CodexCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum CodexCommand {
    /// Check the installed CLI queue surface and optional control socket without sending.
    Selftest,
}

pub async fn run(context: &AppContext, args: &CodexArgs) -> Result<i32> {
    match args.command {
        CodexCommand::Selftest => selftest(context).await,
    }
}

async fn selftest(context: &AppContext) -> Result<i32> {
    let mut version_spec = CommandSpec::new("codex")
        .args(["--version"])
        .cwd(&context.cwd);
    version_spec.timeout = Duration::from_secs(5);
    let version = match context
        .processes
        .run(version_spec, &context.cancellation)
        .await
    {
        Ok(version) => version,
        Err(error) => {
            eprintln!("✗ Codex CLI unavailable: {error}");
            return Ok(1);
        }
    };
    if !version.status.success() {
        eprintln!("✗ Codex CLI unavailable: {}", version.stderr_text().trim());
        return Ok(1);
    }
    println!("✓ {}", version.stdout_text().trim());

    let mut queue_spec = CommandSpec::new("codex")
        .args(["queue", "--help"])
        .cwd(&context.cwd);
    queue_spec.timeout = Duration::from_secs(5);
    let queue = context
        .processes
        .run(queue_spec, &context.cancellation)
        .await;
    let queue_ok = queue.as_ref().is_ok_and(|output| output.status.success());
    if queue_ok {
        println!("✓ durable `codex queue` fallback is available");
    } else {
        eprintln!("✗ this Codex CLI has no usable `queue` subcommand");
    }

    let socket = codex_socket_path(&context.home);
    if !socket.exists() {
        println!(
            "○ Codex app-server socket is offline; expected local socket: {}",
            socket.display()
        );
    } else {
        match AppHarness::new(context)
            .codex_app_server_info(context)
            .await
        {
            Ok(Some(info)) => println!("✓ native app-server queue connected ({})", info.user_agent),
            Ok(None) => println!("○ Codex app-server daemon is offline; wt will use `codex queue`"),
            Err(error) => {
                eprintln!("✗ app-server socket is present but unusable: {error}");
                println!(
                    "  update Codex or restart the user-managed daemon; wt does not own its lifecycle"
                );
            }
        }
    }
    Ok(if queue_ok { 0 } else { 1 })
}

fn codex_socket_path(home: &std::path::Path) -> PathBuf {
    home.join(".codex/app-server-control/app-server-control.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_socket_location_is_home_injected() {
        assert_eq!(
            codex_socket_path(std::path::Path::new("/fixture")),
            PathBuf::from("/fixture/.codex/app-server-control/app-server-control.sock")
        );
    }
}
