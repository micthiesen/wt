//! Shared lifecycle planning for CLI, TUI and automation callers. A plan is
//! presentation data, not permission to skip the service's locked rechecks.
use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, bail};
use wt_github::{GithubClient, GithubData, GithubOptions};
use wt_lifecycle::{LifecycleService, ServiceConfig};
use wt_vcs::WorktreeRecord;

use crate::{commands::resolve::run_git, context::AppContext};

pub fn service(ctx: &AppContext) -> Result<LifecycleService> {
    let context = ctx.clone();
    let service = LifecycleService::new(
        ServiceConfig::from_config(&ctx.config),
        (*ctx.repository).clone(),
        ctx.processes.clone(),
    )
    .with_before_remove(move |target, cancellation| {
        let context = context.clone();
        async move {
            match crate::host_cleanup::before_remove(
                &context,
                target.slug(),
                Path::new(&target.path),
                &cancellation,
            )
            .await
            {
                Ok(cleanup) => {
                    let mut messages = cleanup.warnings;
                    messages.extend(
                        cleanup
                            .stopped_sessions
                            .into_iter()
                            .map(|session| format!("stopped session {session}")),
                    );
                    messages.extend(
                        cleanup
                            .reaped_listeners
                            .into_iter()
                            .map(|pid| format!("reaped listener pid {pid}")),
                    );
                    messages
                }
                Err(_) if cancellation.is_cancelled() => Vec::new(),
                Err(error) => vec![format!("host cleanup failed: {error:#}")],
            }
        }
    });
    Ok(if ctx.config.dev_server.is_some() {
        service.with_dev_server(crate::dev::service(ctx)?)
    } else {
        service
    })
}

pub async fn resolve_key(ctx: &AppContext, key: &str) -> Result<WorktreeRecord> {
    ctx.repository
        .inventory(&ctx.cancellation)
        .await?
        .into_iter()
        .find(|row| !row.is_main && wt_core::worktree_target_key(&row.target) == key)
        .context("worktree disappeared before the operation could run")
}

pub struct RemovalPlan {
    pub row: WorktreeRecord,
    pub landed: bool,
    pub local_merged: bool,
    pub hazards: Vec<String>,
    pub destroy_stage: bool,
    pub revision: wt_lifecycle::RemovalRevision,
    pub removed_snapshot: wt_store::RemovedWorktree,
}

pub struct RemovalPlans {
    pub rows: Vec<RemovalPlan>,
    pub warning: Option<String>,
}

pub async fn plan(ctx: &AppContext, rows: Vec<WorktreeRecord>) -> Result<RemovalPlans> {
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let branches = rows
        .iter()
        .map(|row| row.target.branch.clone())
        .collect::<Vec<_>>();
    let client = GithubClient::new(
        ctx.processes.clone(),
        ctx.config.paths.main_clone.clone(),
        GithubOptions::from_config(&ctx.config, false),
    );
    let (github, warning) = if std::env::var("WT_GITHUB").as_deref() == Ok("off") {
        (GithubData::default(), None)
    } else {
        match client.fetch_worktrees(&branches, &ctx.cancellation).await {
            Ok(data) => (data, None),
            Err(error) if !ctx.cancellation.is_cancelled() => (
                GithubData::default(),
                Some(format!("GitHub merge evidence unavailable: {error}")),
            ),
            Err(error) => return Err(error.into()),
        }
    };
    plan_with_facts(ctx, rows, &state, &github, warning).await
}

