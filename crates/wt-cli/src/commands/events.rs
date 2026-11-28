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

/// One reading of a budget window some wt process already took, with when
/// the read that reported it began (epoch ms).
type Reading = (RateLimit, u64);

/// The budget as wt last read it, for a report that should not spend two
/// GitHub calls to say what every GitHub read already reports: the newest
/// reading of each window still open, from the events daemon's snapshot,
/// the board's last read and the read waits and display commands share.
/// Empty when every window read has since reset: nothing then describes the
/// budget, and only GitHub can.
async fn recorded_budget(context: &AppContext, now_ms: u64) -> Vec<Reading> {
    // Read as plain JSON: all that is wanted of the snapshot is what it
    // recorded of the budget, whatever else its writer's build put there.
    let snapshot = tokio::fs::read(events::events_directory(&context.config).join("github.json"))
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        // Another repository's daemon may use another token.
        .filter(|snapshot| {
            snapshot["repository"].as_str()
                == Some(events::repository_identity(&context.config).as_str())
        });
    let (board, shared) = tokio::join!(
        wt_sources::wait_facts::board_read(context),
        wt_sources::wait_facts::latest_shared_read(context),
    );
    let mut readings = Vec::new();
    if let Some(snapshot) = snapshot
        && let Some(written_ms) = snapshot["updatedAt"].as_u64()
    {
        let window =
            |value: &serde_json::Value| serde_json::from_value::<RateLimit>(value.clone()).ok();
        let windows = snapshot["rateWindows"]
            .as_array()
            .filter(|windows| !windows.is_empty())
            .map(|windows| windows.iter().filter_map(window).collect::<Vec<_>>())
            .unwrap_or_else(|| window(&snapshot["rateLimit"]).into_iter().collect());
        readings.extend(windows.into_iter().map(|window| (window, written_ms)));
    }
    for (data, started_ms) in [board, shared].into_iter().flatten() {
        readings.extend(data.rate_limit.map(|window| (window, started_ms)));
    }
    open_windows(readings, now_ms)
}

/// The newest reading of each window still open at `now_ms`, soonest reset
/// first. GitHub reports a window's reset to the second, and two readings
/// of one window can differ by one.
fn open_windows(mut readings: Vec<Reading>, now_ms: u64) -> Vec<Reading> {
    readings.retain(|(window, read_ms)| window.reset_at_ms > now_ms && *read_ms <= now_ms);
    readings.sort_by_key(|(_, read_ms)| std::cmp::Reverse(*read_ms));
    let mut open: Vec<Reading> = Vec::new();
    for reading in readings {
        let same = |kept: &Reading| kept.0.reset_at_ms.abs_diff(reading.0.reset_at_ms) <= 2_000;
        if !open.iter().any(same) {
            open.push(reading);
        }
    }
    open.sort_by_key(|(window, _)| window.reset_at_ms);
    open
}

/// `4613 of 5000 left (resets in 21m), read 40s ago`: what
/// [`recorded_budget`] found, each window with the age of its reading, so
/// nobody takes it for a read made now.
fn recorded_budget_lines(readings: &[Reading], now_ms: u64) -> Vec<(&'static str, String)> {
    let age = |read_ms: u64| {
        let seconds = now_ms.saturating_sub(read_ms) / 1_000;
        if seconds < 60 {
            format!("{seconds}s")
        } else {
            format!("{}m", seconds / 60)
        }
    };
    let open: Vec<String> = readings
        .iter()
        .filter_map(|(window, read_ms)| {
            Some(format!(
                "{}, read {} ago",
                budget_text(std::slice::from_ref(window), now_ms)?,
                age(*read_ms)
            ))
        })
        .collect();
    let mut lines = Vec::new();
    if !open.is_empty() {
        lines.push(("budget", open.join("; ")));
    }
    if open.len() > 1 {
        lines.push((
            "",
            "GitHub counts this token in more than one window; the lowest runs out first".into(),
        ));
    }
    lines
}

/// Who GitHub said the token is when it was last asked, and the budget
/// windows it was counting that token in then. A budget read later is this
/// account's only while it is of one of those windows: another token has
/// windows of its own, with other reset times.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Account {
    repository: String,
    login: String,
    reset_at_ms: Vec<u64>,
}

impl Account {
    fn knows(&self, window: &RateLimit) -> bool {
        self.reset_at_ms
            .iter()
            .any(|reset| reset.abs_diff(window.reset_at_ms) <= 2_000)
    }

    /// The login to print beside `readings`, when every one of them is of a
    /// window GitHub was seen counting this account in.
    fn owner_of(&self, readings: &[Reading]) -> Option<&str> {
        (!readings.is_empty() && readings.iter().all(|(window, _)| self.knows(window)))
            .then_some(self.login.as_str())
    }

    /// The account after GitHub answered `login` and `windows` just now.
    /// One answer does not show every window, so the windows an earlier
    /// answer showed for the same login are kept while they are open.
    fn seen(self, repository: &str, login: &str, windows: &[RateLimit], now_ms: u64) -> Self {
        let same = self.repository == repository && self.login == login;
        let mut next = Self {
            reset_at_ms: windows.iter().map(|window| window.reset_at_ms).collect(),
            ..Self::default()
        };
        for reset in self.reset_at_ms {
            let kept = RateLimit {
                limit: 0,
                remaining: 0,
                cost: 0,
                reset_at_ms: reset,
            };
            if same && reset > now_ms && !next.knows(&kept) {
                next.reset_at_ms.push(reset);
            }
        }
        let reset_at_ms = next.reset_at_ms;
        Self {
            repository: repository.to_owned(),
            login: login.to_owned(),
            reset_at_ms,
        }
    }
}

