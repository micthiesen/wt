use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::{context::AppContext, events};

#[derive(Debug, Clone, Args)]
pub struct EventsArgs {
    #[command(subcommand)]
    pub command: Option<EventsCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum EventsCommand {
    /// Run the webhook daemon in the foreground.
    Serve,
    /// Show daemon and snapshot status.
    Status,
    /// Install the per-user launchd agent and create a webhook secret if needed.
    Install,
    /// Stop and remove this repository's launchd agent.
    Uninstall,
    /// Load the launchd agent.
    Start,
    /// Unload the launchd agent.
    Stop,
    /// Reload the launchd agent and wait for a fresh daemon process.
    Restart,
    /// Ensure a webhook secret exists and print setup guidance.
    Secret,
}

pub async fn run(context: &AppContext, args: &EventsArgs) -> Result<i32> {
    match args.command.as_ref() {
        Some(EventsCommand::Serve) => {
            events::serve(context)
                .await
                .context("run GitHub events daemon")?;
            Ok(0)
        }
        Some(EventsCommand::Status) => {
            events::status(context).await?;
            Ok(0)
        }
        Some(EventsCommand::Install) => {
            events::install_agent(context).await?;
            Ok(0)
        }
        Some(EventsCommand::Uninstall) => events::agent_mutation(context, "uninstall").await,
        Some(EventsCommand::Start) => events::agent_mutation(context, "load").await,
        Some(EventsCommand::Stop) => events::agent_mutation(context, "unload").await,
        Some(EventsCommand::Restart) => events::agent_mutation(context, "restart").await,
        Some(EventsCommand::Secret) => {
            if events::secret_is_configured(context).await {
                println!("GitHub webhook secret is already configured; the value is not shown.");
            } else {
                let _ = events::ensure_secret_for_install(context).await?;
            }
            if let Some(config) = &context.config.github.events {
                println!(
                    "Webhook URL: https://<your-domain>/webhook (forward to {}:{}/webhook)",
                    config.host, config.port
                );
                println!("Content type: application/json");
                println!(
                    "Configure these events: pull_request, pull_request_review, pull_request_review_thread, issue_comment, check_suite, check_run, status, merge_group, push"
                );
            }
            Ok(0)
        }
        None => {
            println!("usage: wt events <serve|status|install|uninstall|start|stop|restart|secret>");
            Ok(2)
        }
    }
}
