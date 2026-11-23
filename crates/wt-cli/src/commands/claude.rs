use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::json;
use wt_harness::ClaudeSelftestOutcome;

use wt_app_core::{
    context::AppContext,
    harness::{AgentRoute, AppHarness, SelectionSource, unavailable_source_message},
};
use wt_ops::commands::agent::send_to;

#[derive(Debug, Clone, Args)]
pub struct ClaudeArgs {
    #[command(subcommand)]
    pub command: ClaudeCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ClaudeCommand {
    /// Deprecated compatibility alias; selection remains harness-neutral.
    Send {
        target: String,
        #[arg(trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// List live Claude sessions.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Check the private Inspector injection transport for live sessions.
    Selftest { slug: Option<String> },
    /// Stop the primary Claude session for a target.
    #[command(alias = "kill")]
    Stop { target: String },
}

pub async fn run(context: &AppContext, args: &ClaudeArgs) -> Result<i32> {
    if let ClaudeCommand::Send { target, text } = &args.command {
        eprintln!("`wt claude send` is deprecated; routing through `wt agent send`");
        return send_to(context, target, &text.join(" "), None).await;
    }
    if let ClaudeCommand::Stop { target } = &args.command
        && let Some((remote, slug)) = remote_claude_target(context, target)?
    {
        return wt_ops::remote::remote_admin_command(
            context,
            remote,
            &["claude".into(), "stop".into(), slug],
        )
        .await;
    }
    let app = AppHarness::new(context);
    let routes = app.routes(context).await?;
    if routes
        .iter()
        .any(|route| route.choice.source == SelectionSource::Unavailable)
    {
        bail!("could not inspect wt's tmux session registry");
    }
    match &args.command {
        ClaudeCommand::Send { .. } => unreachable!(),
        ClaudeCommand::Ls { json: as_json } => list_claude(context, &app, &routes, *as_json).await,
        ClaudeCommand::Selftest { slug } => selftest(context, &app, &routes, slug.as_deref()).await,
        ClaudeCommand::Stop { target } => {
            let route = resolve_target(context, target, &routes)?;
            if route.target.remote {
                bail!(
                    "remote Claude stops require a canonical @remote/<host>/<slug> target; refusing to stop a local session for {}",
                    route.target.slug
                );
            }
            app.stop_claude(
                &route.target.slug,
                &route.target.cwd,
                route.target.managed_name.clone(),
                context,
            )
            .await?;
            println!("✓ stopped {}'s Claude session", route.target.slug);
            Ok(0)
        }
    }
}

fn remote_claude_target<'a>(
    context: &'a AppContext,
    target: &str,
) -> Result<Option<(&'a wt_config::RemoteConfig, String)>> {
    if !target.starts_with("@remote/") {
        return Ok(None);
    }
    let wt_core::WorktreeRef::Remote { host, slug } =
        wt_core::parse_worktree_ledger_key(target).context("invalid remote worktree key")?
    else {
        bail!("Claude remote target must use a remote worktree key");
    };
    let remote = crate::commands::archive::configured_remote(&context.config.remotes, &host)
        .context("remote host is not configured")?;
    Ok(Some((remote, slug)))
}

fn resolve_target<'a>(
    context: &AppContext,
    target: &str,
    routes: &'a [AgentRoute],
) -> Result<&'a AgentRoute> {
    AppHarness::target_for(target, routes).with_context(|| {
        if target == "wt"
            && context
                .config
                .paths
                .wt_source
                .as_ref()
                .is_none_or(|path| !path.is_dir())
        {
            unavailable_source_message().to_owned()
        } else {
            format!("no live worktree or special session named {target}")
        }
    })
}

async fn list_claude(
    context: &AppContext,
    app: &AppHarness,
    routes: &[AgentRoute],
    as_json: bool,
) -> Result<i32> {
    let inventory = app.session_inventory(context).await?;
    let mut rows = Vec::new();
    for route in routes {
        let live_names = inventory
            .iter()
            .filter(|session| {
                session.name == route.target.slug
                    || session.name.starts_with(&format!("{}~", route.target.slug))
            })
            .collect::<Vec<_>>();
        if live_names.is_empty() {
            continue;
        }
        let mut claude_route = route.clone();
        claude_route.choice.selected = Some(wt_core::HarnessId::Claude);
        claude_route.choice.source = SelectionSource::Live;
        let discovered = app.discover(&claude_route, context).await?;
        for session in discovered.into_iter().filter(|entry| {
            live_names
                .iter()
                .any(|live| live.name == entry.tmux_session_name)
        }) {
            let tmux = live_names
                .iter()
                .find(|live| live.name == session.tmux_session_name)
                .expect("filtered live entry");
            rows.push(json!({
                "slug": route.target.slug,
                "name": session.extras.managed_name,
                "session_id": session.session_id,
                "cwd": route.target.cwd,
                "tmux_session": tmux.name,
                "alive": true,
                "last_activity_ms": session.last_active_ms,
                "status": session.extras.derived_state,
                "waiting_for": session.extras.waiting_for,
            }));
        }
    }
    rows.sort_by(|a, b| a["slug"].as_str().cmp(&b["slug"].as_str()));
    if as_json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() {
        println!("no live Claude sessions");
    } else {
        for row in rows {
            let slug = row["slug"].as_str().unwrap_or_default();
            let name = row["name"].as_str();
            println!(
                "{}",
                name.map_or_else(|| slug.to_owned(), |name| format!("{slug} ~{name}"))
            );
        }
    }
    Ok(0)
}