fn account_path(context: &AppContext) -> std::path::PathBuf {
    context.config.paths.cache_root.join("github-account.json")
}

async fn remembered_account(context: &AppContext) -> Account {
    let repository = events::repository_identity(&context.config);
    tokio::fs::read(account_path(context))
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Account>(&bytes).ok())
        .filter(|account| account.repository == repository)
        .unwrap_or_default()
}

/// Keep what a live answer said of the account for the reports that follow.
/// A file that cannot be written only costs them the same question again.
async fn remember_account(context: &AppContext, account: &Account) {
    let path = account_path(context);
    let staged = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let Ok(bytes) = serde_json::to_vec(account) else {
        return;
    };
    if tokio::fs::write(&staged, bytes).await.is_err()
        || tokio::fs::rename(&staged, &path).await.is_err()
    {
        let _ = tokio::fs::remove_file(&staged).await;
    }
}

/// The token lines of `wt events status`: the account and its budget from
/// what wt already knows, or from GitHub when that does not say whose
/// budget it is or how much is left.
async fn status_budget_lines(context: &AppContext) -> Vec<(&'static str, String)> {
    let now = now_ms();
    let readings = recorded_budget(context, now).await;
    let account = remembered_account(context).await;
    if let Some(login) = account.owner_of(&readings) {
        let mut lines = vec![("account", login.to_owned())];
        lines.extend(recorded_budget_lines(&readings, now));
        return lines;
    }
    let budget = token_budget(context).await;
    if let (Some(login), Ok(windows)) = (&budget.login, &budget.windows) {
        let repository = events::repository_identity(&context.config);
        remember_account(
            context,
            &account.seen(&repository, login, windows, now_ms()),
        )
        .await;
    }
    budget_lines(&budget, now_ms())
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
                for (label, text) in status_budget_lines(context).await {
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
    fn a_recorded_budget_shows_the_newest_reading_of_each_open_window_with_its_age() {
        let window = |remaining, reset_at_ms| RateLimit {
            limit: 5_000,
            remaining,
            cost: 6,
            reset_at_ms,
        };
        let now = 10_000_000;
        let soon = now + 21 * 60_000;
        let later = now + 39 * 60_000;
        let readings = vec![
            // The daemon's old reading and the board's newer one of the
            // same window, a second window, and one that has reset.
            (window(4_700, soon), now - 15 * 60_000),
            (window(4_613, soon + 1_000), now - 40_000),
            (window(4_828, later), now - 5 * 60_000),
            (window(10, now - 1), now - 30 * 60_000),
            // A reading from a clock ahead of this one says nothing.
            (window(1, later), now + 60_000),
        ];
        let open = open_windows(readings, now);
        assert_eq!(
            open.iter()
                .map(|(window, _)| window.remaining)
                .collect::<Vec<_>>(),
            [4_613, 4_828]
        );
        assert_eq!(
            recorded_budget_lines(&open, now),
            vec![
                (
                    "budget",
                    "4613 of 5000 left (resets in 22m), read 40s ago; 4828 of 5000 left (resets in 39m), read 5m ago"
                        .to_owned()
                ),
                (
                    "",
                    "GitHub counts this token in more than one window; the lowest runs out first"
                        .to_owned()
                ),
            ]
        );
        // Every window read has reset: nothing recorded describes the
        // budget, so the report asks GitHub.
        let reset = open_windows(vec![(window(10, now - 1), now - 60_000)], now);
        assert!(recorded_budget_lines(&reset, now).is_empty());
    }

    #[test]
    fn a_recorded_budget_is_an_accounts_only_in_windows_github_showed_for_it() {
        let window = |reset_at_ms| RateLimit {
            limit: 5_000,
            remaining: 4_000,
            cost: 6,
            reset_at_ms,
        };
        let now = 10_000_000;
        let (soon, later) = (now + 20 * 60_000, now + 40 * 60_000);
        // Nothing remembered: whose budget a reading is cannot be said.
        assert_eq!(Account::default().owner_of(&[(window(soon), now)]), None);
        let account = Account::default().seen("repo:/a", "octocat", &[window(soon)], now);
        assert_eq!(
            account.owner_of(&[(window(soon + 1_000), now)]),
            Some("octocat")
        );
        // No reading at all, or one of a window GitHub was not seen
        // counting this account in (another token's, or a new hour's):
        // GitHub is asked, never guessed for.
        assert_eq!(account.owner_of(&[]), None);
        assert_eq!(
            account.owner_of(&[(window(soon), now), (window(later), now)]),
            None
        );
        // The next answer shows the other window; the first is kept while
        // it is open, for the same login only.
        let both = account
            .clone()
            .seen("repo:/a", "octocat", &[window(later)], now);
        assert_eq!(
            both.owner_of(&[(window(soon), now), (window(later), now)]),
            Some("octocat")
        );
        assert_eq!(
            both.clone()
                .seen("repo:/a", "octocat", &[window(later)], soon + 1)
                .reset_at_ms,
            [later]
        );
        let switched = both.seen("repo:/a", "hubot", &[window(later)], now);
        assert_eq!(switched.owner_of(&[(window(soon), now)]), None);
        assert_eq!(switched.owner_of(&[(window(later), now)]), Some("hubot"));
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
