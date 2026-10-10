use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Args;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{sync::Semaphore, task::JoinSet};
use wt_dev::{DevServerStatus, DevWorktree};
use wt_github::{GithubClient, GithubOptions, PullRequest};
use wt_store::{RemovedWorktree, WorkStatusRecord};
use wt_vcs::WorktreeSnapshot;

use crate::{
    commands::{diagnostics::OperationLock, resolve::run_git},
    context::AppContext,
    issue_identity,
    worktree_facts::{PushFacts, push_facts},
};

#[derive(Debug, Clone, Args, Default)]
pub struct ListArgs {
    #[arg(long)]
    pub json: bool,
}

pub async fn run(ctx: &AppContext, args: &ListArgs) -> Result<i32> {
    let snapshots = ctx.repository.inventory_status(&ctx.cancellation).await?;
    let live = snapshots
        .iter()
        .filter(|snapshot| !snapshot.worktree.is_main)
        .collect::<Vec<_>>();
    let live_slugs = live
        .iter()
        .map(|snapshot| snapshot.worktree.target.slug().to_owned())
        .collect::<BTreeSet<_>>();
    let now = now_ms();
    let (state, removed) = ctx
        .database
        .call(move |store| {
            Ok((
                store.read_wt_state()?,
                store.recently_removed_worktrees(&live_slugs, now)?,
            ))
        })
        .await?;
    let locks = collect_operation_locks(ctx, &live).await?;
    if args.json {
        let remote_url = run_git(
            ctx,
            &ctx.config.paths.main_clone,
            ["remote", "get-url", "origin"],
        )
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        let pushes = collect_push_facts(ctx, &live, &state).await?;
        let dev = collect_dev_status(ctx, &live).await?;
        let rows = live
            .iter()
            .map(|snapshot| {
                let slug = snapshot.worktree.target.slug();
                live_json(
                    ctx,
                    &state,
                    snapshot,
                    LiveFacts {
                        remote_url: remote_url.as_deref(),
                        push: pushes.get(slug).copied().unwrap_or_default(),
                        lock: locks.get(slug).and_then(|lock| lock.as_ref()),
                        dev: dev.get(slug).and_then(|row| row.status.as_ref()),
                        dev_error: dev.get(slug).and_then(|row| row.error.as_deref()),
                    },
                )
            })
            .chain(removed.iter().map(removed_json))
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    if live.is_empty() {
        if removed.is_empty() {
            println!("No worktrees.");
        } else {
            println!("No active worktrees ({} recently removed).", removed.len());
        }
        return Ok(0);
    }
    let branches = live
        .iter()
        .map(|snapshot| snapshot.worktree.target.branch.clone())
        .collect::<Vec<_>>();
    let github = GithubClient::new(
        ctx.processes.clone(),
        ctx.config.paths.main_clone.clone(),
        GithubOptions::from_config(&ctx.config, true),
    );
    let prs = match github.fetch_worktrees(&branches, &ctx.cancellation).await {
        Ok(data) => Some(data.prs),
        Err(error) => {
            eprintln!("PR data unavailable: {error}");
            None
        }
    };
    let with_stage = ctx.config.sst.is_some();
    if with_stage {
        println!(
            "{:<30}  {:<38}  {:<20}  {:<16}  STATUS  PATH",
            "SLUG", "BRANCH", "STAGE", "PR"
        );
    } else {
        println!(
            "{:<30}  {:<38}  {:<16}  STATUS  PATH",
            "SLUG", "BRANCH", "PR"
        );
    }
    for snapshot in live {
        let row = &snapshot.worktree;
        let status = status_label(
            snapshot,
            locks.get(row.target.slug()).and_then(|lock| lock.as_ref()),
        );
        let pr = prs
            .as_ref()
            .map(|prs| {
                pr_cell(wt_github::pick_pr_for_worktree(
                    Some(&row.target.branch),
                    Path::new(&row.target.path),
                    prs,
                ))
            })
            .unwrap_or_else(|| "unavailable".into());
        if with_stage {
            println!(
                "{:<30}  {:<38}  {:<20}  {:<16}  {}  {}",
                row.target.slug(),
                row.target.branch,
                stage_cell(ctx, row.target.path.as_ref()),
                pr,
                status,
                row.target.path
            );
        } else {
            println!(
                "{:<30}  {:<38}  {:<16}  {}  {}",
                row.target.slug(),
                row.target.branch,
                pr,
                status,
                row.target.path
            );
        }
    }
    let merged = removed
        .iter()
        .filter(|entry| is_merged(entry))
        .map(|entry| entry.slug.as_str())
        .collect::<Vec<_>>();
    if !merged.is_empty() {
        println!("\nrecently merged: {}", merged.join(", "));
    }
    Ok(0)
}

async fn collect_push_facts(
    ctx: &AppContext,
    rows: &[&WorktreeSnapshot],
    state: &Value,
) -> Result<BTreeMap<String, PushFacts>> {
    let semaphore = Arc::new(Semaphore::new(8));
    let mut tasks = JoinSet::new();
    for row in rows {
        let target = row.worktree.target.clone();
        let slug = target.slug().to_owned();
        let base = state
            .get("slugs")
            .and_then(|slugs| slugs.get(&slug))
            .and_then(|entry| entry.get("baseBranch"))
            .and_then(Value::as_str)
            .unwrap_or(&ctx.config.branch.base)
            .to_owned();
        let context = ctx.clone();
        let semaphore = semaphore.clone();
        tasks.spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .context("acquire bounded Git fact slot")?;
            let facts = push_facts(&context, Path::new(&target.path), &target.branch, &base).await;
            Ok::<_, anyhow::Error>((slug, facts))
        });
    }
    let mut result = BTreeMap::new();
    while let Some(row) = tasks.join_next().await {
        let (slug, facts) = row.context("worktree fact task panicked")??;
        result.insert(slug, facts);
    }
    Ok(result)
}

