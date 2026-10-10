use std::{collections::BTreeMap, path::Path, time::Duration};

use tokio_util::sync::CancellationToken;
use wt_github::GithubClient;
use wt_platform::process::ProcessRunner;

use crate::{
    backup::backup_owner,
    chain::{RestackChain, StackStep},
    git::{checked_git, git_sha, is_ancestor, process_message, run_git},
    service::{StackConfig, StackError, StackEvent, StackService},
};

pub(crate) struct ReplayChainResult {
    pub total: usize,
    pub replayed: usize,
    pub conflict: Option<ReplayConflict>,
    pub error: Option<String>,
}

pub(crate) struct ReplayConflict {
    pub branch: String,
    pub backup_ref: String,
    pub error: String,
}

struct ReplayStepResult {
    new_tip: String,
    new_base: String,
    moved: bool,
    pushed: bool,
}

pub(crate) struct ReplayContext<'a> {
    pub service: &'a StackService,
    pub config: &'a StackConfig,
    pub runner: &'a ProcessRunner,
    pub github: &'a GithubClient,
}

pub(crate) async fn replay_chain(
    context: &ReplayContext<'_>,
    chain: &RestackChain,
    trunk: &str,
    cancellation: &CancellationToken,
    on_event: &mut dyn FnMut(StackEvent),
) -> Result<ReplayChainResult, StackError> {
    for step in &chain.steps {
        let status = run_git(
            context.runner,
            Path::new(&step.worktree_path),
            [
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
            ],
            cancellation,
        )
        .await?;
        if !status.status.success() {
            return Ok(ReplayChainResult {
                total: chain.steps.len(),
                replayed: 0,
                conflict: None,
                error: Some(format!(
                    "could not inspect {} before replay: {}",
                    step.branch,
                    process_message(&wt_platform::process::ProcessError::Exit {
                        program: "git".into(),
                        code: status.status.code(),
                        stderr: status.stderr_text(),
                        stdout: status.stdout_text()
                    })
                )),
            });
        }
        if !status.stdout.is_empty() {
            return Ok(ReplayChainResult {
                total: chain.steps.len(),
                replayed: 0,
                conflict: None,
                error: Some(format!(
                    "worktree {} ({}) has uncommitted or untracked changes; commit, move, or stash them before restacking",
                    step.worktree_path, step.branch
                )),
            });
        }
        if rebase_in_progress(context.runner, step, cancellation).await? {
            return Ok(ReplayChainResult {
                total: chain.steps.len(),
                replayed: 0,
                conflict: None,
                error: Some(format!(
                    "worktree {} ({}) is mid-rebase; finish or `git rebase --abort` there before restacking",
                    step.worktree_path, step.branch
                )),
            });
        }
    }

    let path_by_branch = chain
        .steps
        .iter()
        .map(|step| (step.branch.as_str(), step.worktree_path.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut anchors = BTreeMap::new();
    for step in &chain.steps {
        let parent_ref = anchor_parent_ref(context.runner, step, trunk, cancellation).await?;
        let anchor = resolve_anchor(context.runner, step, &parent_ref, cancellation).await?;
        let Some(anchor) = anchor else {
            return Ok(ReplayChainResult {
                total: chain.steps.len(),
                replayed: 0,
                conflict: None,
                error: Some(format!(
                    "could not resolve a replay anchor for {} (no recorded base sha and no merge-base with {})",
                    step.branch, parent_ref
                )),
            });
        };
        anchors.insert(step.branch.clone(), anchor);
    }

    let mut new_tip_by_branch = BTreeMap::new();
    let mut replayed = 0;
    for step in &chain.steps {
        if cancellation.is_cancelled() {
            return Err(StackError::Cancelled);
        }
        let anchor = anchors
            .get(&step.branch)
            .expect("anchor preflight covered every member");
        let Some(new_base) = resolve_new_base(
            context.runner,
            context.config,
            step,
            trunk,
            &new_tip_by_branch,
            &path_by_branch,
            cancellation,
        )
        .await?
        else {
            return Ok(ReplayChainResult {
                total: chain.steps.len(),
                replayed,
                conflict: None,
                error: Some(format!(
                    "could not resolve the new base for {} (parent {})",
                    step.branch,
                    step.parent_branch.as_deref().unwrap_or(trunk)
                )),
            });
        };
        on_event(StackEvent::Log(format!("replay {}", step.branch)));
        let result = match replay_one(
            context.runner,
            step,
            anchor,
            &new_base,
            cancellation,
            on_event,
        )
        .await?
        {
            Ok(result) => result,
            Err(ReplayFailure::Conflict { backup_ref, error }) => {
                return Ok(ReplayChainResult {
                    total: chain.steps.len(),
                    replayed,
                    conflict: Some(ReplayConflict {
                        branch: step.branch.clone(),
                        backup_ref,
                        error,
                    }),
                    error: None,
                });
            }
            Err(ReplayFailure::Error(error)) => {
                return Ok(ReplayChainResult {
                    total: chain.steps.len(),
                    replayed,
                    conflict: None,
                    error: Some(error),
                });
            }
        };
        new_tip_by_branch.insert(step.branch.clone(), result.new_tip.clone());
        if (step.parent_branch.is_some() || step.has_record)
            && !context
                .service
                .advance_anchor(
                    &step.slug,
                    step.parent_branch.as_deref().unwrap_or(trunk),
                    &result.new_base,
                )
                .await?
        {
            on_event(StackEvent::Log(format!(
                "{}: base record changed during replay; left it for the next reconcile",
                step.branch
            )));
        }
        if result.moved {
            replayed += 1;
        }
        if result.moved || result.pushed {
            retarget_if_needed(
                context.runner,
                context.github,
                step,
                step.parent_branch.as_deref().unwrap_or(trunk),
                cancellation,
                on_event,
            )
            .await?;
        }
    }
    Ok(ReplayChainResult {
        total: chain.steps.len(),
        replayed,
        conflict: None,
        error: None,
    })
}

async fn rebase_in_progress(
    runner: &ProcessRunner,
    step: &StackStep,
    cancellation: &CancellationToken,
) -> Result<bool, StackError> {
    for name in ["rebase-merge", "rebase-apply"] {
        let output = run_git(
            runner,
            Path::new(&step.worktree_path),
            ["rev-parse", "--git-path", name],
            cancellation,
        )
        .await?;
        if !output.status.success() {
            continue;
        }
        let path = Path::new(&step.worktree_path).join(output.stdout_text().trim());
        if tokio::fs::try_exists(path)
            .await
            .map_err(|error| StackError::Invalid(format!("inspect rebase state: {error}")))?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn anchor_parent_ref(
    runner: &ProcessRunner,
    step: &StackStep,
    trunk: &str,
    cancellation: &CancellationToken,
) -> Result<String, StackError> {
    let Some(parent) = step.parent_branch.as_deref() else {
        return Ok(format!("origin/{trunk}"));
    };
    for candidate in [parent.to_owned(), format!("origin/{parent}")] {
        if git_sha(
            runner,
            Path::new(&step.worktree_path),
            &candidate,
            cancellation,
        )
        .await?
        .is_some()
        {
            return Ok(candidate);
        }
    }
    Ok(parent.to_owned())
}

async fn resolve_anchor(
    runner: &ProcessRunner,
    step: &StackStep,
    parent_ref: &str,
    cancellation: &CancellationToken,
) -> Result<Option<String>, StackError> {
    let cwd = Path::new(&step.worktree_path);
    let merge = run_git(
        runner,
        cwd,
        ["merge-base", &step.branch, parent_ref],
        cancellation,
    )
    .await?;
    let live = merge
        .status
        .success()
        .then(|| merge.stdout_text().trim().to_owned())
        .filter(|v| !v.is_empty());
    let Some(stored) = step.base_sha.as_deref() else {
        return Ok(live);
    };
    if git_sha(runner, cwd, stored, cancellation).await?.is_none()
        || !is_ancestor(runner, cwd, stored, &step.branch, cancellation).await?
    {
        return Ok(live);
    }
    let Some(live) = live else {
        return Ok(Some(stored.to_owned()));
    };
    if is_ancestor(runner, cwd, stored, &live, cancellation).await? {
        Ok(Some(live))
    } else {
        Ok(Some(stored.to_owned()))
    }
}

async fn resolve_new_base(
    runner: &ProcessRunner,
    config: &StackConfig,
    step: &StackStep,
    trunk: &str,
    replayed: &BTreeMap<String, String>,
    path_by_branch: &BTreeMap<&str, &str>,
    cancellation: &CancellationToken,
) -> Result<Option<String>, StackError> {
    let cwd = Path::new(&step.worktree_path);
    let Some(parent) = step.parent_branch.as_deref() else {
        let Some(main_tip) = git_sha(
            runner,
            &config.main_clone,
            &format!("origin/{trunk}"),
            cancellation,
        )
        .await?
        else {
            return Ok(None);
        };
        let child_tip = git_sha(runner, cwd, &format!("origin/{trunk}"), cancellation).await?;
        if child_tip.as_deref() != Some(&main_tip) {
            checked_git(
                runner,
                cwd,
                [
                    "fetch",
                    "--no-tags",
                    config.main_clone.to_string_lossy().as_ref(),
                    &format!("+refs/remotes/origin/{trunk}:refs/remotes/origin/{trunk}"),
                ],
                cancellation,
            )
            .await?;
        }
        return Ok(Some(main_tip));
    };
    if let Some(parent_tip) = replayed.get(parent) {
        if git_sha(runner, cwd, parent_tip, cancellation)
            .await?
            .is_some()
        {
            return Ok(Some(parent_tip.clone()));
        }
        if let Some(parent_path) = path_by_branch.get(parent) {
            checked_git(
                runner,
                cwd,
                [
                    "fetch",
                    "--no-tags",
                    parent_path,
                    &format!("+refs/heads/{parent}:refs/remotes/origin/{parent}"),
                ],
                cancellation,
            )
            .await?;
            if git_sha(runner, cwd, parent_tip, cancellation)
                .await?
                .is_some()
            {
                return Ok(Some(parent_tip.clone()));
            }
        }
        return Ok(None);
    }
    for reference in [parent.to_owned(), format!("origin/{parent}")] {
        if let Some(sha) = git_sha(runner, cwd, &reference, cancellation).await? {
            return Ok(Some(sha));
        }
    }
    Ok(None)
}

enum ReplayFailure {
    Conflict { backup_ref: String, error: String },
    Error(String),
}

async fn replay_one(
    runner: &ProcessRunner,
    step: &StackStep,
    anchor: &str,
    new_base: &str,
    cancellation: &CancellationToken,
    on_event: &mut dyn FnMut(StackEvent),
) -> Result<Result<ReplayStepResult, ReplayFailure>, StackError> {
    let cwd = Path::new(&step.worktree_path);
    let Some(new_base_sha) = git_sha(runner, cwd, new_base, cancellation).await? else {
        return Ok(Err(ReplayFailure::Error(format!(
            "cannot resolve new base for {}",
            step.branch
        ))));
    };
    let Some(before_tip) = git_sha(runner, cwd, &step.branch, cancellation).await? else {
        return Ok(Err(ReplayFailure::Error(format!(
            "cannot resolve branch {}",
            step.branch
        ))));
    };
    let remote = git_sha(
        runner,
        cwd,
        &format!("origin/{}", step.branch),
        cancellation,
    )
    .await?;
    if anchor == new_base_sha {
        let mut pushed = false;
        if let Some(remote_tip) = remote.as_deref().filter(|tip| *tip != before_tip) {
            if let Err(error) = force_with_lease(
                runner,
                cwd,
                &step.branch,
                &before_tip,
                remote_tip,
                cancellation,
            )
            .await
            {
                return Ok(Err(ReplayFailure::Error(error)));
            }
            pushed = true;
        }
        if !pushed {
            on_event(StackEvent::Log(format!(
                "{}: already on base, skipping",
                step.branch
            )));
        }
        prune_superseded_backups(runner, cwd, &step.branch, on_event).await;
        return Ok(Ok(ReplayStepResult {
            new_tip: before_tip,
            new_base: new_base_sha,
            moved: false,
            pushed,
        }));
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let backup_ref = format!("backup/restack-{timestamp}-{}", step.branch);
    let backup = run_git(
        runner,
        cwd,
        ["branch", "--force", &backup_ref, &before_tip],
        cancellation,
    )
    .await?;
    if !backup.status.success() {
        return Ok(Err(ReplayFailure::Error(format!(
            "could not snapshot {} to {backup_ref}: {}",
            step.branch,
            process_message(&wt_platform::process::ProcessError::Exit {
                program: "git".into(),
                code: backup.status.code(),
                stderr: backup.stderr_text(),
                stdout: backup.stdout_text()
            })
        ))));
    }
    on_event(StackEvent::Log(format!(
        "rebase {} onto {} (from {})",
        step.branch,
        short(&new_base_sha),
        short(anchor)
    )));
    let mut last_error = String::new();
    let mut succeeded = false;
    for attempt in 1..=5_u64 {
        let result = run_git(
            runner,
            cwd,
            ["rebase", "--onto", &new_base_sha, anchor, &step.branch],
            cancellation,
        )
        .await;
        let output = match result {
            Ok(output) => output,
            Err(_error) if cancellation.is_cancelled() => {
                abort_rebase(runner, cwd).await;
                return Err(StackError::Cancelled);
            }
            Err(error) => {
                last_error = error.to_string();
                continue;
            }
        };
        if output.status.success() {
            succeeded = true;
            break;
        }
        last_error = process_message(&wt_platform::process::ProcessError::Exit {
            program: "git".into(),
            code: output.status.code(),
            stderr: output.stderr_text(),
            stdout: output.stdout_text(),
        });
        if rebase_in_progress(runner, step, cancellation).await? {
            let conflicts = run_git(
                runner,
                cwd,
                ["diff", "--name-only", "--diff-filter=U", "-z"],
                cancellation,
            )
            .await?;
            let names = conflicts
                .stdout
                .split(|byte| *byte == 0)
                .filter(|value| !value.is_empty())
                .map(|value| String::from_utf8_lossy(value).into_owned())
                .collect::<Vec<_>>();
            let aborted = abort_rebase(runner, cwd).await;
            if !aborted {
                return Ok(Err(ReplayFailure::Error(format!(
                    "rebase of {} could not be aborted cleanly; backup remains at {backup_ref}",
                    step.branch
                ))));
            }
            if !names.is_empty() {
                let listing = names.join(", ");
                return Ok(Err(ReplayFailure::Conflict {
                    backup_ref,
                    error: format!(
                        "conflict replaying {} (conflicts in {listing})",
                        step.branch
                    ),
                }));
            }
            if looks_like_lock_error(&last_error) && attempt < 5 {
                on_event(StackEvent::Log(format!(
                    "{}: transient git lock during rebase, retrying ({attempt}/5)",
                    step.branch
                )));
                tokio::select! { _ = cancellation.cancelled() => return Err(StackError::Cancelled), _ = tokio::time::sleep(Duration::from_millis(250 * attempt)) => {} }
                continue;
            }
            return Ok(Err(ReplayFailure::Error(format!(
                "could not replay {} onto {} (branch tip restored): {last_error}",
                step.branch,
                short(&new_base_sha)
            ))));
        }
        if attempt < 5 {
            on_event(StackEvent::Log(format!(
                "{}: rebase did not start, retrying ({attempt}/5)",
                step.branch
            )));
            tokio::select! { _ = cancellation.cancelled() => return Err(StackError::Cancelled), _ = tokio::time::sleep(Duration::from_millis(250 * attempt)) => {} }
        }
    }
    if !succeeded {
        delete_backup(runner, cwd, &backup_ref, cancellation).await;
        return Ok(Err(ReplayFailure::Error(format!(
            "could not replay {} after five attempts: {last_error}",
            step.branch
        ))));
    }
    let Some(new_tip) = git_sha(runner, cwd, &step.branch, cancellation).await? else {
        return Ok(Err(ReplayFailure::Error(format!(
            "lost {} tip after rebase",
            step.branch
        ))));
    };
    if new_tip == before_tip {
        delete_backup(runner, cwd, &backup_ref, cancellation).await;
        let mut pushed = false;
        if let Some(remote_tip) = remote.as_deref().filter(|tip| *tip != new_tip) {
            if let Err(error) = force_with_lease(
                runner,
                cwd,
                &step.branch,
                &new_tip,
                remote_tip,
                cancellation,
            )
            .await
            {
                return Ok(Err(ReplayFailure::Error(error)));
            }
            pushed = true;
        }
        prune_superseded_backups(runner, cwd, &step.branch, on_event).await;
        return Ok(Ok(ReplayStepResult {
            new_tip,
            new_base: new_base_sha,
            moved: false,
            pushed,
        }));
    }
    let Some(remote_tip) = remote else {
        delete_backup(runner, cwd, &backup_ref, cancellation).await;
        on_event(StackEvent::Log(format!(
            "rebased {} (not pushed: no origin branch)",
            step.branch
        )));
        prune_superseded_backups(runner, cwd, &step.branch, on_event).await;
        return Ok(Ok(ReplayStepResult {
            new_tip,
            new_base: new_base_sha,
            moved: true,
            pushed: false,
        }));
    };
    if let Err(error) = force_with_lease(
        runner,
        cwd,
        &step.branch,
        &new_tip,
        &remote_tip,
        cancellation,
    )
    .await
    {
        return Ok(Err(ReplayFailure::Error(error)));
    }
    delete_backup(runner, cwd, &backup_ref, cancellation).await;
    on_event(StackEvent::Log(format!("pushed {}", step.branch)));
    prune_superseded_backups(runner, cwd, &step.branch, on_event).await;
    Ok(Ok(ReplayStepResult {
        new_tip,
        new_base: new_base_sha,
        moved: true,
        pushed: true,
    }))
}

async fn force_with_lease(
    runner: &ProcessRunner,
    cwd: &Path,
    branch: &str,
    local_tip: &str,
    remote_tip: &str,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let out = run_git(
        runner,
        cwd,
        [
            "push",
            &format!("--force-with-lease=refs/heads/{branch}:{remote_tip}"),
            "origin",
            &format!("{local_tip}:refs/heads/{branch}"),
        ],
        cancellation,
    )
    .await
    .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "push {branch}: {}",
            process_message(&wt_platform::process::ProcessError::Exit {
                program: "git".into(),
                code: out.status.code(),
                stderr: out.stderr_text(),
                stdout: out.stdout_text()
            })
        ))
    }
}

async fn abort_rebase(runner: &ProcessRunner, cwd: &Path) -> bool {
    let cleanup = CancellationToken::new();
    for _ in 0..4 {
        let _ = run_git(runner, cwd, ["rebase", "--abort"], &cleanup).await;
        let merge = run_git(
            runner,
            cwd,
            ["rev-parse", "--git-path", "rebase-merge"],
            &cleanup,
        )
        .await;
        let apply = run_git(
            runner,
            cwd,
            ["rev-parse", "--git-path", "rebase-apply"],
            &cleanup,
        )
        .await;
        let mut active = false;
        for output in [merge, apply].into_iter().flatten() {
            if output.status.success()
                && tokio::fs::try_exists(cwd.join(output.stdout_text().trim()))
                    .await
                    .unwrap_or(true)
            {
                active = true;
            }
        }
        if !active {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    false
}

async fn delete_backup(
    runner: &ProcessRunner,
    cwd: &Path,
    reference: &str,
    cancellation: &CancellationToken,
) {
    let _ = run_git(runner, cwd, ["branch", "-D", reference], cancellation).await;
}

async fn prune_superseded_backups(
    runner: &ProcessRunner,
    cwd: &Path,
    branch: &str,
    on_event: &mut dyn FnMut(StackEvent),
) {
    let Ok(refs) = run_git(
        runner,
        cwd,
        [
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/backup/",
        ],
        &CancellationToken::new(),
    )
    .await
    else {
        return;
    };
    let stale = refs
        .stdout_text()
        .lines()
        .filter(|reference| backup_owner(reference) == Some(branch))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for reference in &stale {
        let _ = run_git(
            runner,
            cwd,
            ["branch", "-D", reference],
            &CancellationToken::new(),
        )
        .await;
    }
    if !stale.is_empty() {
        on_event(StackEvent::Log(format!(
            "pruned {} stale backup(s) of {branch}",
            stale.len()
        )));
    }
}

async fn retarget_if_needed(
    runner: &ProcessRunner,
    github: &GithubClient,
    step: &StackStep,
    expected_base: &str,
    cancellation: &CancellationToken,
    on_event: &mut dyn FnMut(StackEvent),
) -> Result<(), StackError> {
    let Some(pr) = github.view_pr(&step.branch, cancellation).await? else {
        return Ok(());
    };
    if pr.state == "CLOSED" && pr.base_ref_name != expected_base {
        if git_sha(
            runner,
            Path::new(&step.worktree_path),
            &format!("origin/{}", pr.base_ref_name),
            cancellation,
        )
        .await?
        .is_none()
        {
            on_event(StackEvent::Attention(format!(
                "{}: PR #{} was closed when its base was deleted; branch is restacked onto {expected_base}; open a fresh PR",
                step.branch, pr.number
            )));
        }
        return Ok(());
    }
    if pr.state != "OPEN" || pr.base_ref_name == expected_base {
        return Ok(());
    }
    let result = github
        .retarget_pr_base(pr.number, expected_base, cancellation)
        .await;
    if result.is_ok() {
        on_event(StackEvent::Log(format!(
            "retargeted PR #{} base to {expected_base}",
            pr.number
        )));
    } else {
        on_event(StackEvent::Log(format!(
            "warning: could not retarget PR #{} base: {}",
            pr.number,
            result.error.unwrap_or_default()
        )));
    }
    Ok(())
}

fn looks_like_lock_error(message: &str) -> bool {
    message.contains("another git process")
        || (message.contains("unable to create") && message.contains(".lock"))
}

fn short(sha: &str) -> &str {
    sha.get(..9).unwrap_or(sha)
}