/// Build removal evidence from caller-prepared wtstate and GitHub snapshots.
/// This keeps automation evaluation on the same authoritative safety path as
/// explicit cleanup without launching another GitHub fetch.
pub async fn plan_with_facts(
    ctx: &AppContext,
    rows: Vec<WorktreeRecord>,
    state: &serde_json::Value,
    github: &GithubData,
    warning: Option<String>,
) -> Result<RemovalPlans> {
    let lifecycle = service(ctx)?;
    let mut plans = Vec::with_capacity(rows.len());
    for row in rows {
        if row.is_main {
            continue;
        }
        let path = Path::new(&row.target.path);
        let stored = &state["slugs"][row.target.slug()];
        let head = run_git(ctx, path, ["rev-parse", "--verify", "HEAD"])
            .await?
            .checked("git")?
            .stdout_text()
            .trim()
            .to_owned();
        let own_work = if let Some(base_sha) = stored["baseSha"].as_str() {
            // A missing or corrupted fork anchor is unknown, never evidence of
            // landing. An empty branch must not inherit its parent's landing.
            let own = run_git(
                ctx,
                path,
                ["rev-list", "--count", &format!("{base_sha}..HEAD")],
            )
            .await?;
            own.status.success() && own.stdout_text().trim().parse::<u64>().is_ok_and(|n| n > 0)
        } else {
            false
        };
        let pr_landed = own_work
            && github.prs.get(&row.target.branch).is_some_and(|pr| {
                pr.state == "MERGED" && pr.head_ref_oid.as_deref() == Some(head.as_str())
            });
        let mut local_merged = false;
        if own_work && !pr_landed {
            for reference in [
                format!("refs/remotes/origin/{}", ctx.config.branch.base),
                format!("refs/heads/{}", ctx.config.branch.base),
            ] {
                let exists =
                    run_git(ctx, path, ["rev-parse", "--verify", "--quiet", &reference]).await?;
                if !exists.status.success() {
                    continue;
                }
                let ancestry = run_git(
                    ctx,
                    path,
                    ["merge-base", "--is-ancestor", &head, &reference],
                )
                .await?;
                match ancestry.status.code() {
                    Some(0) => local_merged = true,
                    Some(1) => {}
                    _ => {
                        ancestry.checked("git")?;
                    }
                }
                break;
            }
        }
        let landed = pr_landed || local_merged;
        let mut extra = serde_json::Map::new();
        let pr = github.prs.get(&row.target.branch);
        if let Some(title) = stored["manualTitle"]
            .as_str()
            .filter(|title| !title.trim().is_empty())
            .or_else(|| {
                pr.map(|pr| pr.title.as_str())
                    .filter(|title| !title.trim().is_empty())
            })
        {
            extra.insert("title".into(), serde_json::json!(title));
        }
        if let Some(issue_id) =
            crate::issue_identity::resolve(row.target.slug(), stored["issueId"].as_str())
        {
            extra.insert("issueId".into(), serde_json::json!(issue_id));
        }
        if let Some(github_issue) = stored.get("githubIssue") {
            extra.insert("githubIssue".into(), github_issue.clone());
        }
        if landed {
            extra.insert("gitState".into(), serde_json::json!("merged"));
            extra.insert("landedOnAtRemoval".into(), serde_json::json!("base"));
        }
        if let Some(pr) = pr {
            extra.insert("prNumber".into(), serde_json::json!(pr.number));
            extra.insert("prUrl".into(), serde_json::json!(pr.url));
            extra.insert("prState".into(), serde_json::json!(pr.state));
            if pr.state == "MERGED"
                && let Some(merge_oid) = &pr.merge_commit_oid
            {
                extra.insert("prMergeCommitOid".into(), serde_json::json!(merge_oid));
            }
        }
        let removed_snapshot = wt_store::RemovedWorktree {
            slug: row.target.slug().to_owned(),
            branch: row.target.branch.clone(),
            removed_at: String::new(),
            work: None,
            automations_paused: None,
            extra,
        };
        let revision = lifecycle
            .removal_revision(&row.target, landed, &ctx.cancellation)
            .await?;
        let hazards = revision.hazards.clone();
        let stage_path = path.to_path_buf();
        let stage = row.target.stage.clone();
        let prefix = ctx.config.stage.prefix.clone();
        let default_stage = ctx.config.stage.default_personal.clone();
        let has_sst = ctx.config.sst.is_some();
        let destroy_stage = tokio::task::spawn_blocking(move || {
            has_sst
                && stage != default_stage
                && crate::commands::remove::is_our_stage_deployed(&stage_path, &stage, &prefix)
        })
        .await?;
        plans.push(RemovalPlan {
            row,
            landed,
            local_merged,
            hazards,
            destroy_stage,
            revision,
            removed_snapshot,
        });
    }
    Ok(RemovalPlans {
        rows: plans,
        warning,
    })
}

/// Re-plan the exact confirmed set. Never expand a user's confirmation because
/// another row became eligible while the dialog was open.
pub async fn cleanup_confirmed(
    ctx: &AppContext,
    confirmed: &[wt_tui::RemovalRevision],
) -> Result<String> {
    let live = ctx.repository.inventory(&ctx.cancellation).await?;
    let selected = live
        .into_iter()
        .filter(|row| {
            confirmed.iter().any(|item| {
                item.key == wt_core::worktree_target_key(&row.target)
                    && item.path == row.target.path
                    && item.branch == row.target.branch
                    && item.head == row.head_sha.as_deref().unwrap_or_default()
            })
        })
        .collect();
    let plans = plan(ctx, selected).await?;
    let mut by_key: BTreeMap<_, _> = plans
        .rows
        .into_iter()
        .map(|plan| (wt_core::worktree_target_key(&plan.row.target), plan))
        .collect();
    let mut requests = Vec::new();
    let mut kept = Vec::new();
    for expected in confirmed {
        match by_key.remove(&expected.key) {
            Some(plan)
                if same_revision(&plan.revision, expected)
                    && plan.landed
                    && plan.hazards.is_empty() =>
            {
                requests.push((
                    plan.row,
                    crate::commands::_destroy::DestroyOptions {
                        force: false,
                        delete_branch: true,
                        landed: true,
                        destroy_stage: plan.destroy_stage,
                        expected_revision: Some(plan.revision),
                        removed_snapshot: Some(plan.removed_snapshot),
                    },
                ))
            }
            Some(plan) if !same_revision(&plan.revision, expected) => kept.push(format!(
                "{}: checkout changed after confirmation",
                plan.row.target.slug()
            )),
            Some(plan) => kept.push(format!(
                "{}: {}",
                plan.row.target.slug(),
                if plan.hazards.is_empty() {
                    "landing no longer proven".into()
                } else {
                    plan.hazards.join(", ")
                }
            )),
            None => kept.push(format!(
                "{}: no longer present or identity changed",
                expected.key
            )),
        }
    }
    let mut queued = 0;
    for (request, result) in requests
        .iter()
        .zip(crate::commands::_destroy::start_removals(ctx, &requests).await)
    {
        match result {
            Ok(_) => queued += 1,
            Err(error) => kept.push(format!("{}: {error:#}", request.0.target.slug())),
        }
    }
    if !kept.is_empty() {
        bail!("Queued {queued}; kept {}", kept.join("; "));
    }
    Ok(format!("Cleanup queued for {queued} worktrees"))
}

fn same_revision(plan: &wt_lifecycle::RemovalRevision, expected: &wt_tui::RemovalRevision) -> bool {
    plan.key == expected.key
        && plan.path == expected.path
        && plan.branch == expected.branch
        && plan.head == expected.head
        && plan.digest == expected.digest
        && plan.hazards == expected.hazards
}
