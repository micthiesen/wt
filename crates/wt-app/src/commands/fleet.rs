use anyhow::Result;
use clap::Args;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wt_dev::DevWorktree;
use wt_github::{GithubClient, GithubOptions, MergeQueueEntry};
use wt_harness::ClaudePaths;
use wt_tmux::{TmuxClient, TmuxServer};

use crate::context::AppContext;

#[derive(Debug, Clone, Args, Default)]
pub struct FleetArgs {
    #[arg(long)]
    pub json: bool,
}

pub async fn run(ctx: &AppContext, args: &FleetArgs) -> Result<i32> {
    let rows = ctx.repository.inventory_status(&ctx.cancellation).await?;
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let branches = rows
        .iter()
        .filter(|row| !row.worktree.is_main)
        .map(|row| row.worktree.target.branch.clone())
        .collect::<Vec<_>>();
    let github = GithubClient::new(
        ctx.processes.clone(),
        ctx.config.paths.main_clone.clone(),
        GithubOptions::from_config(&ctx.config, true),
    );
    let (prs, queue, pr_note) = match github.fetch_worktrees(&branches, &ctx.cancellation).await {
        Ok(data) => (Some(data.prs), Some(data.merge_queue), None),
        Err(error) => (None, None, Some(error.to_string())),
    };
    let sessions = TmuxClient::new(
        ctx.processes.clone(),
        TmuxServer::named(ctx.config.tmux.socket.clone()).with_cwd(ctx.home.clone()),
    )
    .list_sessions(&ctx.cancellation)
    .await;
    let (live_sessions, session_note) = match sessions {
        Ok(sessions) => (
            Some(
                sessions
                    .into_iter()
                    .map(|session| session.name)
                    .collect::<std::collections::HashSet<_>>(),
            ),
            None,
        ),
        Err(error) => (None, Some(error.to_string())),
    };
    let claude_paths = ClaudePaths::new(ctx.home.clone(), ctx.config.paths.cache_root.clone());
    let registry_dir = claude_paths.sessions_dir();
    let registry = tokio::task::spawn_blocking(move || read_claude_registry(&registry_dir)).await?;
    let (dev, dev_note) = if ctx.config.dev_server.is_some() {
        let service = crate::dev::service(ctx);
        match service {
            Ok(service) => {
                let inventory = rows
                    .iter()
                    .filter(|row| !row.worktree.is_main)
                    .map(|row| DevWorktree {
                        slug: row.worktree.target.slug().to_owned(),
                        path: row.worktree.target.path.clone().into(),
                        branch: row.worktree.target.branch.clone(),
                    })
                    .collect::<Vec<_>>();
                match service.status_all(&inventory, &ctx.cancellation).await {
                    Ok(snapshot) => (Some(snapshot), None),
                    Err(error) => (None, Some(error.to_string())),
                }
            }
            Err(error) => (None, Some(error.to_string())),
        }
    } else {
        (None, Some("dev server is not configured".to_owned()))
    };
    let dev_by_slug = dev
        .as_ref()
        .map(|snapshot| {
            snapshot
                .worktrees
                .iter()
                .map(|row| (row.slug.as_str(), row))
                .collect::<std::collections::HashMap<_, _>>()
        })
        .unwrap_or_default();
    let live_slugs = rows
        .iter()
        .filter(|row| !row.worktree.is_main)
        .map(|row| row.worktree.target.slug().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    let reports_removed = {
        let live = live_slugs.clone();
        ctx.database
            .call(move |store| Ok(store.recently_removed_worktrees(&live, now_ms())?))
            .await?
    };
    let head_by_slug = rows
        .iter()
        .filter(|row| !row.worktree.is_main)
        .map(|row| {
            (
                row.worktree.target.slug().to_owned(),
                row.worktree.head_sha.clone(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut reports = Vec::new();
    for row in rows.into_iter().filter(|row| !row.worktree.is_main) {
        let slug = row.worktree.target.slug().to_owned();
        let worktree_state = state
            .get("slugs")
            .and_then(Value::as_object)
            .and_then(|slugs| slugs.get(&slug));
        let mut work = worktree_state
            .and_then(|record| record.get("work"))
            .cloned();
        if let Some(work) = work.as_mut().and_then(Value::as_object_mut) {
            // Attribution is explicitly nullable in the fleet JSON contract,
            // even when legacy durable records omitted the optional field.
            work.entry("by").or_insert(Value::Null);
            let stale = work
                .get("sha")
                .and_then(Value::as_str)
                .zip(row.worktree.head_sha.as_deref())
                .is_some_and(|(old, current)| old != current);
            work.insert("stale".into(), Value::Bool(stale));
        }
        let section = worktree_state
            .and_then(|record| record.get("section"))
            .cloned()
            .unwrap_or(Value::Null);
        let pr = prs
            .as_ref()
            .and_then(|prs| prs.get(&row.worktree.target.branch));
        let session_names = live_sessions.as_ref();
        let live_claude = session_names.is_some_and(|sessions| {
            sessions
                .iter()
                .any(|name| name == &slug || name.starts_with(&format!("{slug}~")))
        });
        let alive = session_names.map(|sessions| {
            sessions.iter().any(|name| {
                name == &slug
                    || name.starts_with(&format!("{slug}~"))
                    || name == &format!("{slug}-codex")
                    || name == &format!("{slug}-opencode")
            })
        });
        let claude_activity = registry
            .iter()
            .filter(|entry| {
                entry.cwd == row.worktree.target.path
                    && (entry.name.as_deref() == Some(slug.as_str())
                        || entry.name.as_deref() == Some("primary")
                        || entry.name.is_none())
            })
            .max_by_key(|entry| entry.updated_at);
        let busy = if !live_claude {
            None
        } else {
            claude_activity.map(|entry| matches!(entry.status.as_str(), "busy" | "shell"))
        };
        let last_activity = claude_activity.and_then(|entry| format_timestamp_ms(entry.updated_at));
        let (operation, operation_note) =
            match crate::commands::diagnostics::operation_lock(ctx, &slug).await {
                Ok(lock) => (
                    lock.map(|lock| serde_json::to_value(lock).unwrap_or(Value::Null)),
                    None,
                ),
                Err(error) => (None, Some(error.to_string())),
            };
        let dev_row = dev_by_slug.get(slug.as_str());
        let dev_status = dev_row
            .and_then(|row| row.status.as_ref())
            .map(|status| serde_json::to_value(status).unwrap_or(Value::Null));
        let dev_error = dev_row.and_then(|row| row.error.as_deref());
        let edges = state
            .get("edges")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|edge| {
                edge.get("from").and_then(Value::as_str) == Some(&slug)
                    || edge.get("to").and_then(Value::as_str) == Some(&slug)
            })
            .map(|edge| {
                let from = edge.get("from").and_then(Value::as_str).unwrap_or_default();
                let to = edge.get("to").and_then(Value::as_str).unwrap_or_default();
                let from_sha = edge.get("fromSha").and_then(Value::as_str);
                let to_sha = edge.get("toSha").and_then(Value::as_str);
                let stale = from_sha
                    .zip(head_by_slug.get(from).and_then(Option::as_deref))
                    .is_some_and(|(expected, actual)| expected != actual)
                    || to_sha
                        .zip(head_by_slug.get(to).and_then(Option::as_deref))
                        .is_some_and(|(expected, actual)| expected != actual)
                    || from_sha.is_none()
                    || to_sha.is_none();
                let mut edge = edge.clone();
                if let Some(object) = edge.as_object_mut() {
                    object.insert("stale".into(), Value::Bool(stale));
                }
                edge
            })
            .collect::<Vec<_>>();
        let pr_json = pr.map(|pr| json!({
            "number":pr.number,"url":pr.url,"title":pr.title,"state":pr.state,"isDraft":pr.is_draft,
            "checks":pr.checks,"failedChecks":pr.failed_checks,
            "mergeStateStatus":merge_field(pr.merge_state_status.as_deref(), &pr.state),
            "mergeable":merge_field(pr.mergeable.as_deref(), &pr.state),
            "unresolved_threads":pr.unresolved_threads_total,"unresolved_human_threads":pr.unresolved_threads,
            "review_bot":pr.review_bot.as_ref().map(|status| json!({"state":status.state,"unresolved":status.unresolved,"stale":status.stale.unwrap_or(false)})),
        }));
        let pr_queue = queue
            .as_ref()
            .and_then(|entries| entries.get(&row.worktree.target.branch))
            .map(queue_json);
        let report = json!({
            "kind":"live", "slug":slug, "branch":row.worktree.target.branch,
            "path":row.worktree.target.path, "head":row.worktree.head_sha,
            "base":worktree_state.and_then(|record| record.get("baseBranch")).and_then(Value::as_str).unwrap_or(&ctx.config.branch.base),
            "section":section, "work":work, "edges":edges,
            "dirty":row.status.as_ref().map(|status| status.dirty),
            "session":{"alive":alive,"busy":busy,"last_activity":last_activity}, "session_note":session_note,
            "operation":operation,"operation_note":operation_note,
            "dev":{"status":dev_status,"error":dev_error,"note":dev_note,"slots":dev.as_ref().map(|snapshot| &snapshot.slots)},
            "pr":pr_json, "pr_note":pr_note,"merge_queue":pr_queue,
            "inventory_error":row.error
        });
        reports.push(report);
    }
    for entry in reports_removed {
        let merged = wt_store::is_merged_removal(&entry);
        let work = entry.work.as_ref().map(serde_json::to_value).transpose()?;
        let verification_owed = entry.work.as_ref().is_some_and(|work| {
            work.verify_after_merge.is_some() && work.state != "verified" && work.state != "dropped"
        });
        reports.push(json!({"kind":if merged {"merged"} else {"removed"},"slug":entry.slug,"branch":entry.branch,"path":Value::Null,"head":Value::Null,"base":Value::Null,"section":Value::Null,"work":work,"dirty":Value::Null,"session":Value::Null,"session_note":Value::Null,"pr":entry.extra.get("prNumber").filter(|number| !number.is_null()).map(|number| json!({"number":number,"url":entry.extra.get("prUrl"),"title":entry.extra.get("title")})),"pr_note":Value::Null,"archived_at":entry.removed_at,"verify_after_merge":entry.work.and_then(|work| work.verify_after_merge),"verification_owed":verification_owed}));
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        println!(
            "SLUG                 BRANCH                         WORK       AGENT   DIRTY    PR"
        );
        for row in &reports {
            println!(
                "{:<20} {:<30} {:<10} {:<7} {:<8} {}",
                row["slug"].as_str().unwrap_or("?"),
                row["branch"].as_str().unwrap_or("?"),
                row["work"]["state"].as_str().unwrap_or("—"),
                row["session"]["alive"]
                    .as_bool()
                    .map(|v| if v { "live" } else { "—" })
                    .unwrap_or("unknown"),
                row["dirty"]
                    .as_bool()
                    .map(|v| if v { "dirty" } else { "clean" })
                    .unwrap_or("unknown"),
                row["pr"]["number"]
                    .as_u64()
                    .map(|n| format!("#{n}"))
                    .unwrap_or_else(|| "—".into())
            );
        }
        if reports.is_empty() {
            println!("no live worktrees");
        }
        if let Some(note) = reports.first().and_then(|r| r["pr_note"].as_str()) {
            println!("PR data unavailable: {note}");
        }
        if let Some(note) = reports.first().and_then(|r| r["session_note"].as_str()) {
            println!("Session data unavailable: {note}");
        }
    }
    Ok(0)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn format_timestamp_ms(timestamp_ms: i64) -> Option<String> {
    if timestamp_ms <= 0 {
        return None;
    }
    OffsetDateTime::from_unix_timestamp_nanos(timestamp_ms as i128 * 1_000_000)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

fn merge_field(value: Option<&str>, state: &str) -> Option<String> {
    if state != "OPEN" {
        return None;
    }
    value.map(|value| {
        if value == "UNKNOWN" {
            "computing".to_owned()
        } else {
            value.to_ascii_lowercase()
        }
    })
}

fn queue_json(entry: &MergeQueueEntry) -> Value {
    serde_json::to_value(entry).unwrap_or(Value::Null)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeActivity {
    pid: u32,
    cwd: String,
    name: Option<String>,
    status: String,
    updated_at: i64,
}

fn read_claude_registry(directory: &Path) -> Vec<ClaudeActivity> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "json")
            {
                return None;
            }
            let value: ClaudeActivity =
                serde_json::from_slice(&fs::read(entry.path()).ok()?).ok()?;
            process_is_alive(value.pid).then_some(value)
        })
        .collect()
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: signal zero only checks whether a positive pid exists.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0
            || std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
    }
    #[cfg(not(unix))]
    {
        false
    }
}
