use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::{Value, json};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use wt_core::worktree_target_key;
use wt_lifecycle::{RemovalRevision, RemoveOptions};
use wt_platform::process::CommandSpec;
use wt_vcs::WorktreeRecord;

use crate::commands::resolve::run_git;
use wt_app_core::context::AppContext;

const VERSION: u64 = 1;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Args)]
pub struct DestroyWorkerArgs {
    #[arg(value_name = "JOB_ID")]
    pub job_id: String,
}

#[derive(Debug, Clone)]
pub struct DestroyOptions {
    pub force: bool,
    pub delete_branch: bool,
    pub landed: bool,
    pub destroy_stage: bool,
    pub expected_revision: Option<RemovalRevision>,
    pub removed_snapshot: Option<wt_store::RemovedWorktree>,
}

pub async fn start_remove(
    ctx: &AppContext,
    record: &WorktreeRecord,
    options: DestroyOptions,
) -> Result<String> {
    let jobs = job_dir(ctx);
    tokio::fs::create_dir_all(&jobs)
        .await
        .with_context(|| format!("create job directory {}", jobs.display()))?;
    reap_abandoned(ctx, &jobs).await?;
    start_remove_inner(ctx, record, options).await
}

pub async fn start_removals(
    ctx: &AppContext,
    requests: &[(WorktreeRecord, DestroyOptions)],
) -> Vec<Result<String>> {
    let jobs = job_dir(ctx);
    let setup = async {
        tokio::fs::create_dir_all(&jobs)
            .await
            .with_context(|| format!("create job directory {}", jobs.display()))?;
        reap_abandoned(ctx, &jobs).await
    }
    .await;
    if let Err(error) = setup {
        let message = format!("{error:#}");
        return requests
            .iter()
            .map(|_| Err(anyhow::anyhow!(message.clone())))
            .collect();
    }
    let mut results = Vec::with_capacity(requests.len());
    for (record, options) in requests {
        results.push(start_remove_inner(ctx, record, options.clone()).await);
    }
    results
}

