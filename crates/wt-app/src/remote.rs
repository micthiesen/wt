//! Native SSH worker boundary. The controller owns presentation and the
//! configured stable worker binary owns its local worktrees and sessions.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use wt_config::{InstanceRole, LoadOptions, RemoteConfig};
use wt_core::parse_work_status;
use wt_remote::{
    BinaryCandidate, RemoteClient, RemotePlatform, StatusKind, WORKER_PROTOCOL_VERSION, WorkerInfo,
    WorkerRole, WorkerSnapshot, WorktreeSnapshot, WorktreeStatus,
};
use wt_update::{InstallState, VersionId};

use crate::{commands::resolve::run_git, context::AppContext, worktree_facts::push_facts};

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

/// Resolve a build-matched native worker executable, install it in the
/// worker's immutable runtime cache, and bind the returned client to it.
pub async fn prepare_remote_client(
    context: &AppContext,
    remote: &RemoteConfig,
) -> Result<(RemoteClient, WorkerInfo)> {
    let base_client = RemoteClient::new(context.processes.clone(), remote.clone());
    let platform = base_client.probe_platform(&context.cancellation).await?;
    let (candidate, scratch) =
        native_candidate_for_platform(context, &platform, &context.cancellation).await?;
    let client = base_client
        .prepare_runtime(&candidate, &context.cancellation)
        .await?;
    drop(scratch);
    let worker = client.require_worker(&context.cancellation).await?;
    Ok((client, worker))
}

async fn native_candidate_for_platform(
    context: &AppContext,
    platform: &RemotePlatform,
    cancellation: &CancellationToken,
) -> Result<(BinaryCandidate, Option<tempfile::TempDir>)> {
    if platform.target == env!("WT_TARGET") {
        let path = std::env::current_exe()
            .context("resolve running native wt executable")?
            .canonicalize()
            .context("canonicalize running native wt executable")?;
        let output = context
            .processes
            .run(
                wt_platform::process::CommandSpec::new(path.as_os_str()).args(["--_boot-probe"]),
                cancellation,
            )
            .await?
            .checked(&path)?;
        let expected = format!("wt-build-id:{}:{}", env!("WT_BUILD_ID"), env!("WT_TARGET"));
        if output.stdout_text().trim() != expected {
            bail!("running wt binary did not prove its native identity; expected `{expected}`");
        }
        let candidate =
            BinaryCandidate::from_path(path, env!("WT_TARGET"), env!("WT_BUILD_ID")).await?;
        return Ok((candidate, None));
    }

    let options = LoadOptions::default();
    let paths = crate::updates::install_paths(&options)?;
    let store = wt_update::StateStore::new(paths);
    let state = tokio::task::spawn_blocking(move || store.load())
        .await
        .context("join native release identity read")??;
    let launcher_release = std::env::var_os(wt_launcher::INSTALL_VERSION_ENV)
        .map(|value| {
            value.into_string().map_err(|_| {
                anyhow::anyhow!(
                    "launcher-provided {} is not valid UTF-8",
                    wt_launcher::INSTALL_VERSION_ENV
                )
            })
        })
        .transpose()?;
    let version = resolve_controller_release(
        &state,
        launcher_release.as_deref(),
        env!("WT_BUILD_ID"),
        env!("WT_TARGET"),
    )
    .with_context(|| {
        format!(
            "cannot provision {} for remote: running build {} has no matching published native release",
            platform.target,
            env!("WT_BUILD_ID")
        )
    })?;
    let source = wt_update::ReleaseSource::new(crate::updates::repository(&options)?)?;
    let release = tokio::select! {
        biased;
        _ = cancellation.cancelled() => bail!("remote runtime preparation cancelled before release lookup"),
        result = tokio::time::timeout(
            std::time::Duration::from_secs(100),
            source.by_tag(version.release_version()),
        ) => result.context("matching native release lookup timed out")??,
    };
    if release.build_id() != env!("WT_BUILD_ID") {
        bail!(
            "release {} has build {}, but the running controller is {}; refusing a cross-target runtime mismatch",
            release.tag(),
            release.build_id(),
            env!("WT_BUILD_ID")
        );
    }
    let verified = tokio::select! {
        biased;
        _ = cancellation.cancelled() => bail!("remote runtime preparation cancelled before artifact download"),
        result = tokio::time::timeout(
            std::time::Duration::from_secs(100),
            source.download_verified(&release, &platform.target),
        ) => result.context("matching native worker artifact download timed out")??,
    };
    if verified.version().build_id() != env!("WT_BUILD_ID")
        || verified.version().target() != platform.target
        || verified.version().release_version() != version.release_version()
    {
        bail!("verified worker artifact identity does not match the running controller build");
    }
    let scratch = tempfile::Builder::new()
        .prefix("wt-remote-runtime-")
        .tempdir()
        .context("create native worker candidate directory")?;
    let local_path = scratch.path().join("wt");
    tokio::fs::write(&local_path, verified.app_binary())
        .await
        .context("write verified native worker candidate")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&local_path, std::fs::Permissions::from_mode(0o700))
            .await
            .context("make verified native worker candidate executable")?;
    }
    let candidate =
        BinaryCandidate::from_path(local_path, platform.target.clone(), env!("WT_BUILD_ID"))
            .await?;
    Ok((candidate, Some(scratch)))
}

