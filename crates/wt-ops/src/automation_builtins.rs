//! Host-local implementations of the automation engine's built-in runs.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::{collections::BTreeSet, path::Path};
use wt_automations::AutomationFire;
use wt_core::{ChainMember, build_stack_index};
use wt_github::{GithubClient, GithubOptions};
use wt_platform::process::{CommandSpec, ProcessRunner};
use wt_stack::{RestackOptions, RestackOutcome, StackConfig, StackService, StateConfig};
use wt_store::RepositoryIdentity;

use crate::lifecycle_ops;
use wt_app_core::context::AppContext;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuiltinExecution {
    /// Positive proof that no effect began; the caller may release the fire.
    Retry {
        reason: String,
    },
    /// Nothing was written, and the run must be checked again later even
    /// after its worktree is gone: the engine requeues the same frozen fire.
    Defer {
        reason: String,
        /// What a retry needs that may be gone by then, by frozen-var name.
        frozen: std::collections::BTreeMap<String, String>,
    },
    Completed {
        message: String,
    },
    Failed {
        message: String,
    },
}

pub async fn execute(ctx: &AppContext, fire: &AutomationFire) -> BuiltinExecution {
    match execute_inner(ctx, fire).await {
        Ok(result) => result,
        Err(error) => BuiltinExecution::Failed {
            message: format!("{}: {error:#}", fire.rule.run),
        },
    }
}

async fn execute_inner(ctx: &AppContext, fire: &AutomationFire) -> Result<BuiltinExecution> {
    match fire.rule.run.as_str() {
        "builtin:clean" => clean(ctx, fire).await,
        "builtin:restack" => restack(ctx, fire).await,
        "builtin:notify" => notify(ctx, fire).await,
        "builtin:close-issue" => close_issue(ctx, fire).await,
        "builtin:delete-branch" => delete_branch(ctx, fire).await,
        run => Ok(BuiltinExecution::Failed {
            message: format!("unknown automation builtin {run:?}"),
        }),
    }
}

async fn clean(ctx: &AppContext, fire: &AutomationFire) -> Result<BuiltinExecution> {
    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    let Some(row) = rows
        .into_iter()
        .find(|row| !row.is_main && row.target.slug() == fire.slug)
    else {
        return Ok(BuiltinExecution::Completed {
            message: format!("{} is already absent", fire.slug),
        });
    };
    let plan = lifecycle_ops::plan(ctx, vec![row]).await?;
    let Some(plan) = plan.rows.into_iter().next() else {
        return Ok(BuiltinExecution::Completed {
            message: format!("{} is no longer cleanable", fire.slug),
        });
    };
    if !plan.landed || !plan.hazards.is_empty() {
        return Ok(BuiltinExecution::Completed {
            message: format!(
                "kept {}: {}",
                fire.slug,
                if plan.hazards.is_empty() {
                    "landing is no longer proven".to_owned()
                } else {
                    plan.hazards.join(", ")
                }
            ),
        });
    }
    let revision = wt_tui::RemovalRevision {
        key: plan.revision.key,
        path: plan.revision.path,
        branch: plan.revision.branch,
        head: plan.revision.head,
        digest: plan.revision.digest,
        hazards: plan.revision.hazards,
        published_base: plan.revision.published_base,
    };
    let message = lifecycle_ops::cleanup_confirmed(ctx, &[revision]).await?;
    Ok(BuiltinExecution::Completed { message })
}