async fn start_remove_inner(
    ctx: &AppContext,
    record: &WorktreeRecord,
    options: DestroyOptions,
) -> Result<String> {
    let jobs = job_dir(ctx);
    let head = record
        .head_sha
        .as_ref()
        .context("cannot schedule removal without an inventory HEAD")?;
    let service = crate::lifecycle_ops::service(ctx)?;
    let revision = match options.expected_revision.clone() {
        Some(revision) => revision,
        None => {
            service
                .removal_revision(&record.target, options.landed, &ctx.cancellation)
                .await?
        }
    };
    if revision.key != worktree_target_key(&record.target)
        || revision.path != record.target.path
        || revision.branch != record.target.branch
        || revision.head != *head
    {
        bail!("removal snapshot does not match the scheduled worktree");
    }
    let job_id = new_id();
    let log_path = jobs.join(format!("{job_id}.log"));
    let job_path = jobs.join(format!("{job_id}.json"));
    let archive_key = worktree_target_key(&record.target);
    let lookup = archive_key.clone();
    let was_archived = ctx
        .database
        .call(move |store| Ok(store.read_archived_keys()?.contains(&lookup)))
        .await?;
    let record_json = json!({
        "version": VERSION,
        "id": job_id,
        "target": {
            "key": worktree_target_key(&record.target),
            "path": record.target.path,
            "branch": record.target.branch,
            "head": head,
            "revision": revision,
        },
        "operation": "remove",
        "wasArchived": was_archived,
        "options": {
            "force": options.force,
            "deleteBranch": options.delete_branch,
            "landed": options.landed,
            "destroyStage": options.destroy_stage,
            "removedSnapshot": options.removed_snapshot,
        },
        "createdAt": now_string(),
        "workerPid": null,
        "workerStartIdentity": null,
        "state": "starting",
        "error": null,
        "logPath": log_path,
        "completedAt": null,
    });
    atomic_write(&job_path, &record_json).await?;

    let executable = std::env::current_exe().context("resolve current wt executable")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open job log {}", log_path.display()))?;
    let stderr = log.try_clone().context("clone job log handle")?;
    let mut command = Command::new(executable);
    command
        .arg("_destroy")
        .arg(&job_id)
        .current_dir(&ctx.config.paths.main_clone)
        .env("HOME", &ctx.home)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    if let Some(path) = &ctx.config.repository_config {
        command.env("WT_REPO_CONFIG", path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .context("start background wt destroy worker")?;
    let pid = child.id();

    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    loop {
        let current = read_job(&job_path).await?;
        if current["state"] == "running" && current["workerPid"].as_u64() == Some(pid as u64) {
            // The job outlives this request; still reap its OS child when it
            // finishes, including a crash before writing terminal state.
            std::thread::spawn(move || child.wait());
            return Ok(job_id);
        }
        if current["state"] == "succeeded" {
            std::thread::spawn(move || child.wait());
            return Ok(job_id);
        }
        if current["state"] == "failed" {
            std::thread::spawn(move || child.wait());
            bail!(
                "background worker exited during startup; see {}",
                log_path.display()
            );
        }
        if child.try_wait()?.is_some() {
            // The worker may have recorded a terminal state between the read
            // above and its exit; that record, not the exit, decides.
            let current = read_job(&job_path).await?;
            if current["state"] == "succeeded" {
                return Ok(job_id);
            }
            if current["state"] == "failed" {
                bail!(
                    "background worker exited during startup; see {}",
                    log_path.display()
                );
            }
            bail!(
                "background worker exited before startup acknowledgement; see {}",
                log_path.display()
            );
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "background worker did not acknowledge startup; see {}",
                log_path.display()
            );
        }
        sleep(Duration::from_millis(30)).await;
    }
}

/// A started removal whose terminal outcome cannot be established safely.
#[derive(Debug)]
pub struct RemovalUncertain(pub String);

impl std::fmt::Display for RemovalUncertain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for RemovalUncertain {}

/// A TUI write finishes when its durable job does, not at startup. This wait
/// belongs to the host's accepted-command drain and deliberately ignores UI
/// cancellation. CLI background mode continues to return the startup ack.
pub async fn wait_for_completion(ctx: &AppContext, job_id: &str) -> Result<()> {
    match wait_for_completion_or_shutdown(ctx, job_id, None).await? {
        RemovalWait::Completed => Ok(()),
        RemovalWait::ContinuesInBackground => unreachable!("without shutdown, wait cannot detach"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovalWait {
    Completed,
    /// The durable worker was proven alive after its UI owner shut down.
    /// The caller must report ambiguity, never successful completion.
    ContinuesInBackground,
}

pub async fn wait_for_completion_or_shutdown(
    ctx: &AppContext,
    job_id: &str,
    shutdown: Option<&CancellationToken>,
) -> Result<RemovalWait> {
    validate_job_id(job_id)?;
    let path = job_dir(ctx).join(format!("{job_id}.json"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(29 * 60);
    let mut check_process_at = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let job = read_job(&path).await.map_err(|error| {
            RemovalUncertain(format!(
                "cannot read accepted removal job {job_id}: {error:#}; inspect {} before retrying",
                path.display()
            ))
        })?;
        match job["state"].as_str() {
            Some("succeeded") => return Ok(RemovalWait::Completed),
            Some("failed") => bail!("{}", job["error"].as_str().unwrap_or("removal failed")),
            Some("starting" | "running") => {
                if shutdown.is_some_and(CancellationToken::is_cancelled)
                    && job["state"].as_str() == Some("running")
                {
                    if worker_is_proven_alive(&job, &ctx.processes).await {
                        tracing::info!(
                            job_id,
                            "detaching terminal wait; durable removal worker continues in background"
                        );
                        return Ok(RemovalWait::ContinuesInBackground);
                    }
                    return Err(RemovalUncertain(format!(
                        "removal job {job_id} is running but its worker identity cannot be verified; inspect {} before retrying",
                        path.display()
                    )).into());
                }
            }
            _ => {
                return Err(RemovalUncertain(format!(
                    "invalid removal job state; inspect {}",
                    path.display()
                ))
                .into());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(RemovalUncertain(format!("removal job {job_id} has not reported completion; it may still be running; inspect {} before retrying", path.display())).into());
        }
        if tokio::time::Instant::now() >= check_process_at {
            check_process_at = tokio::time::Instant::now() + Duration::from_secs(2);
            if let (Some(pid), Some(expected)) = (
                job["workerPid"]
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok()),
                job["workerStartIdentity"]
                    .as_str()
                    .filter(|identity| !identity.is_empty()),
            ) {
                let actual = process_start_identity(pid, &ctx.processes).await;
                if pid_is_gone(pid) || identities_conflict(expected, &actual) {
                    // An unrelated reused PID cannot own this job. Do not
                    // rewrite archive state here: the durable record does
                    // not prove where a crashed worker stopped.
                    let final_job = read_job(&path).await.map_err(|error| {
                        RemovalUncertain(format!(
                            "cannot reconcile removal job {job_id}: {error:#}"
                        ))
                    })?;
                    if final_job["state"] == "running" || final_job["state"] == "starting" {
                        return Err(RemovalUncertain(format!("removal worker {pid} exited without a terminal result; inspect {} before retrying", path.display())).into());
                    }
                    continue;
                }
            }
        }
        if let Some(shutdown) = shutdown.filter(|shutdown| !shutdown.is_cancelled()) {
            tokio::select! {
                _ = sleep(Duration::from_millis(100)) => {},
                _ = shutdown.cancelled() => {},
            }
        } else {
            sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn worker_is_proven_alive(job: &Value, runner: &wt_platform::process::ProcessRunner) -> bool {
    let Some(pid) = job["workerPid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
    else {
        return false;
    };
    let Some(expected) = job["workerStartIdentity"]
        .as_str()
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let actual = process_start_identity(pid, runner).await;
    !actual.is_empty() && actual == expected
}

pub async fn run_worker(ctx: &AppContext, args: &DestroyWorkerArgs) -> Result<i32> {
    validate_job_id(&args.job_id)?;
    let job_path = job_dir(ctx).join(format!("{}.json", args.job_id));
    let mut job = read_job(&job_path).await?;
    if job["version"].as_u64() != Some(VERSION) || job["id"] != args.job_id {
        bail!("job record identity or version mismatch");
    }
    if job["state"] != "starting" {
        bail!("destroy job is not awaiting a worker");
    }
    job["workerPid"] = json!(std::process::id());
    job["workerStartIdentity"] =
        json!(process_start_identity(std::process::id(), &ctx.processes).await);
    atomic_write(&job_path, &job).await?;
    let archive_key = job["target"]["key"]
        .as_str()
        .context("job missing worktree key")?
        .to_owned();
    let key = archive_key.clone();
    let job_id = args.job_id.clone();
    let path = job["target"]["path"]
        .as_str()
        .context("job missing path")?
        .to_owned();
    let branch = job["target"]["branch"]
        .as_str()
        .context("job missing branch")?
        .to_owned();
    let accepted = ctx
        .database
        .call(move |store| Ok(store.begin_removal(&key, &job_id, &path, &branch)?))
        .await?;
    if !accepted {
        job["state"] = json!("failed");
        job["error"] = json!("another removal is already active for this worktree");
        job["completedAt"] = json!(now_string());
        atomic_write(&job_path, &job).await?;
        bail!("another removal is already active for this worktree");
    }
    if job["wasArchived"].is_boolean() {
        let key = archive_key;
        ctx.database
            .call(move |store| {
                store.set_archived(&key, true)?;
                Ok(())
            })
            .await?;
    }
    job["state"] = json!("running");
    atomic_write(&job_path, &job).await?;

    let result = execute_job(ctx, &job).await;
    match result {
        Ok(()) => {
            job["state"] = json!("succeeded");
            job["error"] = Value::Null;
            job["completedAt"] = json!(now_string());
            atomic_write(&job_path, &job).await?;
            publish_removal_result(ctx, &job).await?;
            tracing::info!(
                target: "wt_attention",
                event_channel = "attention",
                event_source = job["target"]["key"].as_str().unwrap_or("wt"),
                event_text = "Worktree removed",
                "background removal completed"
            );
            Ok(0)
        }
        Err(error) => {
            // Keep an accepted removal out of Inbox even if cleanup fails.
            // The durable job and attention event retain the failure for retry.
            job["state"] = json!("failed");
            job["error"] = json!(format!("{error:#}"));
            job["completedAt"] = json!(now_string());
            atomic_write(&job_path, &job).await?;
            publish_removal_result(ctx, &job).await?;
            tracing::error!(
                target: "wt_attention",
                event_channel = "attention",
                event_source = job["target"]["key"].as_str().unwrap_or("wt"),
                event_text = %format!("Removal failed: {error:#}"),
                "background removal failed"
            );
            Err(error)
        }
    }
}

async fn publish_removal_result(ctx: &AppContext, job: &Value) -> Result<()> {
    let (Some(key), Some(job_id)) = (job["target"]["key"].as_str(), job["id"].as_str()) else {
        return Ok(());
    };
    let key = key.to_owned();
    let job_id = job_id.to_owned();
    let error = match job["state"].as_str() {
        Some("succeeded") => None,
        Some("failed") => Some(job["error"].as_str().unwrap_or("removal failed").to_owned()),
        _ => return Ok(()),
    };
    ctx.database
        .call(move |store| {
            store.finish_removal(&key, &job_id, error.as_deref())?;
            Ok(())
        })
        .await
}

/// Repair interrupted result publication from the durable job. This runs in
/// the metadata source, never on the terminal input path. Unknown workers
/// remain active; only a proven exit permits retry.
pub async fn reconcile_removals(ctx: &AppContext, state: &Value) -> Result<()> {
    let Some(removals) = state["removals"].as_object() else {
        return Ok(());
    };
    // One unreadable or racing entry must not stop the others, nor the
    // metadata snapshot that runs this reconciliation.
    for (key, removal) in removals {
        if let Err(error) = reconcile_removal(ctx, key, removal).await {
            tracing::warn!(%key, error = %format!("{error:#}"), "cannot reconcile accepted removal; retaining its state");
        }
    }
    Ok(())
}

async fn reconcile_removal(ctx: &AppContext, key: &str, removal: &Value) -> Result<()> {
    if !removal["error"].is_null() {
        return Ok(());
    }
    let Some(job_id) = removal["jobId"].as_str() else {
        return Ok(());
    };
    validate_job_id(job_id)?;
    let path = job_dir(ctx).join(format!("{job_id}.json"));
    let mut job = match read_job(&path).await {
        Ok(job) => job,
        Err(error) => {
            tracing::warn!(%job_id, %error, "cannot reconcile accepted removal; retaining its state");
            return Ok(());
        }
    };
    if job["id"].as_str() != Some(job_id) || job["target"]["key"].as_str() != Some(key) {
        tracing::warn!(%job_id, "removal journal identity mismatch; retaining its state");
        return Ok(());
    }
    if matches!(job["state"].as_str(), Some("succeeded" | "failed")) {
        return publish_removal_result(ctx, &job).await;
    }
    let Some(pid) = job["workerPid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
    else {
        return Ok(());
    };
    let expected = job["workerStartIdentity"].as_str().unwrap_or_default();
    let actual = process_start_identity(pid, &ctx.processes).await;
    let exited = pid_is_gone(pid) || identities_conflict(expected, &actual);
    if !exited {
        return Ok(());
    }
    job = read_job(&path).await?;
    if matches!(job["state"].as_str(), Some("starting" | "running")) {
        job["state"] = json!("failed");
        job["error"] = json!(
            "worker process disappeared before recording completion; inspect the checkout before retrying"
        );
        job["completedAt"] = json!(now_string());
        atomic_write(&path, &job).await?;
    }
    publish_removal_result(ctx, &job).await
}

async fn execute_job(ctx: &AppContext, job: &Value) -> Result<()> {
    let target = &job["target"];
    let key = target["key"].as_str().context("job missing target key")?;
    let path = target["path"].as_str().context("job missing target path")?;
    let branch = target["branch"]
        .as_str()
        .context("job missing target branch")?;
    let expected_head = target["head"].as_str().context("job missing target head")?;
    let revision: RemovalRevision = serde_json::from_value(target["revision"].clone())
        .context("job missing valid removal revision")?;
    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    let row = rows
        .into_iter()
        .find(|row| {
            !row.is_main
                && worktree_target_key(&row.target) == key
                && row.target.path == path
                && row.target.branch == branch
        })
        .context("scheduled worktree is no longer the same local target")?;
    let actual_head = run_git(ctx, Path::new(path), ["rev-parse", "HEAD"])
        .await?
        .stdout_text();
    if actual_head.trim() != expected_head {
        bail!("scheduled worktree HEAD changed before background removal");
    }
    if job["operation"] != "remove" {
        bail!("unsupported destroy job operation");
    }
    let options = &job["options"];
    let dev_port = crate::host_cleanup::stored_dev_port(ctx, row.target.slug()).await;
    let service = crate::lifecycle_ops::service(ctx)?;
    let removed = service
        .remove_with_revision(
            &row.target,
            RemoveOptions {
                force: options["force"].as_bool().unwrap_or(false),
                delete_branch: options["deleteBranch"].as_bool().unwrap_or(true),
                landed: options["landed"].as_bool().unwrap_or(false),
                destroy_stage: options["destroyStage"].as_bool().unwrap_or(false),
                removed_snapshot: options["removedSnapshot"]
                    .as_null()
                    .is_none()
                    .then(|| serde_json::from_value(options["removedSnapshot"].clone()))
                    .transpose()
                    .context("background job has invalid removed-history snapshot")?,
            },
            &revision,
            &ctx.cancellation,
        )
        .await?;
    // Past the commit point, leftovers (such as an undeletable quarantine)
    // are warnings: the removal succeeded and has nothing to retry.
    for warning in removed.warnings {
        eprintln!("warning: {warning}");
        tracing::warn!(
            target: "wt_attention",
            event_channel = "attention",
            event_source = key,
            event_text = %format!("Removal warning: {warning}"),
            "background removal left a warning"
        );
    }
    if !removed.removed {
        bail!("background removal did not remove the worktree; see job log");
    }
    if removed.removed {
        for warning in crate::host_cleanup::after_remove(ctx, row.target.slug(), dev_port).await {
            eprintln!("warning: {warning}");
        }
    }
    Ok(())
}

fn job_dir(ctx: &AppContext) -> PathBuf {
    ctx.config.paths.lock_dir.join("destroy-jobs")
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{:x}", std::process::id(), nanos)
}

fn now_string() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn validate_job_id(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        bail!("invalid destroy job id");
    }
    Ok(())
}

async fn read_job(path: &Path) -> Result<Value> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read destroy job {}", path.display()))?;
    serde_json::from_str(&text).context("parse destroy job record")
}

async fn atomic_write(path: &Path, value: &Value) -> Result<()> {
    let tmp = path.with_extension(format!("json.{}.tmp", new_id()));
    let encoded = serde_json::to_vec_pretty(value)?;
    tokio::fs::write(&tmp, encoded)
        .await
        .with_context(|| format!("write destroy job {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("publish destroy job {}", path.display()))?;
    Ok(())
}

async fn reap_abandoned(ctx: &AppContext, directory: &Path) -> Result<()> {
    let mut entries = tokio::fs::read_dir(directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let Ok(mut job) = read_job(&entry.path()).await else {
            continue;
        };
        if job["state"] != "running" {
            continue;
        }
        let Some(pid) = job["workerPid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
        else {
            continue;
        };
        let expected = job["workerStartIdentity"].as_str().unwrap_or_default();
        let actual = process_start_identity(pid, &ctx.processes).await;
        // An empty identity is either an exited worker or a failed probe; only
        // a confirmed missing PID or a reused one abandons the job.
        let abandoned = if actual.is_empty() {
            pid_is_gone(pid)
        } else {
            identities_conflict(expected, &actual)
        };
        if !abandoned {
            continue;
        }
        // A worker that finished between the first read and the probe has
        // already recorded its own outcome.
        let Ok(current) = read_job(&entry.path()).await else {
            continue;
        };
        if current["state"] != "running" || current["workerPid"] != job["workerPid"] {
            continue;
        }
        job["state"] = json!("failed");
        job["error"] = json!("worker process disappeared before recording completion");
        job["completedAt"] = json!(now_string());
        atomic_write(&entry.path(), &job).await?;
        publish_removal_result(ctx, &job).await?;
        tracing::error!(
            target: "wt_attention",
            event_channel = "attention",
            event_source = job["target"]["key"].as_str().unwrap_or("wt"),
            event_text = "Removal failed: worker process disappeared before recording completion; checkout remains in Removing",
            "background removal worker disappeared"
        );
    }
    Ok(())
}

/// True only when the OS confirms no process has this PID.
fn pid_is_gone(pid: u32) -> bool {
    #[cfg(unix)]
    {
        i32::try_from(pid)
            .ok()
            .filter(|pid| *pid > 0)
            .is_some_and(|pid| {
                // SAFETY: signal zero only probes the checked positive PID.
                (unsafe { libc::kill(pid, 0) }) == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            })
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

async fn process_start_identity(pid: u32, runner: &wt_platform::process::ProcessRunner) -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some(rest) = stat.rsplit_once(") ").map(|(_, rest)| rest)
            && let Some(start) = rest.split_whitespace().nth(19)
        {
            return format!("linux:{start}");
        }
    }
    // `lstart` is formatted in the caller's locale and time zone; pin both so
    // every wt process, whatever its environment, prints the same identity.
    let mut spec = CommandSpec::new("ps").args(["-o", "lstart=", "-p", &pid.to_string()]);
    spec.env = vec![
        ("LC_ALL".into(), Some("C".into())),
        ("TZ".into(), Some("UTC".into())),
    ];
    spec.timeout = Duration::from_secs(2);
    runner
        .run(spec, &CancellationToken::new())
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| format!("{PS_IDENTITY}:{}", output.stdout_text().trim()))
        .unwrap_or_default()
}

/// Scheme of [`process_start_identity`] on macOS. Builds before it wrote
/// `ps:` in their own locale, which is not comparable.
const PS_IDENTITY: &str = "ps-c-utc";

/// Proof that a PID now belongs to another process: two known identities
/// of the same scheme that differ. A record from another scheme proves
/// nothing, so a worker that predates an update is never declared gone.
fn identities_conflict(expected: &str, actual: &str) -> bool {
    let scheme = |identity: &str| {
        identity
            .split_once(':')
            .map(|(scheme, _)| scheme.to_owned())
    };
    !expected.is_empty()
        && !actual.is_empty()
        && scheme(expected) == scheme(actual)
        && expected != actual
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_app_core::test_support::CommandFixture;

    #[test]
    fn only_same_scheme_identities_can_prove_a_reused_pid() {
        let now = "ps-c-utc:Mon Oct 26 17:50:50 2026";
        assert!(!identities_conflict(now, now));
        assert!(identities_conflict(
            now,
            "ps-c-utc:Mon Oct 26 17:50:51 2026"
        ));
        // A record from an older build, in whatever locale it ran, and an
        // unreadable probe prove nothing.
        assert!(!identities_conflict("ps:Mon 26 Oct 10:50:50 2026", now));
        assert!(!identities_conflict("", now));
        assert!(!identities_conflict(now, ""));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn start_identity_ignores_the_callers_locale_and_zone() {
        let runner = wt_platform::process::ProcessRunner::default();
        let pid = std::process::id();
        let identity = process_start_identity(pid, &runner).await;
        assert!(identity.starts_with("ps-c-utc:"), "{identity}");
        // The pinned environment wins over anything the caller inherited.
        let mut spec = CommandSpec::new("ps").args(["-o", "lstart=", "-p", &pid.to_string()]);
        spec.env = vec![
            ("LC_ALL".into(), Some("en_CA.UTF-8".into())),
            ("TZ".into(), Some("America/Vancouver".into())),
        ];
        let local = runner.run(spec, &CancellationToken::new()).await.unwrap();
        let local = local.stdout_text();
        assert_ne!(format!("ps-c-utc:{}", local.trim()), identity);
        assert_eq!(process_start_identity(pid, &runner).await, identity);
    }

    #[tokio::test]
    async fn accepted_failure_is_published_and_a_lost_success_reply_is_reconciled() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let row = crate::lifecycle_ops::resolve_key(ctx, "one").await.unwrap();
        let revision = crate::lifecycle_ops::service(ctx)
            .unwrap()
            .removal_revision(&row.target, false, &ctx.cancellation)
            .await
            .unwrap();
        let jobs = job_dir(ctx);
        tokio::fs::create_dir_all(&jobs).await.unwrap();
        let path = jobs.join("1-a.json");
        let job = json!({
            "version": VERSION, "id": "1-a", "state": "starting",
            "wasArchived": false, "operation": "remove",
            "target": {
                "key": "one", "path": row.target.path, "branch": row.target.branch,
                "head": "old-head", "revision": revision,
            }
        });
        atomic_write(&path, &job).await.unwrap();
        let error = run_worker(
            ctx,
            &DestroyWorkerArgs {
                job_id: "1-a".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("HEAD changed"));
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert!(
            state["removals"]["one"]["error"]
                .as_str()
                .unwrap()
                .contains("HEAD changed")
        );
        assert!(Path::new(&row.target.path).exists());

        let target_path = row.target.path.clone();
        let branch = row.target.branch.clone();
        ctx.database
            .call(move |store| {
                assert!(store.begin_removal("one", "2-b", &target_path, &branch)?);
                Ok(())
            })
            .await
            .unwrap();
        let success =
            json!({"id": "2-b", "state": "succeeded", "target": {"key": "one"}, "error": null});
        atomic_write(&jobs.join("2-b.json"), &success)
            .await
            .unwrap();
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        reconcile_removals(ctx, &state).await.unwrap();
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert!(state["removals"]["one"].is_null());
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn one_invalid_removal_entry_does_not_stop_reconciling_the_others() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let row = crate::lifecycle_ops::resolve_key(ctx, "one").await.unwrap();
        let jobs = job_dir(ctx);
        tokio::fs::create_dir_all(&jobs).await.unwrap();
        let target_path = row.target.path.clone();
        let branch = row.target.branch.clone();
        ctx.database
            .call(move |store| {
                assert!(store.begin_removal("one", "2-b", &target_path, &branch)?);
                Ok(())
            })
            .await
            .unwrap();
        let success =
            json!({"id": "2-b", "state": "succeeded", "target": {"key": "one"}, "error": null});
        atomic_write(&jobs.join("2-b.json"), &success)
            .await
            .unwrap();
        let mut state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        // Sorted before "one", so a fail-fast loop would never reach it.
        state["removals"]["aaa-corrupt"] = json!({"jobId": "../escape", "error": null});
        reconcile_removals(ctx, &state).await.unwrap();
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert!(state["removals"]["one"].is_null());
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn completion_wait_does_not_treat_startup_as_success() {
        let fixture = CommandFixture::new().await.unwrap();
        let jobs = job_dir(&fixture.ctx);
        tokio::fs::create_dir_all(&jobs).await.unwrap();
        let path = jobs.join("1-a.json");
        atomic_write(&path, &json!({"state": "running"}))
            .await
            .unwrap();
        let waiter = wait_for_completion(&fixture.ctx, "1-a");
        tokio::pin!(waiter);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiter)
                .await
                .is_err()
        );
        atomic_write(
            &path,
            &json!({"state": "failed", "error": "checkout changed after confirmation"}),
        )
        .await
        .unwrap();
        let error = waiter.await.unwrap_err();
        assert_eq!(error.to_string(), "checkout changed after confirmation");
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn reaping_fails_a_running_job_whose_worker_exited() {
        let fixture = CommandFixture::new().await.unwrap();
        let jobs = job_dir(&fixture.ctx);
        tokio::fs::create_dir_all(&jobs).await.unwrap();
        let mut exited = std::process::Command::new("true").spawn().unwrap();
        let pid = exited.id();
        exited.wait().unwrap();
        let path = jobs.join("1-a.json");
        let live = jobs.join("2-b.json");
        let running = |pid: u32| {
            json!({
                "id": "1-a",
                "state": "running",
                "workerPid": pid,
                "workerStartIdentity": "ps:an earlier start",
                "wasArchived": false,
                "target": {"key": "one"},
            })
        };
        atomic_write(&path, &running(pid)).await.unwrap();
        // A live worker whose identity probe works is never reaped.
        let own = process_start_identity(std::process::id(), &fixture.ctx.processes).await;
        let mut alive = running(std::process::id());
        alive["id"] = json!("2-b");
        alive["target"]["key"] = json!("two");
        alive["workerStartIdentity"] = json!(own);
        atomic_write(&live, &alive).await.unwrap();
        fixture
            .ctx
            .database
            .call(|store| {
                store.set_archived("one", true)?;
                store.begin_removal("one", "1-a", "/one", "feature/one")?;
                store.begin_removal("two", "2-b", "/two", "feature/two")?;
                Ok(())
            })
            .await
            .unwrap();

        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        reconcile_removals(&fixture.ctx, &state).await.unwrap();

        let job = read_job(&path).await.unwrap();
        assert_eq!(job["state"], "failed");
        assert_eq!(read_job(&live).await.unwrap()["state"], "running");
        let archived = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_archived_keys()?))
            .await
            .unwrap();
        assert!(archived.contains("one"));
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert!(
            state["removals"]["one"]["error"]
                .as_str()
                .unwrap()
                .contains("disappeared")
        );
        assert!(state["removals"]["two"]["error"].is_null());
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn unreadable_accepted_job_is_uncertain_not_a_failed_removal() {
        let fixture = CommandFixture::new().await.unwrap();
        let error = wait_for_completion(&fixture.ctx, "1-a").await.unwrap_err();
        assert!(error.downcast_ref::<RemovalUncertain>().is_some());
        assert!(error.to_string().contains("before retrying"));
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn completion_wait_reports_durable_success_even_when_ui_is_cancelled() {
        let fixture = CommandFixture::new().await.unwrap();
        let jobs = job_dir(&fixture.ctx);
        tokio::fs::create_dir_all(&jobs).await.unwrap();
        let path = jobs.join("1-a.json");
        atomic_write(&path, &json!({"state": "running"}))
            .await
            .unwrap();
        fixture.ctx.cancellation.cancel();
        let waiter = wait_for_completion(&fixture.ctx, "1-a");
        tokio::pin!(waiter);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiter)
                .await
                .is_err()
        );
        atomic_write(&path, &json!({"state": "succeeded"}))
            .await
            .unwrap();
        waiter.await.unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_detaches_only_after_proving_the_durable_worker_is_live() {
        let fixture = CommandFixture::new().await.unwrap();
        let jobs = job_dir(&fixture.ctx);
        tokio::fs::create_dir_all(&jobs).await.unwrap();
        let path = jobs.join("1-a.json");
        let pid = std::process::id();
        let identity = process_start_identity(pid, &fixture.ctx.processes).await;
        assert!(
            !identity.is_empty(),
            "test process identity must be available"
        );
        atomic_write(
            &path,
            &json!({
                "state": "running",
                "workerPid": pid,
                "workerStartIdentity": identity,
            }),
        )
        .await
        .unwrap();
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let outcome = wait_for_completion_or_shutdown(&fixture.ctx, "1-a", Some(&shutdown))
            .await
            .unwrap();
        assert_eq!(outcome, RemovalWait::ContinuesInBackground);

        atomic_write(&path, &json!({"state": "running"}))
            .await
            .unwrap();
        let error = wait_for_completion_or_shutdown(&fixture.ctx, "1-a", Some(&shutdown))
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<RemovalUncertain>().is_some());
        fixture.close().await.unwrap();
    }
}