fn resolve_controller_release(
    state: &InstallState,
    launcher_release: Option<&str>,
    build_id: &str,
    target: &str,
) -> Result<VersionId> {
    if let Some(release) = launcher_release {
        // The stable launcher supplies this tag from the same VersionId as the
        // build and target identity it exports. Validate the component before
        // using it in a release lookup; the release manifest must still prove
        // that the tag actually contains this build and target.
        return VersionId::new(release, build_id, target)
            .context("validate launcher-provided release/build/target identity");
    }

    state
        .current
        .iter()
        .chain(state.last_good.iter())
        .chain(
            state
                .pending_boot
                .iter()
                .flat_map(|pending| std::iter::once(&pending.candidate).chain(pending.fallback.iter())),
        )
        .chain(state.history.iter().flat_map(|entry| entry.from.iter().chain(entry.to.iter())))
        .find(|version| version.build_id() == build_id && version.target() == target)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "running build {build_id} for target {target} is absent from launcher identity and retained install history"
            )
        })
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
    let main_first_parents = if has_non_main {
        main_first_parent_shas(context).await
    } else {
        None
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
        let main_first_parents = main_first_parents.clone();
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
            build_snapshot_row(
                row_context,
                snapshot,
                entry,
                remote_url,
                dev_row,
                main_first_parents,
            )
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
    main_first_parents: Option<HashSet<String>>,
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
    let push = push_facts(&context, Path::new(&target.path), &target.branch, &base).await;
    let path = std::path::PathBuf::from(&target.path);
    let stage_prefix = context.config.stage.prefix.clone();
    let (exists, deployed) = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
        let exists = path.try_exists()?;
        let deployed = matches!(
            wt_sst::observe_local_deployment(&path, &stage_prefix),
            wt_sst::DeploymentObservation::Deployed { .. }
        );
        Ok((exists, deployed))
    })
    .await
    .context("inspect remote snapshot stage")??;
    let issue_number = entry
        .as_ref()
        .and_then(|entry| entry.get("githubIssue"))
        .and_then(Value::as_u64);
    let github_issue_url = issue_number.and_then(|number| {
        repo_web_url(remote_url.as_deref()?).map(|repo| format!("{repo}/issues/{number}"))
    });
    let status = remote_worktree_status(
        &context,
        &snapshot,
        exists,
        git_status.is_some_and(|status| status.dirty),
        entry.as_ref(),
        main_first_parents.as_ref(),
    )
    .await?;
    let dirty = git_status.is_some_and(|status| status.dirty);
    Ok(WorktreeSnapshot {
        slug: slug.to_owned(),
        branch: target.branch.clone(),
        base,
        path: target.path.clone(),
        stage: target.stage.clone(),
        deployed,
        exists,
        status,
        // A row error stays explicit while a null status preserves unknown.
        // In particular, a failed status read is never rendered as stopped.
        dev: dev_row
            .as_ref()
            .and_then(|row| row.status.as_ref())
            .map(remote_dev_status),
        dev_error: dev_row.and_then(|row| row.error),
        dirty,
        unpushed: push.unpushed.map(|count| count as f64),
        pushed: push.pushed,
        ahead_of_base: push.ahead_of_base.map(|count| count as f64),
        issue_id,
        issue_url,
        github_issue: issue_number,
        github_issue_url,
        work,
    })
}