async fn collect_operation_locks(
    ctx: &AppContext,
    rows: &[&WorktreeSnapshot],
) -> Result<BTreeMap<String, Option<OperationLock>>> {
    let semaphore = Arc::new(Semaphore::new(8));
    let mut tasks = JoinSet::new();
    for row in rows {
        let slug = row.worktree.target.slug().to_owned();
        let context = ctx.clone();
        let semaphore = semaphore.clone();
        tasks.spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .context("acquire bounded operation-lock slot")?;
            let lock = crate::commands::diagnostics::operation_lock(&context, &slug).await?;
            Ok::<_, anyhow::Error>((slug, lock))
        });
    }
    let mut result = BTreeMap::new();
    while let Some(row) = tasks.join_next().await {
        let (slug, lock) = row.context("operation-lock task panicked")??;
        result.insert(slug, lock);
    }
    Ok(result)
}

async fn collect_dev_status(
    ctx: &AppContext,
    rows: &[&WorktreeSnapshot],
) -> Result<BTreeMap<String, wt_dev::DevStatusRow>> {
    if rows.is_empty() || ctx.config.dev_server.is_none() {
        return Ok(BTreeMap::new());
    }
    let service = crate::dev::service(ctx).context("build dev-server status service")?;
    let worktrees = rows
        .iter()
        .map(|row| DevWorktree::from(&row.worktree))
        .collect::<Vec<_>>();
    let snapshot = service
        .status_all(&worktrees, &ctx.cancellation)
        .await
        .context("read batched dev-server status")?;
    Ok(snapshot
        .worktrees
        .into_iter()
        .map(|row| (row.slug.clone(), row))
        .collect())
}

fn status_label(snapshot: &WorktreeSnapshot, lock: Option<&OperationLock>) -> String {
    if let Some(lock) = lock {
        return lock_label(lock);
    }
    match snapshot.status.as_ref() {
        Some(status) if status.dirty => "dirty".into(),
        Some(_) => "clean".into(),
        None => "missing".into(),
    }
}

fn lock_label(lock: &OperationLock) -> String {
    match (lock.op.as_deref(), lock.phase.as_deref()) {
        (Some(op), Some(phase)) if phase != op => format!("{op}: {phase}"),
        (_, Some(phase)) => phase.to_owned(),
        (Some(op), None) => op.to_owned(),
        (None, None) => "busy".into(),
    }
}

