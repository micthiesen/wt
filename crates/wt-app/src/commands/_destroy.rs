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

use crate::{commands::resolve::run_git, context::AppContext};

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
    reap_abandoned(&jobs, &ctx.processes).await?;
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
        reap_abandoned(&jobs, &ctx.processes).await
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
            return Ok(job_id);
        }
        if current["state"] == "succeeded" {
            return Ok(job_id);
        }
        if current["state"] == "failed" {
            bail!(
                "background worker exited during startup; see {}",
                log_path.display()
            );
        }
        if child.try_wait()?.is_some() {
            bail!(
                "background worker exited before startup acknowledgement; see {}",
                log_path.display()
            );
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill();
            bail!(
                "background worker did not acknowledge startup; see {}",
                log_path.display()
            );
        }
        sleep(Duration::from_millis(30)).await;
    }
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
    job["state"] = json!("running");
    atomic_write(&job_path, &job).await?;

    let result = execute_job(ctx, &job).await;
    match result {
        Ok(()) => {
            job["state"] = json!("succeeded");
            job["error"] = Value::Null;
            job["completedAt"] = json!(now_string());
            atomic_write(&job_path, &job).await?;
            Ok(0)
        }
        Err(error) => {
            job["state"] = json!("failed");
            job["error"] = json!(format!("{error:#}"));
            job["completedAt"] = json!(now_string());
            atomic_write(&job_path, &job).await?;
            Err(error)
        }
    }
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
    for warning in removed.warnings {
        eprintln!("warning: {warning}");
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
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let encoded = serde_json::to_vec_pretty(value)?;
    tokio::fs::write(&tmp, encoded)
        .await
        .with_context(|| format!("write destroy job {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("publish destroy job {}", path.display()))?;
    Ok(())
}

async fn reap_abandoned(
    directory: &Path,
    runner: &wt_platform::process::ProcessRunner,
) -> Result<()> {
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
        let actual = process_start_identity(pid, runner).await;
        if expected.is_empty() || actual.is_empty() || actual == expected {
            continue;
        }
        job["state"] = json!("failed");
        job["error"] = json!("worker process disappeared before recording completion");
        job["completedAt"] = json!(now_string());
        atomic_write(&entry.path(), &job).await?;
    }
    Ok(())
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
    let mut spec = CommandSpec::new("ps").args(["-o", "lstart=", "-p", &pid.to_string()]);
    spec.timeout = Duration::from_secs(2);
    runner
        .run(spec, &CancellationToken::new())
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| format!("ps:{}", output.stdout_text().trim()))
        .unwrap_or_default()
}