async fn remote_worktree_status(
    context: &AppContext,
    snapshot: &wt_vcs::WorktreeSnapshot,
    exists: bool,
    dirty: bool,
    entry: Option<&Value>,
    main_first_parents: Option<&HashSet<String>>,
) -> Result<WorktreeStatus> {
    let slug = snapshot.worktree.target.slug();
    if let Some(lock) = crate::commands::diagnostics::operation_lock(context, slug).await? {
        let log_dir = context.config.paths.log_dir.clone();
        let log_slug = slug.to_owned();
        let log = tokio::task::spawn_blocking(move || latest_destroy_log(&log_dir, &log_slug))
            .await
            .ok()
            .flatten();
        return Ok(WorktreeStatus {
            kind: StatusKind::Busy,
            label: operation_lock_label(&lock),
            age: operation_lock_age(&lock),
            log,
            pid: lock.pid.map(i64::from),
            op: lock.op,
        });
    }
    if !exists {
        return Ok(WorktreeStatus {
            kind: StatusKind::Missing,
            label: "missing".into(),
            age: None,
            log: None,
            pid: None,
            op: None,
        });
    }
    if let Some(error) = &snapshot.error {
        bail!("cannot read Git status for remote worktree {slug}: {error}");
    }
    if snapshot.status.is_none() {
        bail!("cannot read Git status for remote worktree {slug}");
    }

    let target = &snapshot.worktree.target;
    let ref_name = format!("refs/heads/{}", target.branch);
    let upstream = run_git(
        context,
        &target.path,
        [
            "for-each-ref",
            "--format=%(upstream:track)",
            ref_name.as_str(),
        ],
    )
    .await?
    .checked("git for-each-ref")?;
    if String::from_utf8_lossy(&upstream.stdout).trim() == "[gone]" {
        return Ok(WorktreeStatus {
            kind: StatusKind::Gone,
            label: "gone (squash-merged or deleted)".into(),
            age: None,
            log: None,
            pid: None,
            op: None,
        });
    }

    if branch_is_merged(context, target, entry, main_first_parents).await? {
        return Ok(WorktreeStatus {
            kind: StatusKind::Merged,
            label: format!("merged into origin/{}", context.config.branch.base),
            age: None,
            log: None,
            pid: None,
            op: None,
        });
    }
    let (kind, label) = if dirty {
        (StatusKind::Dirty, "dirty")
    } else {
        (StatusKind::Clean, "clean")
    };
    Ok(WorktreeStatus {
        kind,
        label: label.into(),
        age: None,
        log: None,
        pid: None,
        op: None,
    })
}

async fn main_first_parent_shas(context: &AppContext) -> Option<HashSet<String>> {
    let trunk_ref = format!("origin/{}", context.config.branch.base);
    let output = run_git(
        context,
        &context.config.paths.main_clone,
        ["rev-list", "--first-parent", trunk_ref.as_str()],
    )
    .await
    .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .filter(|sha| !sha.is_empty())
            .collect(),
    )
}

async fn branch_is_merged(
    context: &AppContext,
    target: &wt_core::WorktreeTarget,
    entry: Option<&Value>,
    main_first_parents: Option<&HashSet<String>>,
) -> Result<bool> {
    let branch_sha = run_git(
        context,
        &target.path,
        ["rev-parse", "--verify", target.branch.as_str()],
    )
    .await?
    .checked("git rev-parse branch")?;
    let branch_sha = String::from_utf8_lossy(&branch_sha.stdout)
        .trim()
        .to_owned();
    let trunk_ref = format!("origin/{}", context.config.branch.base);
    let trunk_sha = run_git(
        context,
        &context.config.paths.main_clone,
        ["rev-parse", "--verify", trunk_ref.as_str()],
    )
    .await?
    .checked("git rev-parse trunk")?;
    if branch_sha == String::from_utf8_lossy(&trunk_sha.stdout).trim() {
        return Ok(false);
    }

    let ancestry = run_git(
        context,
        &context.config.paths.main_clone,
        [
            "merge-base",
            "--is-ancestor",
            branch_sha.as_str(),
            trunk_ref.as_str(),
        ],
    )
    .await?;
    if ancestry.status.code() == Some(1) {
        return Ok(false);
    }
    if !ancestry.status.success() {
        bail!(
            "cannot check whether remote branch {} is merged: {}",
            target.branch,
            ancestry.stderr_text().trim()
        );
    }
    let first_parents = main_first_parents.ok_or_else(|| {
        anyhow::anyhow!(
            "cannot determine origin/{} first-parent history for remote branch {}",
            context.config.branch.base,
            target.branch
        )
    })?;
    if first_parents.contains(&branch_sha) {
        return Ok(false);
    }

    let fork_base = entry
        .and_then(|entry| entry.get("baseSha"))
        .and_then(Value::as_str)
        .or_else(|| {
            entry
                .and_then(|entry| entry.get("baseBranch"))
                .and_then(Value::as_str)
        });
    let Some(fork_base) = fork_base else {
        // Legacy worktrees without a fork record retain the historical
        // behavior: ancestry is sufficient evidence of real work.
        return Ok(true);
    };
    let range = format!("{fork_base}..{branch_sha}");
    let count = run_git(
        context,
        &target.path,
        ["rev-list", "--count", range.as_str()],
    )
    .await?
    .checked("git rev-list branch work")?;
    let count = String::from_utf8_lossy(&count.stdout)
        .trim()
        .parse::<u64>()
        .context("parse commits since recorded fork base")?;
    Ok(count > 0)
}