async fn selftest(
    context: &AppContext,
    app: &AppHarness,
    routes: &[AgentRoute],
    wanted: Option<&str>,
) -> Result<i32> {
    let inventory = app.session_inventory(context).await?;
    let mut entries = Vec::new();
    for route in routes {
        if wanted.is_some_and(|wanted| {
            route.target.slug != wanted && route.target.branch.as_deref() != Some(wanted)
        }) {
            continue;
        }
        for session in &inventory {
            if session.name == route.target.slug
                || session.name.starts_with(&format!("{}~", route.target.slug))
            {
                entries.push((route.target.slug.as_str(), session.name.as_str()));
            }
        }
    }
    entries.sort_by(|a, b| a.1.cmp(b.1));
    if entries.is_empty() {
        println!(
            "{}",
            wanted.map_or_else(
                || "no live Claude sessions".into(),
                |slug| format!("no live Claude session for {slug}")
            )
        );
        return Ok(if wanted.is_some() { 1 } else { 0 });
    }
    let total = entries.len();
    let (mut broken, mut uninspectable) = (0, 0);
    for (_, tmux_name) in entries {
        let (verdict, line) = selftest_line(&app.claude_selftest(tmux_name, context).await);
        match verdict {
            SelftestVerdict::Reachable => println!("✓ {tmux_name}: {line}"),
            SelftestVerdict::Uninspectable => {
                uninspectable += 1;
                println!("– {tmux_name}: {line}");
            }
            SelftestVerdict::Broken => {
                broken += 1;
                println!("✗ {tmux_name}: {line}");
            }
        }
    }
    if broken + uninspectable > 0 {
        println!(
            "{} of {total} session(s) take messages by terminal typing instead of prompt delivery; messages still arrive, but a half-typed draft in that session can be disturbed",
            broken + uninspectable
        );
    }
    // A session wt cannot inspect is not a fault unless it is the one asked about.
    Ok(i32::from(
        broken > 0 || (wanted.is_some() && uninspectable > 0),
    ))
}

#[derive(Debug, Eq, PartialEq)]
enum SelftestVerdict {
    Reachable,
    Uninspectable,
    Broken,
}

/// One line per session saying what prompt delivery can do there and, when it
/// cannot, the single action that restores it.
fn selftest_line(outcome: &ClaudeSelftestOutcome) -> (SelftestVerdict, String) {
    use wt_harness::ClaudeInjectFailureKind as Kind;
    match outcome {
        ClaudeSelftestOutcome::Ready {
            found_input,
            found_caret,
        } => (
            SelftestVerdict::Reachable,
            match (found_input, found_caret) {
                (true, true) => "prompt delivery works; a typed draft and its caret are kept".into(),
                (true, false) => {
                    "prompt delivery works; a typed draft is kept but its caret moves".into()
                }
                (false, _) => {
                    "prompt delivery works, but a typed draft is lost when a message arrives"
                        .into()
                }
            },
        ),
        ClaudeSelftestOutcome::Failed { kind, reason } => match kind {
            Kind::Absent => (
                SelftestVerdict::Uninspectable,
                "no inspector socket; start the session from wt (without WT_INSPECT=off) to get prompt delivery".into(),
            ),
            Kind::Stale => (
                SelftestVerdict::Uninspectable,
                format!("its inspector socket is dead; restart the session from wt ({reason})"),
            ),
            Kind::NotReady => (
                SelftestVerdict::Broken,
                format!(
                    "wt cannot find the prompt in this Claude Code version ({reason}); update wt, or report it if wt is current"
                ),
            ),
            Kind::Blocked | Kind::SubmittedUnknown | Kind::Failed => (
                SelftestVerdict::Broken,
                format!("the inspector call failed ({reason}); rerun, then restart the session from wt if it repeats"),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_harness::ClaudeInjectFailureKind as Kind;

    #[test]
    fn selftest_separates_sessions_it_cannot_inspect_from_broken_delivery() {
        let failed = |kind| ClaudeSelftestOutcome::Failed {
            kind,
            reason: "why".into(),
        };
        assert_eq!(
            selftest_line(&failed(Kind::Absent)).0,
            SelftestVerdict::Uninspectable
        );
        assert_eq!(
            selftest_line(&failed(Kind::Stale)).0,
            SelftestVerdict::Uninspectable
        );
        let (verdict, line) = selftest_line(&failed(Kind::NotReady));
        assert_eq!(verdict, SelftestVerdict::Broken);
        assert!(line.contains("update wt"));
        // Delivery submits without the input fiber; only the draft is at risk.
        let (verdict, line) = selftest_line(&ClaudeSelftestOutcome::Ready {
            found_input: false,
            found_caret: false,
        });
        assert_eq!(verdict, SelftestVerdict::Reachable);
        assert!(line.contains("draft is lost"));
    }
}
