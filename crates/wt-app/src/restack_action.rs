//! Durable, host-local entry point for the TUI's restack action.

use anyhow::{Context, Result, bail};
use wt_actions::{ActionRequest, ActionRunKind};
use wt_config::EffectTag;
use wt_github::{GithubClient, GithubData, GithubOptions};
use wt_platform::lock::FileLock;
use wt_stack::{
    RestackOptions, RestackOutcome, StackConfig, StackEvent, StackService, StateConfig,
};
use wt_store::RepositoryIdentity;
use wt_tui::{Board, UiReply};

use crate::{context::AppContext, harness::AppHarness};

const ACTION_ID: &str = "wt-restack";

/// Validate the selected local row, then enqueue a durable action worker.
/// The worker owns the stack operation and its conflict handoff, so neither is
/// lost if the TUI process or its SSH connection exits.
pub async fn start(
    context: &AppContext,
    key: &str,
    github: &GithubData,
    board: Option<&Board>,
) -> Result<UiReply> {
    let Some(row_view) = board
        .into_iter()
        .flat_map(|board| &board.rows)
        .find(|row| row.key == key)
    else {
        bail!("selected worktree is no longer visible; refresh and try again")
    };
    if row_view.archived || row_view.host.is_some() {
        bail!("restack requires a live local worktree")
    }
    let rows = context.repository.inventory(&context.cancellation).await?;
    let Some(row) = rows
        .iter()
        .find(|row| !row.is_main && wt_core::worktree_target_key(&row.target) == key)
    else {
        bail!("selected worktree disappeared before restack could start")
    };
    if !matches!(row.target.location(), wt_core::WorktreeLocation::Local) {
        bail!("restack is only available for local worktrees")
    }
    if row.target.branch.is_empty() {
        bail!("selected worktree has no branch to restack")
    }

    let service = stack_service(context);
    if service
        .is_busy(&row.target.branch, &context.cancellation)
        .await?
    {
        bail!(
            "{} is already being restacked or changed",
            row.target.branch
        )
    }
    if FileLock::try_acquire(
        &context.config.paths.lock_dir,
        row.target.slug(),
        "check worktree before restack",
    )
    .await?
    .is_none()
    {
        bail!("{} is busy; not starting a restack", row.target.slug())
    }

    let state = context
        .database
        .call(|store| Ok((store.read_wt_state()?, store.read_archived_keys()?)))
        .await?;
    if state.1.contains(row.target.slug()) {
        bail!(
            "{} is being cleaned up; not starting a restack",
            row.target.slug()
        )
    }
    let plan =
        crate::lifecycle_ops::plan_with_facts(context, vec![row.clone()], &state.0, github, None)
            .await?;
    if plan
        .rows
        .first()
        .is_some_and(|plan| plan.landed && plan.hazards.is_empty())
    {
        bail!("branch already landed; clean it (c) instead of rebasing")
    }

    let executable = std::env::current_exe().context("resolve wt executable for restack worker")?;
    let mut config_selectors = std::collections::BTreeMap::new();
    if let Some(path) = &context.config.repository_config {
        config_selectors.insert("WT_REPO_CONFIG".into(), path.to_string_lossy().into_owned());
    }
    let request = ActionRequest {
        action_key: key.to_owned(),
        slug: row.target.slug().to_owned(),
        worktree_ref: Some(wt_core::WorktreeRef::Local {
            slug: row.target.slug().to_owned(),
        }),
        action_id: ACTION_ID.into(),
        action_name: "Restack stack".into(),
        arg_history: None,
        prompt: format!("wt restack {}", row.target.branch),
        kind: ActionRunKind::Harness,
        command: worker_command(&executable, &row.target.branch),
        cwd: context.config.paths.main_clone.clone(),
        affects: vec![EffectTag::Git, EffectTag::Github],
        issue_status: None,
        external: false,
        auto_fire_keys: Vec::new(),
        config_selectors,
    };
    match crate::actions::service(context)?
        .start(request, &context.cancellation)
        .await
    {
        Ok(_start) => Ok(UiReply {
            message: format!(
                "Restack started for {}. Progress and logs are on the worktree row.",
                row.target.branch
            ),
            ..UiReply::default()
        }),
        Err(wt_actions::ActionServiceError::StartAmbiguous {
            run_id,
            session,
            reason,
        }) => Ok(UiReply {
            message: format!(
                "Restack start is uncertain (run {run_id}, session {session}): {reason}. Check the row logs before retrying."
            ),
            failed: true,
            ..UiReply::default()
        }),
        Err(error) => Ok(UiReply {
            message: format!("Could not start restack: {error}"),
            failed: true,
            ..UiReply::default()
        }),
    }
}

