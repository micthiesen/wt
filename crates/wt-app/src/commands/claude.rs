use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::json;
use wt_harness::ClaudeSelftestOutcome;

use crate::{
    commands::agent::send_to,
    context::AppContext,
    harness::{AgentRoute, AppHarness, SelectionSource, unavailable_source_message},
};

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
                    "remote Claude session control is not wired to the remote runtime; refusing local stop for {}",
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
    let mut failures = 0;
    for (_, tmux_name) in entries {
        match app.claude_selftest(tmux_name, context).await {
            ClaudeSelftestOutcome::Ready {
                found_input,
                found_caret,
            } => println!(
                "✓ {tmux_name} prompt + input{}",
                if found_caret {
                    " + caret"
                } else if found_input {
                    " (no caret restore)"
                } else {
                    ""
                }
            ),
            ClaudeSelftestOutcome::Failed { kind, reason } => {
                failures += 1;
                println!("✗ {tmux_name} {kind:?}: {reason}");
            }
        }
    }
    Ok(i32::from(failures > 0))
}
