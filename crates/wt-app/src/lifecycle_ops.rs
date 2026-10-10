//! Shared lifecycle planning for CLI, TUI and automation callers. A plan is
//! presentation data, not permission to skip the service's locked rechecks.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

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
    mut warning: Option<String>,
) -> Result<RemovalPlans> {
    let lifecycle = service(ctx)?;
    let mut plans = Vec::with_capacity(rows.len());
    let mut published_bases = BTreeMap::new();
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
        let local_merged = own_work
            && !pr_landed
            && published_base_contains(ctx, path, &head, &mut published_bases, &mut warning)
                .await?;
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
        }
        // A branch may have advanced after an older PR merged. Persist the
        // current revision's verdict so historical PR metadata cannot turn
        // those unlanded commits into a claim that all work landed.
        extra.insert(
            "landedOnAtRemoval".into(),
            serde_json::json!(if landed { "base" } else { "unlanded" }),
        );
        if let Some(pr) = pr {
            extra.insert("prNumber".into(), serde_json::json!(pr.number));
            extra.insert("prUrl".into(), serde_json::json!(pr.url));
            extra.insert("prState".into(), serde_json::json!(pr.state));
            if pr_landed && let Some(merge_oid) = &pr.merge_commit_oid {
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
        let mut revision = lifecycle
            .removal_revision(&row.target, landed, &ctx.cancellation)
            .await?;
        if local_merged {
            let repository = match ctx.config.backend.kind {
                wt_config::BackendKind::GitWorktree => ctx.config.paths.main_clone.as_path(),
                wt_config::BackendKind::Rift => path,
            };
            revision.published_base = published_bases
                .get(repository)
                .and_then(Option::as_ref)
                .map(|tip| {
                    Box::new((
                        format!("refs/heads/{}", ctx.config.branch.base),
                        tip.clone(),
                    ))
                });
        }
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

/// A cached ancestry match only nominates a candidate. Confirm that the origin
/// still advertises a containing base tip before it can waive the unpushed-work
/// guard. Query once per object store in a batch, without changing any checkout,
/// fetching every branch, or running keep-fresh/install hooks.
async fn published_base_contains(
    ctx: &AppContext,
    path: &Path,
    head: &str,
    published: &mut BTreeMap<PathBuf, Option<String>>,
    warning: &mut Option<String>,
) -> Result<bool> {
    let reference = format!("refs/remotes/origin/{}", ctx.config.branch.base);
    let cached = run_git(ctx, path, ["merge-base", "--is-ancestor", head, &reference]).await?;
    if !cached.status.success() {
        return Ok(false);
    }
    let repository = match ctx.config.backend.kind {
        wt_config::BackendKind::GitWorktree => ctx.config.paths.main_clone.as_path(),
        wt_config::BackendKind::Rift => path,
    };
    if !published.contains_key(repository) {
        let remote_ref = format!("refs/heads/{}", ctx.config.branch.base);
        let result = run_git(
            ctx,
            repository,
            ["ls-remote", "--exit-code", "--refs", "origin", &remote_ref],
        )
        .await?;
        let tip = if result.status.success() {
            result.stdout_text().lines().find_map(|line| {
                let (oid, name) = line.split_once('\t')?;
                (name == remote_ref
                    && matches!(oid.len(), 40 | 64)
                    && oid.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then(|| oid.to_owned())
            })
        } else {
            None
        };
        if tip.is_none() {
            let detail = format!(
                "Current origin/{} landing proof unavailable; cached refs are not sufficient",
                ctx.config.branch.base
            );
            *warning = Some(match warning.take() {
                Some(previous) => format!("{previous}; {detail}"),
                None => detail,
            });
        }
        published.insert(repository.to_path_buf(), tip);
    }
    let Some(tip) = published.get(repository).and_then(Option::as_deref) else {
        return Ok(false);
    };
    // A newly advertised object may not have been fetched yet. Missing objects
    // are unknown, not a reason to trust the older local base.
    Ok(
        run_git(ctx, path, ["merge-base", "--is-ancestor", head, tip])
            .await?
            .status
            .success(),
    )
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
        && plan.published_base == expected.published_base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::CommandFixture;

    async fn git(ctx: &AppContext, path: &Path, args: &[&str]) -> String {
        run_git(ctx, path, args.iter().copied())
            .await
            .unwrap()
            .checked("fixture git")
            .unwrap()
            .stdout_text()
            .trim()
            .to_owned()
    }

    #[tokio::test]
    async fn removal_landing_requires_the_current_published_base() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let path = &ctx.cwd;
        let origin = fixture._root.path().join("origin.git");
        git(ctx, path, &["init", "--bare", origin.to_str().unwrap()]).await;
        git(
            ctx,
            path,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        )
        .await;
        let base = git(ctx, path, &["rev-parse", "HEAD"]).await;
        let anchor = base.clone();
        ctx.database
            .call(move |store| {
                store.set_slug_base("one", Some(("main", Some(&anchor))))?;
                Ok(())
            })
            .await
            .unwrap();
        std::fs::write(path.join("work.txt"), "unpublished branch work\n").unwrap();
        git(ctx, path, &["add", "work.txt"]).await;
        git(ctx, path, &["commit", "-m", "feature work"]).await;
        let head = git(ctx, path, &["rev-parse", "HEAD"]).await;
        git(ctx, path, &["push", "origin", "HEAD:refs/heads/main"]).await;
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let row = ctx
            .repository
            .inventory(&ctx.cancellation)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.target.slug() == "one")
            .unwrap();
        let plan = plan_with_facts(ctx, vec![row.clone()], &state, &GithubData::default(), None)
            .await
            .unwrap();
        assert!(plan.rows[0].local_merged);
        assert!(plan.rows[0].hazards.is_empty());
        assert!(wt_store::is_merged_removal(&plan.rows[0].removed_snapshot));
        // Detached workers deserialize this revision after the planning process
        // exits. Do not reduce the published-base witness to a `landed` bool.
        let delayed: wt_lifecycle::RemovalRevision =
            serde_json::from_slice(&serde_json::to_vec(&plan.rows[0].revision).unwrap()).unwrap();
        assert_eq!(delayed.published_base.as_ref().unwrap().1, head);

        // A linked checkout can override origin independently. Its alternate
        // remote retaining the commit must not validate the main clone's proof.
        let alternate = fixture._root.path().join("alternate-origin.git");
        git(
            ctx,
            path,
            &[
                "clone",
                "--bare",
                origin.to_str().unwrap(),
                alternate.to_str().unwrap(),
            ],
        )
        .await;
        git(ctx, path, &["config", "extensions.worktreeConfig", "true"]).await;
        git(
            ctx,
            path,
            &[
                "config",
                "--worktree",
                &format!("url.{}.insteadOf", alternate.display()),
                origin.to_str().unwrap(),
            ],
        )
        .await;

        // Simulate a remote force-push without updating this checkout's cached
        // origin/main. The old cache still contains HEAD, but origin does not.
        git(ctx, &origin, &["update-ref", "refs/heads/main", &base]).await;
        assert_eq!(git(ctx, path, &["rev-parse", "origin/main"]).await, head);
        assert_eq!(
            git(ctx, path, &["ls-remote", "origin", "refs/heads/main"]).await,
            format!("{head}\trefs/heads/main")
        );
        assert_eq!(
            git(
                ctx,
                &ctx.config.paths.main_clone,
                &["ls-remote", "origin", "refs/heads/main"]
            )
            .await,
            format!("{base}\trefs/heads/main")
        );
        let refused = service(ctx)
            .unwrap()
            .remove_with_revision(
                &row.target,
                wt_lifecycle::RemoveOptions {
                    landed: true,
                    delete_branch: true,
                    ..Default::default()
                },
                &delayed,
                &ctx.cancellation,
            )
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("published base changed"),
            "{refused}"
        );
        assert!(path.exists());
        assert_eq!(
            git(ctx, path, &["rev-parse", "refs/heads/feature/one"]).await,
            head
        );
        let plan = plan_with_facts(ctx, vec![row.clone()], &state, &GithubData::default(), None)
            .await
            .unwrap();
        assert!(!plan.rows[0].landed);
        assert!(
            plan.rows[0]
                .hazards
                .iter()
                .any(|hazard| hazard.contains("unpushed"))
        );
        let mut github = GithubData::default();
        github.prs.insert(
            row.target.branch.clone(),
            serde_json::from_value(serde_json::json!({
                "number": 42, "url": "https://example.invalid/pull/42",
                "headRefName": row.target.branch, "headRefOid": base,
                "baseRefName": "main", "mergeCommitOid": base,
                "title": "Previous work", "isDraft": false, "state": "MERGED",
                "checks": "pass", "failedChecks": [], "review": "none",
                "reviewRequests": 0, "requestedReviewers": [], "suggestedReviewers": [],
                "comments": [], "unresolvedThreads": 0, "unresolvedThreadsTotal": 0
            }))
            .unwrap(),
        );
        let reused = plan_with_facts(ctx, vec![row.clone()], &state, &github, None)
            .await
            .unwrap();
        assert!(!reused.rows[0].landed);
        let history = &reused.rows[0].removed_snapshot;
        assert_eq!(history.extra["landedOnAtRemoval"], "unlanded");
        assert_eq!(history.extra["prState"], "MERGED");
        assert!(!history.extra.contains_key("prMergeCommitOid"));
        assert!(!wt_store::is_merged_removal(history));

        // Neither a deleted remote base nor a matching local main is a proof.
        git(ctx, &origin, &["update-ref", "-d", "refs/heads/main"]).await;
        let plan = plan_with_facts(ctx, vec![row.clone()], &state, &GithubData::default(), None)
            .await
            .unwrap();
        assert!(!plan.rows[0].landed);
        assert!(plan.warning.is_some());
        git(ctx, path, &["update-ref", "-d", "refs/remotes/origin/main"]).await;
        git(ctx, path, &["update-ref", "refs/heads/main", &head]).await;
        let plan = plan_with_facts(ctx, vec![row], &state, &GithubData::default(), None)
            .await
            .unwrap();
        assert!(!plan.rows[0].landed);
        fixture.close().await.unwrap();
    }
}