fn lock_age(lock: &OperationLock) -> Option<String> {
    let started = lock
        .phase_started
        .as_deref()
        .or(lock.started_at.as_deref())?;
    let timestamp = OffsetDateTime::parse(started, &Rfc3339).ok()?;
    let seconds = (OffsetDateTime::now_utc() - timestamp)
        .whole_seconds()
        .max(0);
    Some(if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    })
}

fn pr_cell(pr: Option<&PullRequest>) -> String {
    let Some(pr) = pr else {
        return "—".into();
    };
    let state = if pr.state == "MERGED" {
        " (merged)"
    } else if pr.state == "CLOSED" {
        " (closed)"
    } else if pr.is_draft {
        " (draft)"
    } else {
        ""
    };
    format!("#{}{state}", pr.number)
}

fn stage_cell(ctx: &AppContext, path: &str) -> String {
    match wt_sst::observe_local_deployment(Path::new(path), &ctx.config.stage.prefix) {
        wt_sst::DeploymentObservation::Deployed { stage } => stage,
        wt_sst::DeploymentObservation::NotDeployed { .. } => "(not deployed)".into(),
        wt_sst::DeploymentObservation::Unknown { .. } => "unknown".into(),
    }
}

fn is_deployed(ctx: &AppContext, path: &str) -> bool {
    matches!(
        wt_sst::observe_local_deployment(Path::new(path), &ctx.config.stage.prefix),
        wt_sst::DeploymentObservation::Deployed { .. }
    )
}

struct LiveFacts<'a> {
    remote_url: Option<&'a str>,
    push: PushFacts,
    lock: Option<&'a OperationLock>,
    dev: Option<&'a DevServerStatus>,
    dev_error: Option<&'a str>,
}

fn live_json(
    ctx: &AppContext,
    state: &Value,
    snapshot: &WorktreeSnapshot,
    facts: LiveFacts<'_>,
) -> Value {
    let LiveFacts {
        remote_url,
        push,
        lock,
        dev,
        dev_error,
    } = facts;
    let target = &snapshot.worktree.target;
    let entry = state
        .get("slugs")
        .and_then(|slugs| slugs.get(target.slug()));
    let work = entry
        .and_then(|entry| entry.get("work"))
        .and_then(|value| serde_json::from_value::<WorkStatusRecord>(value.clone()).ok());
    let issue_override = entry
        .and_then(|entry| entry.get("issueId"))
        .and_then(Value::as_str);
    let issue_id = issue_identity::resolve(target.slug(), issue_override);
    let status = match (lock, snapshot.status.as_ref()) {
        (Some(lock), _) => ("busy", lock_label(lock)),
        (None, Some(status)) if status.dirty => ("dirty", "dirty".into()),
        (None, Some(_)) => ("clean", "clean".into()),
        (None, None) => ("missing", "missing".into()),
    };
    let deployed = is_deployed(ctx, target.path.as_ref());
    // No configured service is a known stopped state. A configured service
    // whose observation failed stays null, with dev_error explaining why.
    let unconfigured_dev = DevServerStatus::default();
    let dev = dev.or_else(|| ctx.config.dev_server.is_none().then_some(&unconfigured_dev));
    let base = entry
        .and_then(|entry| entry.get("baseBranch"))
        .and_then(Value::as_str)
        .unwrap_or(&ctx.config.branch.base);
    json!({
        "slug": target.slug(),
        "branch": target.branch,
        "path": target.path,
        "stage": target.stage,
        "deployed": deployed,
        "kind": "live",
        "section": if ctx.config.instance.role == wt_config::InstanceRole::Worker { None } else { entry.and_then(|entry| entry.get("section")).and_then(Value::as_str) },
        "base": base,
        "exists": true,
        "status": status.0,
        "status_label": status.1,
        "status_age": lock.and_then(lock_age),
        "status_op": lock.and_then(|lock| lock.op.as_deref()),
        "dev": dev,
        "dev_error": dev_error,
        "dirty": snapshot.status.as_ref().is_some_and(|status| status.dirty),
        "unpushed": push.unpushed,
        "pushed": push.pushed,
        "ahead_of_base": push.ahead_of_base,
        "issue_id": issue_id,
        "issue_url": issue_id.as_deref().and_then(|id| issue_url(ctx, id, remote_url)),
        "gh_issue": entry.and_then(|entry| entry.get("githubIssue")),
        "gh_issue_url": entry
            .and_then(|entry| entry.get("githubIssue"))
            .and_then(Value::as_u64)
            .and_then(|issue| remote_url.and_then(repo_web_url).map(|repo| format!("{repo}/issues/{issue}"))),
        "work_state": work.as_ref().map(|work| work.state.as_str()),
        "work_note": work.as_ref().and_then(|work| work.note.as_deref()),
        "work_risk": work.as_ref().and_then(|work| work.risk.as_deref()),
        "work_blocked_on": work.as_ref().and_then(|work| work.blocked_on.as_deref()),
        "work_verify_after_merge": work.as_ref().and_then(|work| work.verify_after_merge.as_deref()),
        "work_at": work.as_ref().map(|work| work.at.as_str()),
        "inventory_error": snapshot.error,
    })
}

