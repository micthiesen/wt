//! Native SSH worker boundary. The controller owns presentation and the
//! configured stable worker binary owns its local worktrees and sessions.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::task::JoinSet;
use wt_config::{InstanceRole, RemoteConfig};
use wt_core::parse_work_status;
use wt_remote::{
    StatusKind, WORKER_PROTOCOL_VERSION, WorkerInfo, WorkerRole, WorkerSnapshot, WorktreeSnapshot,
    WorktreeStatus,
};

use crate::{commands::resolve::run_git, context::AppContext};

pub fn worker_info(role: InstanceRole) -> WorkerInfo {
    WorkerInfo {
        role: match role {
            InstanceRole::Controller => WorkerRole::Controller,
            InstanceRole::Worker => WorkerRole::Worker,
        },
        protocol: WORKER_PROTOCOL_VERSION,
        build: env!("WT_BUILD_ID").to_owned(),
    }
}

pub async fn collect_worker_snapshot(context: &AppContext) -> Result<WorkerSnapshot> {
    if context.config.instance.role != InstanceRole::Worker {
        bail!("worker snapshot requires [instance] role = \"worker\" on this host");
    }
    let discovered = context
        .repository
        .inventory_status(&context.cancellation)
        .await?;
    let states = context
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let remote_url = run_git(
        context,
        &context.config.paths.main_clone,
        ["remote", "get-url", "origin"],
    )
    .await
    .ok()
    .filter(|output| output.status.success())
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    let has_non_main = discovered.iter().any(|row| !row.worktree.is_main);
    let dev_rows = if context.config.dev_server.is_some() && has_non_main {
        let dev_worktrees = discovered
            .iter()
            .filter(|row| !row.worktree.is_main)
            .map(|row| wt_dev::DevWorktree {
                slug: row.worktree.target.slug().to_owned(),
                path: Path::new(&row.worktree.target.path).to_owned(),
                branch: row.worktree.target.branch.clone(),
            })
            .collect::<Vec<_>>();
        crate::dev::service(context)?
            .status_all(&dev_worktrees, &context.cancellation)
            .await?
            .worktrees
            .into_iter()
            .map(|row| (row.slug.clone(), row))
            .collect::<HashMap<_, _>>()
    } else {
        HashMap::new()
    };
    let mut tasks = JoinSet::new();
    let mut indexed = Vec::new();
    for (index, snapshot) in discovered
        .into_iter()
        .filter(|row| !row.worktree.is_main)
        .enumerate()
    {
        while tasks.len() >= 8 {
            indexed.push(
                tasks
                    .join_next()
                    .await
                    .context("worker snapshot task disappeared")???,
            );
        }
        let row_context = context.clone();
        let entry = states
            .get("slugs")
            .and_then(|slugs| slugs.get(snapshot.worktree.target.slug()))
            .cloned();
        let remote_url = remote_url.clone();
        let dev_row = dev_rows
            .get(snapshot.worktree.target.slug())
            .cloned()
            .or_else(|| {
                context
                    .config
                    .dev_server
                    .as_ref()
                    .map(|_| wt_dev::DevStatusRow {
                        slug: snapshot.worktree.target.slug().to_owned(),
                        status: None,
                        error: Some("dev status service returned no row".to_owned()),
                    })
            });
        tasks.spawn(async move {
            build_snapshot_row(row_context, snapshot, entry, remote_url, dev_row)
                .await
                .map(|row| (index, row))
        });
    }
    while let Some(row) = tasks.join_next().await {
        indexed.push(row.context("worker snapshot task panicked")??);
    }
    indexed.sort_by_key(|(index, _)| *index);
    let worktrees = indexed.into_iter().map(|(_, row)| row).collect();
    Ok(WorkerSnapshot {
        protocol: WORKER_PROTOCOL_VERSION,
        worktrees,
    })
}