/// Entry used by the hidden durable worker command. This deliberately handles
/// handoff in the worker process rather than the transient TUI controller.
pub async fn worker(context: &AppContext, branch: &str) -> Result<i32> {
    let service = stack_service(context);
    let mut event = |event| match event {
        StackEvent::Log(message) => println!("{message}"),
        StackEvent::Attention(message) => eprintln!("attention: {message}"),
    };
    match service
        .restack(
            branch,
            RestackOptions::default(),
            &context.cancellation,
            &mut event,
        )
        .await?
    {
        RestackOutcome::Complete { replayed, total } => {
            println!("restacked {branch} ({replayed}/{total} worktrees)");
            Ok(0)
        }
        RestackOutcome::Conflict {
            branch,
            backup_ref,
            error,
        } => {
            eprintln!("restack conflict: {error}");
            eprintln!("failing branch: {branch}");
            eprintln!("backup branch: {backup_ref}");
            match handoff_conflict(context, &branch, &error, &backup_ref).await {
                Ok(()) => eprintln!("sent /restack recovery instructions to the failing worktree"),
                Err(handoff_error) => eprintln!(
                    "could not hand off /restack recovery ({handoff_error:#}); resolve in that worktree manually"
                ),
            }
            Ok(3)
        }
        RestackOutcome::Refused { error } => {
            eprintln!("restack refused: {error}");
            Ok(1)
        }
    }
}

async fn handoff_conflict(
    context: &AppContext,
    branch: &str,
    detail: &str,
    backup_ref: &str,
) -> Result<()> {
    let inventory = context
        .repository
        .inventory_status(&context.cancellation)
        .await?;
    let Some(row) = inventory.iter().find(|snapshot| {
        !snapshot.worktree.is_main
            && snapshot.worktree.target.branch == branch
            && matches!(
                snapshot.worktree.target.location(),
                wt_core::WorktreeLocation::Local
            )
    }) else {
        bail!("the failing local worktree is no longer present")
    };
    let slug = row.worktree.target.slug();
    let archived = context
        .database
        .call(|store| Ok(store.read_archived_keys()?))
        .await?;
    if archived.contains(slug) || !std::path::Path::new(&row.worktree.target.path).is_dir() {
        bail!("the failing worktree is gone or being cleaned")
    }
    let lock = FileLock::try_acquire(
        &context.config.paths.lock_dir,
        slug,
        "handoff restack conflict",
    )
    .await?
    .context("the failing worktree is busy")?;
    drop(lock);

    let harness = AppHarness::new(context);
    let routes = harness.routes(context).await?;
    let route = routes
        .iter()
        .find(|route| {
            !route.target.remote
                && route.target.slug == slug
                && route.target.branch.as_deref() == Some(branch)
        })
        .context("the failing worktree has no local harness route")?;
    let selected = route
        .choice
        .selected
        .context("could not inspect the wt tmux session registry")?;
    let text = conflict_handoff_text(selected, detail, backup_ref);
    match harness.send(route, &text, None, context).await {
        Ok(wt_harness::HarnessMessageOutcome::Claude(wt_harness::ClaudeMessageOutcome::Sent {
            delivered: Some(true),
            ..
        }))
        | Ok(wt_harness::HarnessMessageOutcome::Codex(
            wt_harness::CodexMessageOutcome::Queued(_)
            | wt_harness::CodexMessageOutcome::CliQueued { .. },
        ))
        | Ok(wt_harness::HarnessMessageOutcome::OpenCode(_)) => Ok(()),
        Ok(wt_harness::HarnessMessageOutcome::Claude(wt_harness::ClaudeMessageOutcome::Sent {
            delivered: None,
            ..
        }))
        | Ok(wt_harness::HarnessMessageOutcome::Codex(
            wt_harness::CodexMessageOutcome::Terminal {
                delivered: None, ..
            }
            | wt_harness::CodexMessageOutcome::Ambiguous { .. },
        )) => {
            eprintln!("/restack handoff may have been delivered; not retrying to avoid duplicates");
            Ok(())
        }
        Ok(wt_harness::HarnessMessageOutcome::Claude(wt_harness::ClaudeMessageOutcome::Sent {
            delivered: Some(false),
            ..
        }))
        | Ok(wt_harness::HarnessMessageOutcome::Codex(
            wt_harness::CodexMessageOutcome::Terminal {
                delivered: Some(false),
                ..
            }
            | wt_harness::CodexMessageOutcome::NeedsTerminalFallback { .. }
            | wt_harness::CodexMessageOutcome::Failed { .. },
        )) => bail!("the harness did not confirm delivery"),
        Ok(wt_harness::HarnessMessageOutcome::Claude(
            wt_harness::ClaudeMessageOutcome::Failed {
                reason,
                maybe_submitted: true,
            },
        )) => bail!("delivery is uncertain ({reason}); not retrying to avoid duplicates"),
        Ok(wt_harness::HarnessMessageOutcome::Claude(
            wt_harness::ClaudeMessageOutcome::Failed {
                reason,
                maybe_submitted: false,
            },
        )) => bail!("{reason}"),
        Ok(wt_harness::HarnessMessageOutcome::Codex(
            wt_harness::CodexMessageOutcome::Terminal {
                delivered: Some(true),
                ..
            },
        )) => Ok(()),
        Err(error) => Err(error).context("send /restack conflict handoff"),
    }
}