fn issue_url(ctx: &AppContext, id: &str, remote_url: Option<&str>) -> Option<String> {
    if id.to_ascii_uppercase().starts_with("GH-") {
        let repo = repo_web_url(remote_url?)?;
        return Some(format!("{repo}/issues/{}", id.split_once('-')?.1));
    }
    Some(
        ctx.config
            .issue_tracker
            .as_ref()?
            .url_template
            .as_ref()?
            .replace("{id}", &id.to_ascii_uppercase()),
    )
}

fn repo_web_url(remote: &str) -> Option<String> {
    let raw = remote
        .strip_prefix("https://")
        .or_else(|| remote.strip_prefix("http://"))
        .or_else(|| remote.strip_prefix("git@"))
        .or_else(|| remote.strip_prefix("ssh://git@"))?;
    let (host, path) = if let Some((host, path)) = raw.split_once(':') {
        (host, path)
    } else {
        raw.split_once('/')?
    };
    let host = host.split('/').next()?;
    let path = path.trim_end_matches(".git").trim_end_matches('/');
    Some(format!("https://{host}/{path}"))
}

fn removed_json(entry: &RemovedWorktree) -> Value {
    let work = entry.work.as_ref();
    let verify = work.and_then(|work| work.verify_after_merge.as_deref());
    let verification_owed = verify.is_some_and(|steps| {
        !steps.trim().is_empty()
            && !matches!(
                work.map(|work| work.state.as_str()),
                Some("verified" | "dropped")
            )
    });
    json!({
        "slug": entry.slug,
        "branch": entry.branch,
        "kind": if is_merged(entry) { "merged" } else { "removed" },
        "pr": entry.extra.get("prNumber").or_else(|| entry.extra.get("pr")),
        "pr_url": entry.extra.get("prUrl").or_else(|| entry.extra.get("pr_url")),
        "title": entry.extra.get("title"),
        "archived_at": entry.removed_at,
        "work_state": work.map(|work| work.state.as_str()),
        "verify_after_merge": verify,
        "verification_owed": verification_owed,
    })
}

fn is_merged(entry: &RemovedWorktree) -> bool {
    wt_store::is_merged_removal(entry)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::{is_merged, removed_json};
    use serde_json::json;
    use wt_store::RemovedWorktree;

    #[test]
    fn removed_history_keeps_kind_and_verification_contract() {
        let row = RemovedWorktree {
            slug: "feature".into(),
            branch: "michael/feature".into(),
            removed_at: "2026-10-09T00:00:00Z".into(),
            work: Some(wt_store::WorkStatusRecord {
                state: "ready".into(),
                at: "2026-10-01T00:00:00Z".into(),
                verify_after_merge: Some("check staging".into()),
                ..serde_json::from_value(json!({"state":"ready","at":"2026-10-01T00:00:00Z"}))
                    .unwrap()
            }),
            automations_paused: None,
            extra: serde_json::Map::from_iter([("prState".into(), json!("MERGED"))]),
        };
        assert!(is_merged(&row));
        let json = removed_json(&row);
        assert_eq!(json["kind"], "merged");
        assert_eq!(json["verify_after_merge"], "check staging");
        assert_eq!(json["verification_owed"], true);
    }
}