async fn build_snapshot_row(
    context: AppContext,
    snapshot: wt_vcs::WorktreeSnapshot,
    entry: Option<Value>,
    remote_url: Option<String>,
    dev_row: Option<wt_dev::DevStatusRow>,
) -> Result<WorktreeSnapshot> {
    let target = &snapshot.worktree.target;
    let slug = target.slug();
    let base = entry
        .as_ref()
        .and_then(|entry| entry.get("baseBranch"))
        .and_then(Value::as_str)
        .unwrap_or(&context.config.branch.base)
        .to_owned();
    let issue_override = entry
        .as_ref()
        .and_then(|entry| entry.get("issueId"))
        .and_then(Value::as_str);
    let issue_id = resolve_issue_id(slug, issue_override);
    let issue_url = issue_id.as_deref().and_then(|id| {
        if id.starts_with("GH-") {
            let repo = repo_web_url(remote_url.as_deref()?)?;
            Some(format!("{repo}/issues/{}", id.split_once('-')?.1))
        } else {
            context
                .config
                .issue_tracker
                .as_ref()?
                .url_template
                .as_ref()
                .map(|template| template.replace("{id}", &id.to_ascii_uppercase()))
        }
    });
    let work = entry
        .as_ref()
        .and_then(|entry| entry.get("work"))
        .and_then(parse_work_status);
    let git_status = snapshot.status.as_ref();
    let expected_upstream = format!("origin/{}", target.branch);
    let ahead_of_base = ahead_of_base(&context, &target.path, &base).await;
    let has_origin_branch = run_git(
        &context,
        &target.path,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            expected_upstream.as_str(),
        ],
    )
    .await
    .ok()
    .is_some_and(|output| output.status.success());
    let (unpushed, pushed) = if has_origin_branch {
        (
            commit_count(
                &context,
                Path::new(&target.path),
                &format!("{expected_upstream}..HEAD"),
            )
            .await,
            Some(true),
        )
    } else {
        (ahead_of_base, Some(false))
    };
    let path = std::path::PathBuf::from(&target.path);
    let stage_prefix = context.config.stage.prefix.clone();
    let (exists, deployed) = tokio::task::spawn_blocking(move || {
        let exists = path.exists();
        let deployed = matches!(
            wt_sst::observe_local_deployment(&path, &stage_prefix),
            wt_sst::DeploymentObservation::Deployed { .. }
        );
        (exists, deployed)
    })
    .await
    .context("inspect remote snapshot stage")?;
    let issue_number = entry
        .as_ref()
        .and_then(|entry| entry.get("githubIssue"))
        .and_then(Value::as_u64);
    let github_issue_url = issue_number.and_then(|number| {
        repo_web_url(remote_url.as_deref()?).map(|repo| format!("{repo}/issues/{number}"))
    });
    let dirty = git_status.is_some_and(|status| status.dirty);
    let (kind, label) = if snapshot.error.is_some() || git_status.is_none() {
        (StatusKind::Missing, "missing")
    } else if dirty {
        (StatusKind::Dirty, "dirty")
    } else {
        (StatusKind::Clean, "clean")
    };
    Ok(WorktreeSnapshot {
        slug: slug.to_owned(),
        branch: target.branch.clone(),
        base,
        path: target.path.clone(),
        stage: target.stage.clone(),
        deployed,
        exists,
        status: WorktreeStatus {
            kind,
            label: label.to_owned(),
            age: None,
            log: None,
            pid: None,
            op: None,
        },
        // A row error stays explicit while a null status preserves unknown.
        // In particular, a failed status read is never rendered as stopped.
        dev: dev_row
            .as_ref()
            .and_then(|row| row.status.as_ref())
            .map(remote_dev_status),
        dev_error: dev_row.and_then(|row| row.error),
        dirty,
        unpushed,
        pushed,
        ahead_of_base,
        issue_id,
        issue_url,
        github_issue: issue_number,
        github_issue_url,
        work,
    })
}

fn remote_dev_status(status: &wt_dev::DevServerStatus) -> wt_remote::DevServerStatus {
    wt_remote::DevServerStatus {
        running: status.running,
        starting: status.starting,
        crashed: status.crashed,
        port: status.port,
        url: status.url.clone(),
        since: status.since,
        waiting: status.waiting.map(|waiting| wt_remote::DevServerWaiting {
            rank: waiting.rank,
            since: waiting.since,
        }),
        rebased_since: status.rebased_since,
        restarts: status
            .restarts
            .as_ref()
            .map(|restarts| wt_remote::DevServerRestarts {
                count: restarts.count,
                last_exit: restarts.last_exit,
            }),
    }
}

pub async fn remote_admin_command(
    context: &AppContext,
    remote: &RemoteConfig,
    args: &[String],
) -> Result<i32> {
    let client = wt_remote::RemoteClient::new(context.processes.clone(), remote.clone());
    let output = client.run_worker(args, &context.cancellation).await?;
    if !output.stdout.is_empty() {
        print!("{}", output.stdout);
    }
    if !output.stderr.is_empty() {
        eprint!("{}", output.stderr);
    }
    Ok(output.exit_code.unwrap_or(1))
}