fn worker_command(executable: &std::path::Path, branch: &str) -> Vec<String> {
    vec![
        executable.to_string_lossy().into_owned(),
        "_restack-worker".into(),
        "--branch".into(),
        branch.to_owned(),
    ]
}

fn conflict_handoff_text(harness: wt_core::HarnessId, detail: &str, backup_ref: &str) -> String {
    let prefix = match harness {
        wt_core::HarnessId::Claude => "/",
        wt_core::HarnessId::Codex | wt_core::HarnessId::Opencode => "$",
    };
    format!(
        "{prefix}restack\n\nwt's restack engine just bailed on this worktree: {detail}. The pre-rebase tip is backed up at {backup_ref}. Resolve the conflict and finish the restack."
    )
}

fn stack_service(context: &AppContext) -> StackService {
    let github = GithubClient::new(
        context.processes.clone(),
        context.config.paths.main_clone.clone(),
        GithubOptions::from_config(&context.config, false),
    );
    let config = StackConfig {
        main_clone: context.config.paths.main_clone.clone(),
        lock_dir: context.config.paths.lock_dir.clone(),
        trunk_branch: context.config.branch.base.clone(),
        fetch_options: crate::origin::options(context),
        state: StateConfig {
            path: context.config.paths.state_db.clone(),
            identity: RepositoryIdentity::new(
                context.config.repo_id.clone(),
                context.config.repo_path.to_string_lossy(),
            ),
        },
    };
    StackService::new(
        config,
        (*context.repository).clone(),
        context.processes.clone(),
        github,
    )
}

#[cfg(test)]
mod tests {
    use super::{conflict_handoff_text, worker_command};
    use std::path::Path;

    #[test]
    fn worker_receives_branch_as_one_literal_argument() {
        let args = worker_command(Path::new("/opt/wt"), "topic/a; echo unsafe");
        assert_eq!(
            args,
            vec![
                "/opt/wt".to_owned(),
                "_restack-worker".to_owned(),
                "--branch".to_owned(),
                "topic/a; echo unsafe".to_owned()
            ]
        );
    }

    #[test]
    fn conflict_handoff_uses_the_selected_harness_skill_prefix_and_backup() {
        let claude = conflict_handoff_text(wt_core::HarnessId::Claude, "conflict", "backup/x");
        let codex = conflict_handoff_text(wt_core::HarnessId::Codex, "conflict", "backup/x");
        assert!(claude.starts_with("/restack\n"));
        assert!(codex.starts_with("$restack\n"));
        assert!(claude.contains("backup/x"));
        assert!(claude.contains("Resolve the conflict and finish the restack."));
    }
}
