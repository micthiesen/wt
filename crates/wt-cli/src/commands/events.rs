use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use wt_app_core::context::AppContext;
use wt_github::{GithubClient, GithubOptions, RateLimit};
use wt_ops::events;

/// Probes for the token's budget windows. GitHub answers each from one
/// window, and about one in four from the less used one.
const BUDGET_PROBES: usize = 6;

/// Who GitHub says the token is, and every GraphQL budget window it is
/// counting that token against right now.
pub(crate) struct TokenBudget {
    pub login: Option<String>,
    pub windows: Result<Vec<RateLimit>, String>,
}

/// Ask GitHub directly, with queries that cost no GraphQL points. For
/// commands a person runs to see what is going on; nothing reads this on
/// a schedule.
pub(crate) async fn token_budget(context: &AppContext) -> TokenBudget {
    let client = GithubClient::new(
        context.processes.clone(),
        context.config.paths.main_clone.clone(),
        GithubOptions::from_config(&context.config, false),
    );
    let (login, windows) = tokio::join!(
        client.authenticated_login(&context.cancellation),
        client.budget_windows(BUDGET_PROBES, &context.cancellation),
    );
    TokenBudget {
        login: login.ok().flatten(),
        windows: windows.map_err(|error| error.to_string()),
    }
}

/// `4613 of 5000 left (resets in 21m), 4828 of 5000 left (resets in 39m)`:
/// each open window, soonest reset first. `None` when there is none.
pub(crate) fn budget_text(windows: &[RateLimit], now_ms: u64) -> Option<String> {
    let open: Vec<String> = windows
        .iter()
        .filter_map(|window| {
            let to_reset = window
                .reset_at_ms
                .checked_sub(now_ms)
                .filter(|ms| *ms > 0)?;
            Some(format!(
                "{} of {} left (resets in {}m)",
                window.remaining,
                window.limit,
                to_reset.div_ceil(60_000)
            ))
        })
        .collect();
    (!open.is_empty()).then(|| open.join(", "))
}

/// The lines a report prints about the token, each a label and its text.
pub(crate) fn budget_lines(budget: &TokenBudget, now_ms: u64) -> Vec<(&'static str, String)> {
    let mut lines = Vec::new();
    if let Some(login) = &budget.login {
        lines.push(("account", login.clone()));
    }
    match &budget.windows {
        Ok(windows) => {
            if let Some(text) = budget_text(windows, now_ms) {
                lines.push(("budget now", text));
            }
            if windows.len() > 1 {
                lines.push((
                    "",
                    "GitHub counts this token in more than one window; the lowest runs out first"
                        .into(),
                ));
            }
        }
        Err(error) => lines.push(("budget now", format!("unavailable: {error}"))),
    }
    lines
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

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
    /// Reload the launchd agent only if its daemon runs a build other than
    /// the installed one. `wt update` runs this after installing a build.
    Reconcile,
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
            if context.config.github.events.is_some() {
                for (label, text) in budget_lines(&token_budget(context).await, now_ms()) {
                    println!("  {label:<12} {text}");
                }
            }
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
        Some(EventsCommand::Reconcile) => {
            match events::reconcile(context).await? {
                events::Reconciled::NotInstalled | events::Reconciled::Current => {}
                events::Reconciled::Foreign => println!(
                    "GitHub events agent belongs to another configuration; run `wt events restart` there"
                ),
                events::Reconciled::Restarted { build } => {
                    println!("restarted GitHub events daemon on build {build}")
                }
            }
            Ok(0)
        }
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
            println!(
                "usage: wt events <serve|status|install|uninstall|start|stop|restart|reconcile|secret>"
            );
            Ok(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log names a read's process by its subcommand, from a list kept
    /// beside the read. A subcommand missing there would be logged as `wt`.
    #[test]
    fn every_subcommand_and_alias_is_a_known_log_role() {
        use clap::CommandFactory;
        let command = crate::Cli::command();
        for sub in command.get_subcommands() {
            for name in std::iter::once(sub.get_name()).chain(sub.get_all_aliases()) {
                assert!(wt_github::ROLES.contains(&name), "{name}");
            }
        }
    }

    #[test]
    fn the_token_report_names_the_account_and_every_open_window() {
        let window = |remaining, reset_at_ms| RateLimit {
            limit: 5_000,
            remaining,
            cost: 0,
            reset_at_ms,
        };
        let now = 1_000_000;
        let both = TokenBudget {
            login: Some("octocat".into()),
            windows: Ok(vec![
                window(4_613, now + 21 * 60_000 - 1),
                window(4_828, now + 39 * 60_000),
            ]),
        };
        assert_eq!(
            budget_lines(&both, now),
            vec![
                ("account", "octocat".to_owned()),
                (
                    "budget now",
                    "4613 of 5000 left (resets in 21m), 4828 of 5000 left (resets in 39m)"
                        .to_owned()
                ),
                (
                    "",
                    "GitHub counts this token in more than one window; the lowest runs out first"
                        .to_owned()
                ),
            ]
        );
        // One window needs no explanation; a closed one is not a budget.
        let one = TokenBudget {
            login: None,
            windows: Ok(vec![window(10, now + 1)]),
        };
        assert_eq!(
            budget_lines(&one, now),
            vec![("budget now", "10 of 5000 left (resets in 1m)".to_owned())]
        );
        assert_eq!(budget_text(&[window(10, now)], now), None);
        let failed = TokenBudget {
            login: None,
            windows: Err("gh is not installed".into()),
        };
        assert_eq!(
            budget_lines(&failed, now),
            vec![("budget now", "unavailable: gh is not installed".to_owned())]
        );
    }
}