fn operation_lock_label(lock: &crate::commands::diagnostics::OperationLock) -> String {
    match (&lock.op, &lock.phase) {
        (Some(operation), Some(phase)) if operation != phase => {
            format!("{operation}: {phase}")
        }
        (Some(operation), _) => operation.clone(),
        (_, Some(phase)) => phase.clone(),
        _ => "busy".into(),
    }
}

fn operation_lock_age(lock: &crate::commands::diagnostics::OperationLock) -> Option<String> {
    let started = lock
        .phase_started
        .as_deref()
        .or(lock.started_at.as_deref())?;
    let started = OffsetDateTime::parse(started, &Rfc3339).ok()?;
    let seconds = (OffsetDateTime::now_utc() - started).whole_seconds().max(0) as u64;
    Some(if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86400)
    })
}

fn latest_destroy_log(log_dir: &Path, slug: &str) -> Option<String> {
    let prefix = format!("{slug}-");
    let entries = std::fs::read_dir(log_dir).ok()?;
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let stamp = name.strip_prefix(&prefix)?.strip_suffix(".log")?;
            let bytes = stamp.as_bytes();
            if bytes.len() < 11
                || !bytes[..4].iter().all(u8::is_ascii_digit)
                || bytes[4] != b'-'
                || !bytes[5..7].iter().all(u8::is_ascii_digit)
                || bytes[7] != b'-'
                || !bytes[8..10].iter().all(u8::is_ascii_digit)
                || bytes[10] != b'T'
            {
                return None;
            }
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path.to_string_lossy().into_owned())
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
    let (client, worker) = prepare_remote_client(context, remote).await?;
    remote_admin_with_client(context, &client, &worker, args).await
}

/// Execute an administrative command on an already prepared native worker.
/// Callers can reuse the verified runtime for prerequisite and user commands.
pub async fn remote_admin_with_client(
    context: &AppContext,
    client: &RemoteClient,
    worker: &WorkerInfo,
    args: &[String],
) -> Result<i32> {
    if worker.role != WorkerRole::Worker || worker.protocol != WORKER_PROTOCOL_VERSION {
        bail!("remote administrative command requires a prepared compatible worker");
    }
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
    fn launcher_release_identity_survives_bounded_history_eviction() {
        use wt_update::{Channel, StateHistoryEntry};

        let mut state = InstallState::new(Channel::Stable);
        let current = VersionId::new("v2.0.0", "b".repeat(40), "aarch64-apple-darwin").unwrap();
        state.current = Some(current.clone());
        state.last_good = Some(current.clone());
        state.history = (0..64)
            .map(|index| StateHistoryEntry {
                at_unix: index,
                operation: "update".to_owned(),
                from: None,
                to: Some(
                    VersionId::new(
                        format!("v1.{}.0", index),
                        format!("{index:040x}"),
                        "aarch64-apple-darwin",
                    )
                    .unwrap(),
                ),
                detail: None,
            })
            .collect();
        let old_build = "a".repeat(40);

        assert!(
            resolve_controller_release(&state, None, &old_build, "aarch64-apple-darwin").is_err()
        );
        assert_eq!(
            resolve_controller_release(
                &state,
                Some("preview-old-build"),
                &old_build,
                "aarch64-apple-darwin"
            )
            .unwrap(),
            VersionId::new("preview-old-build", old_build, "aarch64-apple-darwin").unwrap()
        );
    }

    #[test]
    fn launcher_release_identity_rejects_unsafe_or_mismatched_target() {
        let state = InstallState::new(wt_update::Channel::Stable);
        let build = "a".repeat(40);
        assert!(
            resolve_controller_release(&state, Some("../escape"), &build, "aarch64-apple-darwin")
                .is_err()
        );
        assert!(resolve_controller_release(&state, Some("v1.2.3"), &build, "../target").is_err());
    }

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