async fn restack(ctx: &AppContext, fire: &AutomationFire) -> Result<BuiltinExecution> {
    let service = stack_service(ctx);
    let stack_id = fire.stack_id.as_deref().unwrap_or(fire.slug.as_str());
    if service.is_busy(stack_id, &ctx.cancellation).await? {
        return Ok(BuiltinExecution::Retry {
            reason: format!("stack {stack_id} has a lifecycle operation in progress"),
        });
    }

    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let branch = rows
        .iter()
        .find(|row| !row.is_main && row.target.slug() == fire.slug)
        .map(|row| row.target.branch.as_str())
        .unwrap_or(stack_id)
        .to_owned();
    let members = rows
        .iter()
        .filter(|row| !row.is_main && !row.target.branch.is_empty())
        .map(|row| {
            ChainMember::new(
                row.target.slug(),
                row.target.branch.clone(),
                state["slugs"][row.target.slug()]["baseBranch"]
                    .as_str()
                    .map(str::to_owned),
            )
        })
        .collect::<Vec<_>>();
    let index = build_stack_index(&members, &ctx.config.branch.base);
    let member_slugs = index
        .by_branch
        .get(stack_id)
        .and_then(|entry| index.layouts.get(entry.layout_index))
        .filter(|layout| layout.stack_id == stack_id)
        .map(|layout| {
            layout
                .nodes
                .iter()
                .map(|node| node.slug.clone())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let member_branches = members
        .iter()
        .filter(|member| member_slugs.contains(&member.slug))
        .map(|member| member.branch.as_str())
        .collect::<BTreeSet<_>>();
    let mut cleanup_slugs = member_slugs.clone();
    for member in members
        .iter()
        .filter(|member| member_slugs.contains(&member.slug))
    {
        let Some(parent) = state["slugs"][&member.slug]["baseBranch"].as_str() else {
            continue;
        };
        if member_branches.contains(parent) || parent == ctx.config.branch.base {
            continue;
        }
        if let Some(parent_row) = rows
            .iter()
            .find(|row| !row.is_main && row.target.branch == parent)
        {
            cleanup_slugs.insert(parent_row.target.slug().to_owned());
        }
    }
    let paused_stacks = state["pausedStacks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect::<BTreeSet<_>>();
    if !paused_stacks.contains(stack_id) {
        let eligible = rows
            .into_iter()
            .filter(|row| cleanup_slugs.contains(row.target.slug()))
            .filter(|row| state["slugs"][row.target.slug()]["automationsPaused"] != true)
            .collect::<Vec<_>>();
        if !eligible.is_empty() {
            let plans = lifecycle_ops::plan(ctx, eligible).await?;
            let revisions = plans
                .rows
                .into_iter()
                .filter(|plan| plan.landed && plan.hazards.is_empty())
                .map(|plan| wt_tui::RemovalRevision {
                    key: plan.revision.key,
                    path: plan.revision.path,
                    branch: plan.revision.branch,
                    head: plan.revision.head,
                    digest: plan.revision.digest,
                    hazards: plan.revision.hazards,
                    published_base: plan.revision.published_base,
                })
                .collect::<Vec<_>>();
            if !revisions.is_empty() {
                lifecycle_ops::cleanup_confirmed(ctx, &revisions).await?;
            }
        }
    }

    let outcome = service
        .restack(
            &branch,
            RestackOptions::default(),
            &ctx.cancellation,
            &mut |_| {},
        )
        .await;
    match outcome {
        Err(wt_stack::StackError::Busy) => Ok(BuiltinExecution::Failed {
            message: "restack became busy after pre-clean; run `wt restack` once it is free".into(),
        }),
        Err(error) => Err(error.into()),
        Ok(RestackOutcome::Complete { replayed, total }) => Ok(BuiltinExecution::Completed {
            message: format!("restacked {branch} ({replayed}/{total} worktrees)"),
        }),
        Ok(RestackOutcome::Conflict { branch, error, .. }) => Ok(BuiltinExecution::Failed {
            message: format!("restack conflict on {branch}: {error}"),
        }),
        Ok(RestackOutcome::Refused { error }) => Ok(BuiltinExecution::Failed { message: error }),
    }
}

async fn notify(ctx: &AppContext, fire: &AutomationFire) -> Result<BuiltinExecution> {
    let title = format!("wt · {}", fire.slug);
    #[cfg(target_os = "macos")]
    {
        let output = ctx
            .processes
            .run(
                CommandSpec::new("osascript").args([
                    "-e",
                    "on run argv",
                    "-e",
                    "display notification (item 2 of argv) with title (item 1 of argv)",
                    "-e",
                    "end run",
                    "--",
                    &title,
                    &fire.detail,
                ]),
                &ctx.cancellation,
            )
            .await?;
        output.checked("osascript")?;
    }
    #[cfg(target_os = "linux")]
    {
        let output = ctx
            .processes
            .run(
                // A detail starting with `-` must not parse as an option.
                CommandSpec::new("notify-send").args(["--", &title, &fire.detail]),
                &ctx.cancellation,
            )
            .await?;
        output.checked("notify-send")?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        tracing::warn!(slug = %fire.slug, "automation notification skipped: desktop notifications are unsupported on this platform");
        return Ok(BuiltinExecution::Completed {
            message: format!(
                "notification skipped for {}: unsupported platform",
                fire.slug
            ),
        });
    }
    Ok(BuiltinExecution::Completed {
        message: format!("notification sent for {}", fire.slug),
    })
}

async fn close_issue(ctx: &AppContext, fire: &AutomationFire) -> Result<BuiltinExecution> {
    let Some(issue) = fire.close_issue else {
        return Ok(BuiltinExecution::Completed {
            message: "fire carried no issue number; nothing to close".into(),
        });
    };
    let client = github_client(ctx);
    let result = client.close_issue(issue, &ctx.cancellation).await;
    if result.ok {
        attention(
            ctx,
            "info",
            &format!("auto {}: closed issue #{issue}", fire.rule.id),
        )
        .await?;
    } else {
        tracing::warn!(rule = %fire.rule.id, issue, error = ?result.error, "automation close issue declined");
    }
    Ok(BuiltinExecution::Completed {
        message: if result.ok {
            format!("closed issue #{issue}")
        } else {
            format!(
                "issue #{issue} was not closed: {}",
                result.error.unwrap_or_default()
            )
        },
    })
}

#[derive(Deserialize)]
struct PullRequestRef {
    number: u64,
    state: String,
    #[serde(rename = "headRefOid", default)]
    head_oid: Option<String>,
}

async fn delete_branch(ctx: &AppContext, fire: &AutomationFire) -> Result<BuiltinExecution> {
    let Some(branch) = fire.delete_branch.as_deref() else {
        return Ok(BuiltinExecution::Completed {
            message: "fire carried no branch; nothing to delete".into(),
        });
    };
    if branch == ctx.config.branch.base {
        return Ok(BuiltinExecution::Failed {
            message: format!("refusing to delete trunk branch {branch}"),
        });
    }
    let output = ctx
        .processes
        .run(
            CommandSpec::new("gh")
                .args([
                    "pr",
                    "list",
                    "--state",
                    "all",
                    "--head",
                    branch,
                    "--json",
                    "number,state,headRefOid",
                ])
                .cwd(&ctx.config.paths.main_clone),
            &ctx.cancellation,
        )
        .await
        .map_err(anyhow::Error::from)
        .and_then(|output| Ok(output.checked("gh pr list")?))
        .and_then(|output| {
            serde_json::from_slice::<Vec<PullRequestRef>>(&output.stdout)
                .context("parse successful `gh pr list` confirmation")
        });
    // An unanswered read proves nothing either way: check again later
    // rather than drop the delete.
    let prs = match output {
        Ok(prs) => prs,
        Err(error) => {
            return Ok(BuiltinExecution::Defer {
                reason: format!("cannot confirm the merge of {branch}: {error:#}"),
                frozen: Default::default(),
            });
        }
    };
    let claimed = fire.delete_branch_pr;
    // A retry may run after the worktree's cleanup deleted the local branch,
    // so a tip proven on an earlier attempt is carried with the fire.
    let carried = fire
        .frozen_vars
        .as_ref()
        .and_then(|vars| vars.get(DELETE_TIP_VAR))
        .cloned();
    let proven_tip = match (claimed, carried) {
        (Some(_), _) => None,
        (None, Some(tip)) => Some(tip),
        (None, None) => local_branch_tip(ctx, branch).await,
    };
    let confirmed = confirms_delete(&prs, claimed, proven_tip.as_deref());
    if !confirmed {
        let reason = if let Some(number) = claimed {
            format!("fire claimed PR #{number}, but no matching merged PR was returned")
        } else {
            format!(
                "{} PR(s) exist and none merged at the landed tip",
                prs.len()
            )
        };
        attention(
            ctx,
            "warn",
            &format!("auto {}: NOT deleting {branch} - {reason}", fire.rule.id),
        )
        .await?;
        return Ok(BuiltinExecution::Completed {
            message: format!("declined to delete {branch}: {reason}"),
        });
    }
    // Delete only the commit that was proven merged: the merged PR's head,
    // or without a PR the local branch tip that Git proved landed. Commits
    // pushed to the branch since then must survive.
    let expected = match claimed {
        Some(number) => prs
            .iter()
            .find(|pr| pr.number == number)
            .and_then(|pr| pr.head_oid.clone()),
        None => proven_tip,
    };
    let Some(expected) = expected.filter(|sha| !sha.trim().is_empty()) else {
        let reason = "the merged head commit is unknown";
        attention(
            ctx,
            "warn",
            &format!("auto {}: NOT deleting {branch} - {reason}", fire.rule.id),
        )
        .await?;
        return Ok(BuiltinExecution::Completed {
            message: format!("declined to delete {branch}: {reason}"),
        });
    };
    let frozen = std::collections::BTreeMap::from([(DELETE_TIP_VAR.to_owned(), expected.clone())]);
    let local_children = match stacked_worktrees(ctx, branch, &fire.slug).await {
        Ok(children) => children,
        Err(error) => {
            return Ok(BuiltinExecution::Defer {
                reason: format!("cannot read which worktrees stack on {branch}: {error:#}"),
                frozen,
            });
        }
    };
    let outcome = match delete_remote_branch_guarded(
        &ctx.processes,
        &github_client(ctx),
        &ctx.config.paths.main_clone,
        branch,
        &expected,
        &local_children,
        &ctx.cancellation,
    )
    .await?
    {
        GuardedDelete::Deferred(reason) => {
            return Ok(BuiltinExecution::Defer { reason, frozen });
        }
        GuardedDelete::Attempted(outcome) => outcome,
    };
    match outcome {
        LeasedDelete::Deleted => {
            attention(
                ctx,
                "info",
                &format!("auto {}: deleted remote branch {branch}", fire.rule.id),
            )
            .await?;
            Ok(BuiltinExecution::Completed {
                message: format!("deleted remote branch {branch}"),
            })
        }
        LeasedDelete::Refused(error) | LeasedDelete::Unreachable(error) => {
            tracing::warn!(rule = %fire.rule.id, branch, %error, "automation delete branch declined");
            attention(
                ctx,
                "warn",
                &format!(
                    "auto {}: NOT deleting {branch} (merged head {}): {error}",
                    fire.rule.id,
                    short_sha(&expected),
                ),
            )
            .await?;
            Ok(BuiltinExecution::Completed {
                message: format!("remote branch {branch} was not deleted: {error}"),
            })
        }
    }
}

/// Frozen var naming the merged commit a deferred delete may remove.
const DELETE_TIP_VAR: &str = "delete_branch_tip";

#[derive(Debug, PartialEq, Eq)]
enum GuardedDelete {
    /// Something may still be based on the branch. Nothing was pushed.
    Deferred(String),
    Attempted(LeasedDelete),
}

/// The one path that deletes a branch on origin. GitHub CLOSES every open PR
/// whose base ref is deleted by push or API (it retargets them only under its
/// own "automatically delete head branches" setting), and such a PR cannot be
/// reopened. So the delete waits until no open PR and no live worktree is
/// based on the branch; a restack moves them to the trunk, then it proceeds.
async fn delete_remote_branch_guarded(
    processes: &ProcessRunner,
    github: &GithubClient,
    repository: &Path,
    branch: &str,
    expected: &str,
    local_children: &[String],
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<GuardedDelete> {
    if !local_children.is_empty() {
        return Ok(GuardedDelete::Deferred(format!(
            "worktree(s) {} still stack on {branch}",
            local_children.join(", ")
        )));
    }
    match github
        .open_pull_requests_based_on(branch, cancellation)
        .await
    {
        Ok(children) if children.is_empty() => {}
        Ok(children) => {
            let numbers = children
                .iter()
                .map(|number| format!("#{number}"))
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(GuardedDelete::Deferred(format!(
                "open PR(s) {numbers} still target {branch}; deleting it would close them"
            )));
        }
        Err(error) => {
            return Ok(GuardedDelete::Deferred(format!(
                "cannot list PRs based on {branch}: {error}"
            )));
        }
    }
    let mut outcome =
        delete_remote_branch_at(processes, repository, branch, expected, cancellation).await?;
    if let LeasedDelete::Unreachable(push_error) = &outcome {
        // git could not reach origin (commonly no git credentials when only
        // gh is authenticated). Fall back to the API with the same head check.
        let result = github
            .delete_remote_branch_at(branch, expected, cancellation)
            .await;
        outcome = if result.ok {
            LeasedDelete::Deleted
        } else {
            LeasedDelete::Refused(format!(
                "{}; git push: {push_error}",
                result
                    .error
                    .as_deref()
                    .unwrap_or("GitHub API delete failed")
            ))
        };
    }
    Ok(GuardedDelete::Attempted(outcome))
}

/// Live worktrees other than `own_slug` whose recorded fork base is `branch`:
/// a restack has yet to move them to the trunk.
async fn stacked_worktrees(ctx: &AppContext, branch: &str, own_slug: &str) -> Result<Vec<String>> {
    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    Ok(rows
        .iter()
        .filter(|row| !row.is_main && row.target.slug() != own_slug)
        .filter(|row| state["slugs"][row.target.slug()]["baseBranch"].as_str() == Some(branch))
        .map(|row| row.target.slug().to_owned())
        .collect())
}

#[derive(Debug, PartialEq, Eq)]
enum LeasedDelete {
    Deleted,
    /// The remote refused: its tip is not `expected` (commits were pushed
    /// after the merge, or it is already gone).
    Refused(String),
    /// The push never got a per-ref answer from the remote (credentials,
    /// network, hooks), so nothing about the remote tip is known.
    Unreachable(String),
}

/// Delete `origin`'s `branch` only while it still points at `expected`. The
/// lease travels with the push, so the remote compares and deletes in one
/// step; a tip that moved meanwhile is refused, never discarded.
async fn delete_remote_branch_at(
    processes: &ProcessRunner,
    repository: &Path,
    branch: &str,
    expected: &str,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<LeasedDelete> {
    let reference = format!("refs/heads/{branch}");
    let mut spec = CommandSpec::new("git")
        .args([
            "push".to_owned(),
            "--porcelain".to_owned(),
            format!("--force-with-lease={reference}:{expected}"),
            "origin".to_owned(),
            format!(":{reference}"),
        ])
        .cwd(repository);
    spec.env = vec![
        ("GIT_TERMINAL_PROMPT".into(), Some("0".into())),
        ("LC_ALL".into(), Some("C".into())),
    ];
    let output = processes.run(spec, cancellation).await?;
    if output.status.success() {
        return Ok(LeasedDelete::Deleted);
    }
    // `--porcelain` prints a `!` status line only when the remote answered
    // for this ref; anything else is a transport or credential failure.
    let stdout = output.stdout_text();
    if let Some(line) = stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('!'))
    {
        return Ok(LeasedDelete::Refused(wt_core::sanitize_terminal_text(line)));
    }
    let stderr = output.stderr_text();
    let detail = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("git push failed");
    Ok(LeasedDelete::Unreachable(wt_core::sanitize_terminal_text(
        detail,
    )))
}

async fn local_branch_tip(ctx: &AppContext, branch: &str) -> Option<String> {
    let commit = format!("refs/heads/{branch}^{{commit}}");
    let output = ctx
        .processes
        .run(
            CommandSpec::new("git")
                .args(["rev-parse", "--verify", "--quiet", commit.as_str()])
                .cwd(&ctx.config.paths.main_clone),
            &ctx.cancellation,
        )
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| output.stdout_text().trim().to_owned())
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn github_client(ctx: &AppContext) -> GithubClient {
    GithubClient::new(
        ctx.processes.clone(),
        ctx.config.paths.main_clone.clone(),
        GithubOptions::from_config(&ctx.config, false),
    )
}

fn stack_service(ctx: &AppContext) -> StackService {
    let config = StackConfig {
        main_clone: ctx.config.paths.main_clone.clone(),
        lock_dir: ctx.config.paths.lock_dir.clone(),
        trunk_branch: ctx.config.branch.base.clone(),
        fetch_options: crate::origin::options(ctx),
        state: StateConfig {
            path: ctx.config.paths.state_db.clone(),
            identity: RepositoryIdentity::new(
                ctx.config.repo_id.clone(),
                ctx.config.repo_path.to_string_lossy(),
            ),
        },
    };
    StackService::new(
        config,
        (*ctx.repository).clone(),
        ctx.processes.clone(),
        github_client(ctx),
    )
}

/// The report spool is appended under an exclusive file lock, so the write
/// runs on the blocking pool rather than an executor thread.
async fn attention(ctx: &AppContext, level: &str, text: &str) -> Result<()> {
    let level = match level {
        "warn" => crate::commands::manager::ReportLevel::Warn,
        _ => crate::commands::manager::ReportLevel::Info,
    };
    let path = ctx.config.paths.cache_root.join("manager/reports.jsonl");
    let text = text.to_owned();
    tokio::task::spawn_blocking(move || {
        crate::commands::manager::append_report(&path, level, &text)
    })
    .await
    .context("append automation report")?
}

/// With no PR claimed, the row saw no PR when it landed, which can mean the
/// board had not read that branch yet. A PR merged at the tip Git proved
/// landed confirms the same commit, so it does not block the delete.
fn confirms_delete(prs: &[PullRequestRef], claimed: Option<u64>, tip: Option<&str>) -> bool {
    if prs.iter().any(|pr| pr.state == "OPEN") {
        return false;
    }
    match claimed {
        Some(number) => prs
            .iter()
            .any(|pr| pr.number == number && pr.state == "MERGED"),
        None => {
            prs.is_empty()
                || prs.iter().any(|pr| {
                    pr.state == "MERGED" && tip.is_some() && pr.head_oid.as_deref() == tip
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GuardedDelete, LeasedDelete, PullRequestRef, confirms_delete, delete_remote_branch_at,
        delete_remote_branch_guarded,
    };
    use std::{path::Path, process::Command};
    use wt_platform::process::ProcessRunner;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn commit(clone: &Path, message: &str) -> String {
        git(
            clone,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                message,
            ],
        );
        git(clone, &["rev-parse", "HEAD"])
    }

    /// Commits pushed to the branch after its PR merged are never deleted:
    /// the push carries the merged head as a lease.
    #[tokio::test]
    async fn branch_deletion_requires_the_remote_tip_to_be_the_merged_head() {
        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote.git");
        let clone = root.path().join("clone");
        git(
            root.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        git(root.path(), &["init", "-q", clone.to_str().unwrap()]);
        git(
            &clone,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&clone, &["checkout", "-qb", "feature"]);
        let merged = commit(&clone, "merged work");
        git(&clone, &["push", "-q", "origin", "feature"]);
        let later = commit(&clone, "pushed after the merge");
        git(&clone, &["push", "-q", "origin", "feature"]);

        let processes = ProcessRunner::default();
        let cancel = tokio_util::sync::CancellationToken::new();
        let refused = delete_remote_branch_at(&processes, &clone, "feature", &merged, &cancel)
            .await
            .unwrap();
        assert!(matches!(refused, LeasedDelete::Refused(_)), "{refused:?}");
        assert_eq!(git(&remote, &["rev-parse", "refs/heads/feature"]), later);

        let deleted = delete_remote_branch_at(&processes, &clone, "feature", &later, &cancel)
            .await
            .unwrap();
        assert_eq!(deleted, LeasedDelete::Deleted);
        assert_eq!(git(&remote, &["for-each-ref", "refs/heads/feature"]), "");
        // Already gone: refused, not an error.
        let gone = delete_remote_branch_at(&processes, &clone, "feature", &later, &cancel)
            .await
            .unwrap();
        assert!(matches!(gone, LeasedDelete::Refused(_)), "{gone:?}");

        // An unreachable origin is not a refusal: the caller may fall back to
        // the GitHub API, which repeats the head check.
        git(
            &clone,
            &[
                "remote",
                "set-url",
                "origin",
                root.path().join("missing.git").to_str().unwrap(),
            ],
        );
        let unreachable = delete_remote_branch_at(&processes, &clone, "feature", &later, &cancel)
            .await
            .unwrap();
        assert!(
            matches!(unreachable, LeasedDelete::Unreachable(_)),
            "{unreachable:?}"
        );
    }

    /// GitHub closes, for good, every open PR whose base branch is deleted by
    /// push or API. The delete waits while a PR or a worktree is still based
    /// on the branch, or while that cannot be ruled out, and goes ahead once
    /// a restack has moved them.
    #[tokio::test]
    async fn branch_deletion_waits_for_everything_stacked_on_it() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote.git");
        let clone = root.path().join("clone");
        git(
            root.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        git(root.path(), &["init", "-q", clone.to_str().unwrap()]);
        git(
            &clone,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&clone, &["checkout", "-qb", "parent"]);
        let merged = commit(&clone, "merged work");
        git(&clone, &["push", "-q", "origin", "parent"]);

        // `children` holds gh's answer for `pr list --base parent`; without
        // it gh fails the way an outage or rate limit does.
        let children = root.path().join("children");
        let args = root.path().join("args");
        let gh = root.path().join("gh");
        std::fs::write(
            &gh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat '{}' || {{ echo 'HTTP 502' >&2; exit 1; }}\n",
                args.display(),
                children.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let github = wt_github::GithubClient::new(
            ProcessRunner::default(),
            clone.clone(),
            wt_github::GithubOptions {
                base_branch: "main".into(),
                reviewers_enabled: false,
                ignored_checks: vec![],
                review_bot_name: String::new(),
                review_bot_login: String::new(),
                review_bot_contexts: vec![],
                review_bot_mode: wt_config::ReviewBotMode::Threads,
                review_bot_summary_markers: vec![],
                review_bot_pending_marker: None,
                ignored_review_repositories: vec![],
                default_reviewer: None,
                repo_has_ci: false,
            },
        )
        .with_gh_program(&gh);
        let processes = ProcessRunner::default();
        let cancel = tokio_util::sync::CancellationToken::new();
        let attempt = |local: Vec<String>| {
            let (processes, github, clone, merged, cancel) = (
                processes.clone(),
                github.clone(),
                clone.clone(),
                merged.clone(),
                cancel.clone(),
            );
            async move {
                delete_remote_branch_guarded(
                    &processes, &github, &clone, "parent", &merged, &local, &cancel,
                )
                .await
                .unwrap()
            }
        };
        let remote_tip = || {
            git(
                &remote,
                &[
                    "for-each-ref",
                    "--format=%(objectname)",
                    "refs/heads/parent",
                ],
            )
        };

        std::fs::write(&children, r#"[{"number":3248}]"#).unwrap();
        let open_child = attempt(vec![]).await;
        assert!(
            matches!(&open_child, GuardedDelete::Deferred(reason) if reason.contains("#3248")),
            "{open_child:?}"
        );
        assert_eq!(remote_tip(), merged);
        let asked = std::fs::read_to_string(&args).unwrap();
        assert!(asked.lines().any(|line| line == "--base"), "{asked}");
        assert!(asked.lines().any(|line| line == "parent"), "{asked}");

        std::fs::remove_file(&children).unwrap();
        let unanswered = attempt(vec![]).await;
        assert!(
            matches!(unanswered, GuardedDelete::Deferred(_)),
            "{unanswered:?}"
        );
        assert_eq!(remote_tip(), merged);

        std::fs::write(&children, "[]").unwrap();
        let local_child = attempt(vec!["child".into()]).await;
        assert!(
            matches!(local_child, GuardedDelete::Deferred(_)),
            "{local_child:?}"
        );
        assert_eq!(remote_tip(), merged);

        // The restack retargeted the child PR and rebased the worktree.
        let retargeted = attempt(vec![]).await;
        assert_eq!(retargeted, GuardedDelete::Attempted(LeasedDelete::Deleted));
        assert_eq!(remote_tip(), "");
    }

    #[test]
    fn delete_requires_exact_merged_pr_or_successful_empty_list() {
        let merged = [PullRequestRef {
            number: 42,
            state: "MERGED".into(),
            head_oid: None,
        }];
        let open = [PullRequestRef {
            number: 42,
            state: "OPEN".into(),
            head_oid: None,
        }];
        let unrelated = [PullRequestRef {
            number: 7,
            state: "MERGED".into(),
            head_oid: None,
        }];
        let reopened = [
            PullRequestRef {
                number: 42,
                state: "MERGED".into(),
                head_oid: None,
            },
            PullRequestRef {
                number: 84,
                state: "OPEN".into(),
                head_oid: None,
            },
        ];

        assert!(confirms_delete(&merged, Some(42), None));
        assert!(!confirms_delete(&open, Some(42), None));
        assert!(!confirms_delete(&unrelated, Some(42), None));
        assert!(!confirms_delete(&reopened, Some(42), None));
        assert!(confirms_delete(&[], None, None));
        assert!(!confirms_delete(&merged, None, None));
        // No claim: a merged PR confirms only the tip Git proved landed.
        let at_tip = [PullRequestRef {
            number: 42,
            state: "MERGED".into(),
            head_oid: Some("tip".into()),
        }];
        assert!(confirms_delete(&at_tip, None, Some("tip")));
        assert!(!confirms_delete(&at_tip, None, Some("newer")));
        assert!(!confirms_delete(&at_tip, None, None));
    }
}