fn resolve_issue_id(slug: &str, stored: Option<&str>) -> Option<String> {
    if let Some(stored) = stored {
        let trimmed = stored.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_ascii_uppercase());
    }
    regex::Regex::new(r"(?i)([a-z]+-\d+)(?:-|$)")
        .expect("constant issue regex")
        .captures(slug)?
        .get(1)
        .map(|found| found.as_str().to_ascii_uppercase())
}

async fn ahead_of_base(context: &AppContext, path: &str, base: &str) -> Option<f64> {
    let trunk = &context.config.branch.base;
    let requested = if base.is_empty() {
        trunk.as_str()
    } else {
        base
    };
    let base_ref = if requested == trunk || requested == format!("origin/{trunk}") {
        let origin_trunk = format!("origin/{trunk}");
        let fresh = run_git(
            context,
            &context.config.paths.main_clone,
            ["rev-parse", origin_trunk.as_str()],
        )
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        if let Some(sha) = fresh {
            let commit = format!("{sha}^{{commit}}");
            let check = run_git(context, path, ["cat-file", "-e", commit.as_str()])
                .await
                .ok()
                .is_some_and(|output| output.status.success());
            if check {
                sha
            } else {
                format!("origin/{trunk}")
            }
        } else {
            format!("origin/{trunk}")
        }
    } else {
        let local = run_git(context, path, ["rev-parse", requested])
            .await
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        if local.is_some() {
            requested.to_owned()
        } else {
            let origin = format!("origin/{requested}");
            let remote = run_git(context, path, ["rev-parse", &origin])
                .await
                .ok()
                .is_some_and(|output| output.status.success());
            if remote {
                origin
            } else {
                format!("origin/{trunk}")
            }
        }
    };
    commit_count(context, Path::new(path), &format!("{base_ref}..HEAD")).await
}

async fn commit_count(context: &AppContext, path: &Path, range: &str) -> Option<f64> {
    let output = run_git(context, path, ["rev-list", "--count", range])
        .await
        .ok()?
        .checked("git")
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()
        .map(|count| count as f64)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_resolution_matches_inventory_override_and_slug_rules() {
        assert_eq!(
            resolve_issue_id("worktree-david+eng-4959-thing", None).as_deref(),
            Some("ENG-4959")
        );
        assert_eq!(
            resolve_issue_id("gh-970-fix-typo", None).as_deref(),
            Some("GH-970")
        );
        assert_eq!(resolve_issue_id("quick-spike", None), None);
        assert_eq!(resolve_issue_id("eng-123-fix", Some("")), None);
        assert_eq!(
            resolve_issue_id("eng-123-fix", Some("other-2")).as_deref(),
            Some("OTHER-2")
        );
    }

    #[test]
    fn remote_git_urls_convert_to_web_urls() {
        assert_eq!(
            repo_web_url("git@github.com:owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo")
        );
        assert_eq!(
            repo_web_url("https://github.com/owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo")
        );
        assert_eq!(repo_web_url("local-path"), None);
    }

    #[test]
    fn dev_status_projection_preserves_running_queue_restart_and_error_facts() {
        let status = wt_dev::DevServerStatus {
            running: true,
            starting: false,
            crashed: false,
            port: Some(4312),
            url: Some("http://127.0.0.1:4312".to_owned()),
            since: Some(123.5),
            waiting: Some(wt_dev::WaitingStatus {
                rank: 2,
                since: 100.0,
            }),
            rebased_since: Some(true),
            restarts: Some(wt_dev::RestartStatus {
                count: 1,
                last_exit: 75,
            }),
        };
        let projected = remote_dev_status(&status);
        assert_eq!(projected.port, Some(4312));
        assert_eq!(projected.waiting.unwrap().rank, 2);
        assert_eq!(projected.restarts.unwrap().last_exit, 75);

        let row = wt_remote::WorktreeSnapshot {
            slug: "fixture".to_owned(),
            branch: "fixture".to_owned(),
            base: "main".to_owned(),
            path: "/tmp/fixture".to_owned(),
            stage: "fixture".to_owned(),
            deployed: false,
            exists: true,
            status: WorktreeStatus {
                kind: StatusKind::Clean,
                label: "clean".to_owned(),
                age: None,
                log: None,
                pid: None,
                op: None,
            },
            dev: None,
            dev_error: Some("status read failed".to_owned()),
            dirty: false,
            unpushed: None,
            pushed: None,
            ahead_of_base: None,
            issue_id: None,
            issue_url: None,
            github_issue: None,
            github_issue_url: None,
            work: None,
        };
        let value = serde_json::to_value(row).unwrap();
        assert_eq!(value["dev"], serde_json::Value::Null);
        assert_eq!(value["devError"], "status read failed");
        assert!(value.get("dev_error").is_none());
    }
}
