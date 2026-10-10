//! Host-local implementations of the automation engine's built-in runs.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeSet;
use wt_automations::AutomationFire;
use wt_core::{ChainMember, build_stack_index};
use wt_github::{GithubClient, GithubOptions};
use wt_platform::process::CommandSpec;
use wt_stack::{RestackOptions, RestackOutcome, StackConfig, StackService, StateConfig};
use wt_store::RepositoryIdentity;

use crate::{context::AppContext, lifecycle_ops};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuiltinExecution {
    /// Positive proof that no effect began; the caller may release the fire.
    Retry {
        reason: String,
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
                CommandSpec::new("notify-send").args([&title, &fire.detail]),
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
        )?;
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
                    "number,state",
                ])
                .cwd(&ctx.config.paths.main_clone),
            &ctx.cancellation,
        )
        .await?;
    let output = output.checked("gh pr list")?;
    let prs: Vec<PullRequestRef> = serde_json::from_slice(&output.stdout)
        .context("parse successful `gh pr list` confirmation")?;
    let claimed = fire.delete_branch_pr;
    let confirmed = confirms_delete(&prs, claimed);
    if !confirmed {
        let reason = if let Some(number) = claimed {
            format!("fire claimed PR #{number}, but no matching merged PR was returned")
        } else {
            format!("{} PR(s) exist, so absence was not proven", prs.len())
        };
        attention(
            ctx,
            "warn",
            &format!("auto {}: NOT deleting {branch} - {reason}", fire.rule.id),
        )?;
        return Ok(BuiltinExecution::Completed {
            message: format!("declined to delete {branch}: {reason}"),
        });
    }
    let result = github_client(ctx)
        .delete_remote_branch(branch, &ctx.cancellation)
        .await;
    if result.ok {
        attention(
            ctx,
            "info",
            &format!("auto {}: deleted remote branch {branch}", fire.rule.id),
        )?;
        Ok(BuiltinExecution::Completed {
            message: format!("deleted remote branch {branch}"),
        })
    } else {
        tracing::warn!(rule = %fire.rule.id, branch, error = ?result.error, "automation delete branch declined");
        Ok(BuiltinExecution::Completed {
            message: format!(
                "remote branch {branch} was not deleted: {}",
                result.error.unwrap_or_default()
            ),
        })
    }
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

fn attention(ctx: &AppContext, level: &str, text: &str) -> Result<()> {
    let level = match level {
        "warn" => crate::commands::manager::ReportLevel::Warn,
        _ => crate::commands::manager::ReportLevel::Info,
    };
    crate::commands::manager::append_report(
        &ctx.config.paths.cache_root.join("manager/reports.jsonl"),
        level,
        text,
    )
}

fn confirms_delete(prs: &[PullRequestRef], claimed: Option<u64>) -> bool {
    if prs.iter().any(|pr| pr.state == "OPEN") {
        return false;
    }
    match claimed {
        Some(number) => prs
            .iter()
            .any(|pr| pr.number == number && pr.state == "MERGED"),
        None => prs.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::{PullRequestRef, confirms_delete};

    #[test]
    fn delete_requires_exact_merged_pr_or_successful_empty_list() {
        let merged = [PullRequestRef {
            number: 42,
            state: "MERGED".into(),
        }];
        let open = [PullRequestRef {
            number: 42,
            state: "OPEN".into(),
        }];
        let unrelated = [PullRequestRef {
            number: 7,
            state: "MERGED".into(),
        }];
        let reopened = [
            PullRequestRef {
                number: 42,
                state: "MERGED".into(),
            },
            PullRequestRef {
                number: 84,
                state: "OPEN".into(),
            },
        ];

        assert!(confirms_delete(&merged, Some(42)));
        assert!(!confirms_delete(&open, Some(42)));
        assert!(!confirms_delete(&unrelated, Some(42)));
        assert!(!confirms_delete(&reopened, Some(42)));
        assert!(confirms_delete(&[], None));
        assert!(!confirms_delete(&merged, None));
    }
}
