use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use wt_config::{BackendKind, Config};
use wt_core::{WorktreeTarget, local_worktree_target};
use wt_platform::{
    lock::{FileLock, LockError},
    process::{CommandSpec, ProcessError, ProcessRunner},
};
use wt_store::{RemovedWorktree, RepositoryIdentity, Store, StoreError};
use wt_vcs::{GitRepository, WorktreeRecord};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
const PROCESS_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct StoreLocation {
    pub path: PathBuf,
    pub identity: RepositoryIdentity,
}

#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub main_clone: PathBuf,
    pub worktree_root: PathBuf,
    pub lock_dir: PathBuf,
    pub state: StoreLocation,
    pub branch_prefix: String,
    pub base_branch: String,
    pub keep_fresh: Vec<String>,
    pub auto_regen_paths: Vec<String>,
    pub branch_id_pattern: String,
    pub slug_max_len: usize,
    pub stage_prefix: String,
    pub default_personal_stage: String,
    pub backend: BackendKind,
    pub copy_files: Vec<String>,
    pub copy_globs: Vec<String>,
    pub install_command: Option<String>,
    pub destroy_command: Option<String>,
    pub has_sst: bool,
    pub reserved_slugs: BTreeSet<String>,
    pub rift_binary: OsString,
    pub shell: OsString,
}

impl ServiceConfig {
    pub fn from_config(config: &Config) -> Self {
        Self {
            main_clone: config.paths.main_clone.clone(),
            worktree_root: config.paths.worktree_root.clone(),
            lock_dir: config.paths.lock_dir.clone(),
            state: StoreLocation {
                path: config.paths.state_db.clone(),
                identity: RepositoryIdentity::new(
                    config.repo_id.clone(),
                    config.repo_path.to_string_lossy(),
                ),
            },
            branch_prefix: config.branch.prefix.clone(),
            base_branch: config.branch.base.clone(),
            keep_fresh: config.branch.keep_fresh.clone(),
            auto_regen_paths: config
                .sst
                .as_ref()
                .map(|sst| sst.auto_regen_paths.clone())
                .unwrap_or_default(),
            branch_id_pattern: config.branch.id_pattern.clone(),
            slug_max_len: config.branch.slug_max_len.max(1.0) as usize,
            stage_prefix: config.stage.prefix.clone(),
            default_personal_stage: config.stage.default_personal.clone(),
            backend: config.backend.kind,
            copy_files: config.lifecycle.env_files_to_copy.clone(),
            copy_globs: config.lifecycle.copy_globs.clone(),
            install_command: config.lifecycle.install_command.clone(),
            destroy_command: config.lifecycle.destroy_command.clone(),
            has_sst: config.sst.is_some(),
            reserved_slugs: ["wt", "main", "dotfiles", "manager"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            rift_binary: "rift".into(),
            shell: std::env::var_os("SHELL")
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "bash".into()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CreateOptions {
    pub base: Option<String>,
    /// Fetch origin before resolving a default base. False for isolated fixtures
    /// and callers that already refreshed repository refs.
    pub fetch_origin: bool,
    pub run_install: bool,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            base: None,
            fetch_origin: true,
            run_install: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateResult {
    pub target: WorktreeTarget,
    pub is_main: bool,
}

#[derive(Clone, Debug, Default)]
pub struct RemoveOptions {
    pub force: bool,
    pub delete_branch: bool,
    pub landed: bool,
    pub destroy_stage: bool,
}

/// Bounded snapshot of the exact checkout that was shown in a destructive
/// confirmation. It is checked again under the per-slug remove lock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovalRevision {
    pub key: String,
    pub path: String,
    pub branch: String,
    pub head: String,
    pub digest: String,
    pub hazards: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoveResult {
    pub removed: bool,
    pub deleted_branch: bool,
    pub destroyed_stage: bool,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CleanupCandidate {
    pub target: WorktreeTarget,
    /// Set only after an authoritative PR/merge-state read confirms the branch
    /// landed. The cleanup operation still rechecks dirt and obligations.
    pub landed: bool,
    pub destroy_stage: bool,
}

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("invalid lifecycle request: {0}")]
    Invalid(String),
    #[error("refusing lifecycle operation: {0}")]
    Refused(String),
    #[error("{operation} at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{operation} in {path}: {source}")]
    Process {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: ProcessError,
    },
    #[error("Git repository inventory: {0}")]
    Repository(#[from] wt_vcs::RepositoryError),
    #[error("durable wt state: {0}")]
    Store(#[from] StoreError),
    #[error("operation lock: {0}")]
    Lock(#[from] LockError),
    #[error("stop development server before removal: {0}")]
    Dev(#[from] wt_dev::DevServerError),
    #[error("background blocking task failed: {0}")]
    Join(String),
    #[error("lifecycle operation cancelled")]
    Cancelled,
    #[error("creation failed: {primary}; rollback also failed: {rollback}")]
    Rollback { primary: String, rollback: String },
}

#[derive(Clone)]
pub struct LifecycleService {
    config: ServiceConfig,
    repository: GitRepository,
    runner: ProcessRunner,
    id_pattern: Option<Regex>,
    dev: Option<wt_dev::DevServerService>,
}

#[derive(Default)]
struct CreateProgress {
    checkout_created: bool,
    checkout_attempted: bool,
    branch_created: bool,
}

impl LifecycleService {
    pub fn new(config: ServiceConfig, repository: GitRepository, runner: ProcessRunner) -> Self {
        let id_pattern = Regex::new(&config.branch_id_pattern).ok();
        Self {
            config,
            repository,
            runner,
            id_pattern,
            dev: None,
        }
    }

    pub fn with_dev_server(mut self, dev: wt_dev::DevServerService) -> Self {
        self.dev = Some(dev);
        self
    }

    pub async fn create(
        &self,
        branch: &str,
        options: CreateOptions,
        cancellation: &CancellationToken,
    ) -> Result<CreateResult, LifecycleError> {
        let branch = branch.trim();
        if branch.is_empty() {
            return Err(LifecycleError::Invalid("branch must not be empty".into()));
        }
        let slug = self.dir_slug(branch);
        let _lock = self.acquire_lock(&slug, "create", cancellation).await?;
        if self.config.reserved_slugs.contains(&slug) {
            return Err(LifecycleError::Refused(format!(
                "{slug:?} is a reserved worktree/session slug"
            )));
        }
        self.validate_branch(branch, cancellation).await?;
        fs::create_dir_all(&self.config.worktree_root)
            .await
            .map_err(|source| LifecycleError::Io {
                operation: "create worktree root",
                path: self.config.worktree_root.clone(),
                source,
            })?;
        let main = canonicalize(&self.config.main_clone, "resolve main clone").await?;
        let root = canonicalize(&self.config.worktree_root, "resolve worktree root").await?;
        let path = root.join(&slug);
        self.ensure_managed_path(&path, &root)?;
        if fs::try_exists(&path)
            .await
            .map_err(|source| LifecycleError::Io {
                operation: "check target path",
                path: path.clone(),
                source,
            })?
        {
            return Err(LifecycleError::Refused(format!(
                "path already exists: {}",
                path.display()
            )));
        }
        if options.fetch_origin {
            let report = self
                .repository
                .fetch_origin(
                    wt_vcs::FetchOriginOptions {
                        keep_fresh: self.config.keep_fresh.clone(),
                        auto_regen_paths: self.config.auto_regen_paths.clone(),
                        sync_install: Some(wt_platform::install::InstallPolicy {
                            command: self.config.install_command.clone(),
                            shell: self.config.shell.clone(),
                        }),
                    },
                    cancellation,
                )
                .await?;
            for warning in report.warnings {
                tracing::warn!(%warning, "Git ref maintenance before creation");
            }
        }
        let local_exists = self
            .ref_exists(&main, &format!("refs/heads/{branch}"), cancellation)
            .await?;
        let remote_exists = if local_exists {
            false
        } else {
            self.ref_exists(
                &main,
                &format!("refs/remotes/origin/{branch}"),
                cancellation,
            )
            .await?
        };
        let existing = local_exists || remote_exists;
        let base_ref = if existing {
            None
        } else {
            let base = options.base.clone().unwrap_or_else(|| {
                if self.config.base_branch.starts_with("origin/") {
                    self.config.base_branch.clone()
                } else {
                    format!("origin/{}", self.config.base_branch)
                }
            });
            Some(base)
        };
        let base_source = match base_ref.as_deref() {
            Some(base) if !base.starts_with("origin/") => {
                let branch_name = base.strip_prefix("refs/heads/").unwrap_or(base);
                let parent = root.join(self.dir_slug(branch_name));
                fs::try_exists(&parent)
                    .await
                    .ok()
                    .filter(|exists| *exists)
                    .map(|_| parent)
            }
            _ => None,
        };
        if let Some(base) = &base_ref {
            let in_main = self.ref_exists(&main, base, cancellation).await?;
            let in_parent = if let Some(source) = &base_source {
                self.ref_exists(source, base, cancellation).await?
            } else {
                false
            };
            if !in_main && !in_parent {
                return Err(LifecycleError::Refused(format!(
                    "base ref {base:?} does not resolve in the main clone or configured parent checkout"
                )));
            }
        }
        let stage = self.compute_stage(&slug);
        let mut progress = CreateProgress::default();
        let created = self
            .create_inner(
                branch,
                &slug,
                &path,
                &main,
                base_ref.as_deref(),
                base_source.as_deref(),
                existing,
                &stage,
                &options,
                &mut progress,
                cancellation,
            )
            .await;
        match created {
            Ok(target) => Ok(CreateResult {
                target,
                is_main: false,
            }),
            Err(primary) => {
                let rollback = self
                    .rollback_create(&path, &main, branch, progress, &CancellationToken::new())
                    .await;
                match rollback {
                    Ok(()) => Err(primary),
                    Err(rollback) => Err(LifecycleError::Rollback {
                        primary: primary.to_string(),
                        rollback: rollback.to_string(),
                    }),
                }
            }
        }
    }

    // This is the creation transaction context; splitting the immutable paths
    // from the options would obscure which values are held through rollback.
    #[allow(clippy::too_many_arguments)]
    async fn create_inner(
        &self,
        branch: &str,
        slug: &str,
        path: &Path,
        main: &Path,
        base_ref: Option<&str>,
        base_source: Option<&Path>,
        existing: bool,
        stage: &str,
        options: &CreateOptions,
        progress: &mut CreateProgress,
        cancellation: &CancellationToken,
    ) -> Result<WorktreeTarget, LifecycleError> {
        self.reset_slug_state(slug, cancellation).await?;
        self.create_backend(
            branch,
            slug,
            path,
            main,
            base_ref,
            base_source,
            progress,
            cancellation,
        )
        .await?;
        let head = self
            .checked_text(
                path,
                ["rev-parse", "--verify", "HEAD"],
                cancellation,
                "record fork point",
            )
            .await?;
        if let Some(base) = base_ref {
            let base_branch = base
                .strip_prefix("origin/")
                .unwrap_or(base)
                .strip_prefix("refs/heads/")
                .unwrap_or_else(|| base.strip_prefix("origin/").unwrap_or(base));
            self.mutate_store(cancellation, {
                let slug = slug.to_owned();
                let base_branch = base_branch.to_owned();
                let head = head.clone();
                move |store| {
                    store
                        .set_slug_base(&slug, Some((&base_branch, Some(head.trim()))))
                        .map(|_| ())
                }
            })
            .await?;
        }
        self.configure_branch(branch, path, base_ref, existing, cancellation)
            .await?;
        self.copy_files(main, path, cancellation).await?;
        if self.config.has_sst {
            let stage_file = path.join(".sst/stage");
            if let Some(parent) = stage_file.parent() {
                fs::create_dir_all(parent)
                    .await
                    .map_err(|source| LifecycleError::Io {
                        operation: "create stage directory",
                        path: parent.to_path_buf(),
                        source,
                    })?;
            }
            fs::write(&stage_file, format!("{stage}\n"))
                .await
                .map_err(|source| LifecycleError::Io {
                    operation: "write stage pin",
                    path: stage_file,
                    source,
                })?;
        }
        if self.backend_for(path).await? == BackendKind::GitWorktree && options.run_install {
            self.install(path, cancellation).await?;
        }
        let created_at = now_iso();
        self.mutate_store(cancellation, {
            let slug = slug.to_owned();
            move |store| store.record_slug_created(&slug, &created_at).map(|_| ())
        })
        .await?;
        Ok(local_worktree_target(
            slug,
            branch,
            path.to_string_lossy(),
            stage,
        ))
    }

    pub async fn remove(
        &self,
        target: &WorktreeTarget,
        options: RemoveOptions,
        cancellation: &CancellationToken,
    ) -> Result<RemoveResult, LifecycleError> {
        self.remove_inner(target, options, false, None, cancellation)
            .await
    }

    pub async fn removal_revision(
        &self,
        target: &WorktreeTarget,
        landed: bool,
        cancellation: &CancellationToken,
    ) -> Result<RemovalRevision, LifecycleError> {
        let row = self.find_live_target(target, cancellation).await?;
        for _ in 0..2 {
            let before = self.capture_revision_for_row(&row, cancellation).await?;
            let hazards = self
                .collect_removal_hazards(&row, landed, cancellation)
                .await?;
            let mut after = self.capture_revision_for_row(&row, cancellation).await?;
            if before == after {
                after.hazards = hazards;
                return Ok(after);
            }
        }
        Err(LifecycleError::Refused(
            "checkout kept changing while preparing its removal warning".into(),
        ))
    }

    pub async fn remove_with_revision(
        &self,
        target: &WorktreeTarget,
        options: RemoveOptions,
        expected: &RemovalRevision,
        cancellation: &CancellationToken,
    ) -> Result<RemoveResult, LifecycleError> {
        self.remove_inner(target, options, false, Some(expected), cancellation)
            .await
    }

    /// Remove only candidates already established as landed by the caller.
    /// This path never forces, and rechecks dirty state and verification debt.
    pub async fn cleanup(
        &self,
        candidates: &[CleanupCandidate],
        cancellation: &CancellationToken,
    ) -> Vec<Result<RemoveResult, LifecycleError>> {
        let mut results = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            if !candidate.landed {
                results.push(Err(LifecycleError::Refused(format!(
                    "{} is not confirmed landed",
                    candidate.target.slug()
                ))));
                continue;
            }
            results.push(
                self.remove_inner(
                    &candidate.target,
                    RemoveOptions {
                        force: false,
                        delete_branch: true,
                        landed: true,
                        destroy_stage: candidate.destroy_stage,
                    },
                    true,
                    None,
                    cancellation,
                )
                .await,
            );
        }
        results
    }

    async fn remove_inner(
        &self,
        target: &WorktreeTarget,
        options: RemoveOptions,
        cleanup: bool,
        expected_revision: Option<&RemovalRevision>,
        cancellation: &CancellationToken,
    ) -> Result<RemoveResult, LifecycleError> {
        let slug = target.slug();
        let lock = self.acquire_lock(slug, "remove", cancellation).await?;
        let root = canonicalize(&self.config.worktree_root, "resolve worktree root").await?;
        let main = canonicalize(&self.config.main_clone, "resolve main clone").await?;
        let requested = PathBuf::from(&target.path);
        let path = canonicalize(&requested, "resolve target checkout").await?;
        if path == main {
            return Err(LifecycleError::Refused(
                "cannot remove the configured main clone".into(),
            ));
        }
        self.ensure_managed_path(&path, &root)?;
        let row = self.find_live_target(target, cancellation).await?;
        if row.is_main {
            return Err(LifecycleError::Refused(
                "cannot remove the configured main clone".into(),
            ));
        }
        if let Some(expected) = expected_revision {
            self.verify_removal_revision(&row, options.landed || cleanup, expected, cancellation)
                .await?;
        }
        if !options.force {
            self.guard_removal(&row, options.landed || cleanup, cancellation)
                .await?;
        }
        if let Some(dev) = &self.dev {
            // Keep the slug lock through supervision cleanup and checkout
            // removal. A new server cannot start in the gap, and a failed
            // external teardown leaves its checkout available for recovery.
            dev.stop_under_lifecycle_lock(&wt_dev::DevWorktree::from(&row), &lock, cancellation)
                .await?;
        }
        let mut warnings = Vec::new();
        let mut force = options.force;
        let mut destroyed_stage = false;
        if options.destroy_stage {
            if !self.config.has_sst {
                warnings.push("skipping sst remove: [deploy.sst] is not configured".into());
            } else {
                match safe_stage(
                    &path,
                    &self.config.stage_prefix,
                    &self.config.default_personal_stage,
                )
                .await
                {
                    Ok(stage) => {
                        let mut spec =
                            CommandSpec::new("pnpm").args(["sst", "remove", "--stage", &stage]);
                        spec.cwd = Some(path.clone());
                        spec.timeout = Duration::from_secs(20 * 60);
                        spec.output_limit = PROCESS_OUTPUT_LIMIT;
                        match self.runner.run(spec, cancellation).await {
                            Ok(output) if output.status.success() => {
                                destroyed_stage = true;
                                force = true;
                            }
                            Ok(output) => warnings.push(format!(
                                "sst remove failed (exit {:?})",
                                output.status.code()
                            )),
                            Err(error) => warnings.push(format!("sst remove failed: {error}")),
                        }
                    }
                    Err(reason) => warnings.push(format!("refusing sst remove: {reason}")),
                }
            }
        }
        if let Err(error) = self.run_destroy_hook(&path, slug, cancellation).await {
            if cancellation.is_cancelled() {
                return Err(LifecycleError::Cancelled);
            }
            warnings.push(error.to_string());
        }
        if let Some(expected) = expected_revision {
            self.verify_removal_revision(&row, options.landed || cleanup, expected, cancellation)
                .await?;
        }
        let backend = self.backend_for(&path).await?;
        match backend {
            BackendKind::GitWorktree => {
                self.remove_git_worktree(&path, &main, force, cancellation)
                    .await?
            }
            BackendKind::Rift => {
                if let Some(warning) = self.remove_rift(&path, &main, force, cancellation).await? {
                    warnings.push(warning);
                }
            }
        }
        // The checkout removal is the irreversible commit point. Finish branch
        // cleanup and durable bookkeeping even if cancellation arrives now.
        let durable_write = CancellationToken::new();
        let mut deleted_branch = false;
        if options.delete_branch
            && backend == BackendKind::GitWorktree
            && !row.target.branch.is_empty()
        {
            match self
                .delete_branch(&main, &row.target.branch, &durable_write)
                .await
            {
                Ok(deleted) => deleted_branch = deleted,
                Err(error) => warnings.push(error.to_string()),
            }
        } else if backend == BackendKind::Rift && !row.target.branch.is_empty() {
            deleted_branch = true;
        }
        if deleted_branch
            && !row.target.branch.is_empty()
            && let Err(error) = self
                .mutate_store(&durable_write, {
                    let branch = row.target.branch.clone();
                    let trunk = self.config.base_branch.clone();
                    let slug = slug.to_owned();
                    move |store| {
                        store
                            .reparent_base_references(&branch, &trunk, Some(&slug))
                            .map(|_| ())
                    }
                })
                .await
        {
            warnings.push(format!("reparent stack references: {error}"));
        }
        let removed = RemovedWorktree {
            slug: slug.to_owned(),
            branch: row.target.branch,
            removed_at: now_iso(),
            work: None,
            automations_paused: None,
            extra: Default::default(),
        };
        if let Err(error) = self
            .mutate_store(&durable_write, move |store| {
                store
                    .record_removed_worktrees(&[removed], now_ms())
                    .map(|_| ())
            })
            .await
        {
            warnings.push(format!("record removed worktree: {error}"));
        }
        Ok(RemoveResult {
            removed: true,
            deleted_branch,
            destroyed_stage,
            warnings,
        })
    }

    pub async fn archive(
        &self,
        key: &str,
        archived: bool,
        cancellation: &CancellationToken,
    ) -> Result<bool, LifecycleError> {
        if key.trim().is_empty() {
            return Err(LifecycleError::Invalid(
                "archive key must not be empty".into(),
            ));
        }
        self.mutate_store(cancellation, {
            let key = key.to_owned();
            move |store| store.set_archived(&key, archived)
        })
        .await
    }

    pub async fn restore(
        &self,
        key: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, LifecycleError> {
        self.archive(key, false, cancellation).await
    }

    async fn acquire_lock(
        &self,
        slug: &str,
        operation: &'static str,
        cancellation: &CancellationToken,
    ) -> Result<FileLock, LifecycleError> {
        validate_slug(slug)?;
        FileLock::acquire(&self.config.lock_dir, slug, operation, cancellation)
            .await
            .map_err(|error| match error {
                LockError::Cancelled => LifecycleError::Cancelled,
                error => LifecycleError::Lock(error),
            })
    }

    async fn mutate_store<R: Send + 'static>(
        &self,
        cancellation: &CancellationToken,
        mutate: impl FnOnce(&mut Store) -> Result<R, StoreError> + Send + 'static,
    ) -> Result<R, LifecycleError> {
        if cancellation.is_cancelled() {
            return Err(LifecycleError::Cancelled);
        }
        let path = self.config.state.path.clone();
        let identity = self.config.state.identity.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = Store::open(path, identity)?;
            mutate(&mut store)
        })
        .await
        .map_err(|error| LifecycleError::Join(error.to_string()))?
        .map_err(Into::into)
    }

    async fn validate_branch(
        &self,
        branch: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        self.run_git(
            &self.config.main_clone,
            ["check-ref-format", "--branch", branch],
            cancellation,
            "validate branch name",
        )
        .await?;
        Ok(())
    }

    async fn ref_exists(
        &self,
        cwd: &Path,
        reference: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, LifecycleError> {
        let commit_ref = format!("{reference}^{{commit}}");
        let output = self
            .run_git_raw(
                cwd,
                [
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &commit_ref,
                ],
                cancellation,
                "check Git ref",
            )
            .await?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) | Some(128) => Ok(false),
            _ => Err(process_error(
                "check Git ref",
                cwd,
                output.checked("git").unwrap_err(),
            )),
        }
    }

    async fn checked_text<const N: usize>(
        &self,
        cwd: &Path,
        args: [&str; N],
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<String, LifecycleError> {
        let output = self.run_git(cwd, args, cancellation, operation).await?;
        Ok(String::from_utf8_lossy(&output).trim().to_owned())
    }

    async fn run_git<const N: usize>(
        &self,
        cwd: &Path,
        args: [&str; N],
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<Vec<u8>, LifecycleError> {
        let output = self.run_git_raw(cwd, args, cancellation, operation).await?;
        output
            .checked("git")
            .map(|output| output.stdout)
            .map_err(|source| process_error(operation, cwd, source))
    }

    async fn run_git_vec(
        &self,
        cwd: &Path,
        args: Vec<OsString>,
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<Vec<u8>, LifecycleError> {
        let output = self
            .run_git_vec_raw(cwd, args, cancellation, operation)
            .await?;
        output
            .checked("git")
            .map(|output| output.stdout)
            .map_err(|source| process_error(operation, cwd, source))
    }

    async fn run_git_raw<const N: usize>(
        &self,
        cwd: &Path,
        args: [&str; N],
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<wt_platform::process::ProcessOutput, LifecycleError> {
        self.run_git_vec_raw(
            cwd,
            args.into_iter().map(OsString::from).collect(),
            cancellation,
            operation,
        )
        .await
    }

    async fn run_git_vec_raw(
        &self,
        cwd: &Path,
        args: Vec<OsString>,
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<wt_platform::process::ProcessOutput, LifecycleError> {
        let mut spec = CommandSpec::new("git");
        spec.args = args;
        spec.cwd = Some(cwd.to_path_buf());
        spec.env = vec![
            ("GIT_OPTIONAL_LOCKS".into(), Some("0".into())),
            ("GIT_TERMINAL_PROMPT".into(), Some("0".into())),
            ("LC_ALL".into(), Some("C".into())),
        ];
        spec.timeout = PROCESS_TIMEOUT;
        spec.output_limit = PROCESS_OUTPUT_LIMIT;
        self.runner
            .run(spec, cancellation)
            .await
            .map_err(|source| process_error(operation, cwd, source))
    }

    fn dir_slug(&self, branch: &str) -> String {
        let mut slug =
            if let Some(rest) = branch.strip_prefix(&format!("{}/", self.config.branch_prefix)) {
                rest.replace('/', "-")
            } else if let Some((_, rest)) = branch.split_once('/') {
                if self
                    .id_pattern
                    .as_ref()
                    .is_some_and(|pattern| pattern.is_match(rest))
                {
                    rest.replace('/', "-")
                } else {
                    branch.replace('/', "-")
                }
            } else {
                branch.to_owned()
            };
        slug = truncate_slug(slug, self.config.slug_max_len.max(1));
        slug
    }

    fn compute_stage(&self, slug: &str) -> String {
        let normalized = slug.to_lowercase();
        let digest = Sha256::digest(normalized.as_bytes());
        let hex = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let issue = self
            .id_pattern
            .as_ref()
            .and_then(|pattern| pattern.captures(&normalized))
            .and_then(|captures| captures.get(1))
            .map(|value| value.as_str());
        match issue {
            Some(issue) => format!("{}{issue}-{}", self.config.stage_prefix, &hex[..6]),
            None => format!("{}{}", self.config.stage_prefix, &hex[..10]),
        }
    }

    fn ensure_managed_path(&self, path: &Path, root: &Path) -> Result<(), LifecycleError> {
        if path == root || !path.starts_with(root) {
            return Err(LifecycleError::Refused(format!(
                "checkout path {} is outside configured worktree root {}",
                path.display(),
                root.display()
            )));
        }
        let relative = path.strip_prefix(root).expect("starts_with checked");
        if relative.components().count() != 1 {
            return Err(LifecycleError::Refused(
                "worktrees must be direct children of the configured root".into(),
            ));
        }
        Ok(())
    }

    async fn reset_slug_state(
        &self,
        slug: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        self.mutate_store(cancellation, {
            let slug = slug.to_owned();
            move |store| {
                store.clear_slug_state(&slug)?;
                store.clear_removed_worktree(&slug)?;
                store.set_archived(&slug, false)?;
                Ok(())
            }
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_backend(
        &self,
        branch: &str,
        slug: &str,
        path: &Path,
        main: &Path,
        base_ref: Option<&str>,
        base_source: Option<&Path>,
        progress: &mut CreateProgress,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        let backend = self.config.backend;
        match backend {
            BackendKind::GitWorktree => {
                progress.checkout_attempted = true;
                let mut args: Vec<OsString> = vec!["worktree".into(), "add".into()];
                if let Some(base) = base_ref {
                    args.extend([
                        "--no-track".into(),
                        "-b".into(),
                        branch.into(),
                        path.to_string_lossy().into_owned().into(),
                        base.into(),
                    ]);
                    progress.branch_created = true;
                } else {
                    args.extend([path.to_string_lossy().into_owned().into(), branch.into()]);
                }
                self.run_git_vec(main, args, cancellation, "create Git worktree")
                    .await?;
                progress.checkout_created = true;
            }
            BackendKind::Rift => {
                progress.checkout_attempted = true;
                if !fs::try_exists(main.join(".rift")).await.map_err(|source| {
                    LifecycleError::Io {
                        operation: "check Rift registration",
                        path: main.join(".rift"),
                        source,
                    }
                })? && let Err(error) = self
                    .run_external(
                        &self.config.rift_binary,
                        main,
                        ["init", "--here"],
                        cancellation,
                        "initialize Rift repository",
                    )
                    .await
                    && !fs::try_exists(main.join(".rift")).await.unwrap_or(false)
                {
                    return Err(error);
                }
                let mut output = self.run_rift_create(main, path, slug, cancellation).await?;
                if !output.status.success()
                    && is_stale_rift_registry_error(&output.stderr, &output.stdout)
                    && !fs::try_exists(path).await.unwrap_or(false)
                {
                    self.run_rift_gc(main, cancellation).await?;
                    output = self.run_rift_create(main, path, slug, cancellation).await?;
                }
                output
                    .checked("rift")
                    .map_err(|source| process_error("create Rift clone", main, source))?;
                progress.checkout_created = fs::try_exists(path).await.unwrap_or(false);
                if !progress.checkout_created {
                    return Err(LifecycleError::Refused(format!(
                        "rift create did not produce {}",
                        path.display()
                    )));
                }
                let refreshed = self
                    .run_git_raw(
                        path,
                        ["update-index", "--refresh"],
                        cancellation,
                        "refresh copied Git index",
                    )
                    .await?;
                if !matches!(refreshed.status.code(), Some(0 | 1)) {
                    return Err(process_error(
                        "refresh copied Git index",
                        path,
                        refreshed.checked("git").unwrap_err(),
                    ));
                }
                if let Some(source) = base_source {
                    let base = base_ref.expect("stacked base source requires a base ref");
                    let fetch = self
                        .run_git_vec_raw(
                            path,
                            vec![
                                "fetch".into(),
                                "--no-tags".into(),
                                source.as_os_str().to_os_string(),
                                normalize_head_ref(base).into(),
                            ],
                            cancellation,
                            "fetch stacked base into Rift clone",
                        )
                        .await?;
                    if !fetch.status.success()
                        && !self
                            .ref_exists(path, base_ref.unwrap_or_default(), cancellation)
                            .await?
                    {
                        return Err(process_error(
                            "fetch stacked base into Rift clone",
                            path,
                            fetch.checked("git").unwrap_err(),
                        ));
                    }
                }
                match base_ref {
                    None => {
                        self.run_git(
                            path,
                            ["switch", "--discard-changes", branch],
                            cancellation,
                            "switch Rift clone branch",
                        )
                        .await?
                    }
                    Some(_) if base_source.is_some() => {
                        self.run_git(
                            path,
                            ["switch", "--discard-changes", "-c", branch, "FETCH_HEAD"],
                            cancellation,
                            "create branch in Rift clone",
                        )
                        .await?
                    }
                    Some(base) => {
                        self.run_git(
                            path,
                            ["switch", "--discard-changes", "-c", branch, base],
                            cancellation,
                            "create branch in Rift clone",
                        )
                        .await?
                    }
                };
            }
        }
        Ok(())
    }

    async fn configure_branch(
        &self,
        branch: &str,
        path: &Path,
        base_ref: Option<&str>,
        existing: bool,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        if existing {
            let upstream = self
                .run_git_raw(
                    path,
                    ["rev-parse", "--abbrev-ref", "@{u}"],
                    cancellation,
                    "check branch upstream",
                )
                .await?;
            if !upstream.status.success() {
                let remote = format!("refs/remotes/origin/{branch}");
                if self.ref_exists(path, &remote, cancellation).await? {
                    self.run_git(
                        path,
                        [
                            "branch",
                            "--set-upstream-to",
                            &format!("origin/{branch}"),
                            branch,
                        ],
                        cancellation,
                        "set branch upstream",
                    )
                    .await?;
                }
            }
        }
        let configured = self
            .run_git_raw(
                path,
                ["config", "--get", &format!("branch.{branch}.gh-merge-base")],
                cancellation,
                "read GitHub merge base",
            )
            .await?;
        let merge_base = if existing && configured.status.success() {
            None
        } else {
            base_ref
                .map(|base| base.strip_prefix("origin/").unwrap_or(base).to_owned())
                .or_else(|| Some(self.config.base_branch.clone()))
        };
        if let Some(base) = merge_base {
            self.run_git(
                path,
                ["config", &format!("branch.{branch}.gh-merge-base"), &base],
                cancellation,
                "set GitHub merge base",
            )
            .await?;
        }
        Ok(())
    }

    async fn copy_files(
        &self,
        source: &Path,
        destination: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        for relative in &self.config.copy_files {
            let relative = validate_relative_path(relative)?;
            let from = source.join(&relative);
            let to = destination.join(&relative);
            self.copy_missing(&from, &to, cancellation).await?;
        }
        if self.config.copy_globs.is_empty() {
            return Ok(());
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in &self.config.copy_globs {
            builder.add(
                GlobBuilder::new(pattern)
                    .literal_separator(false)
                    .build()
                    .map_err(|error| {
                        LifecycleError::Invalid(format!("invalid copy glob {pattern:?}: {error}"))
                    })?,
            );
        }
        let globs: GlobSet = builder
            .build()
            .map_err(|error| LifecycleError::Invalid(format!("invalid copy glob set: {error}")))?;
        let mut pending = vec![source.to_path_buf()];
        while let Some(directory) = pending.pop() {
            if cancellation.is_cancelled() {
                return Err(LifecycleError::Cancelled);
            }
            let mut entries =
                fs::read_dir(&directory)
                    .await
                    .map_err(|source| LifecycleError::Io {
                        operation: "scan copy glob source",
                        path: directory.clone(),
                        source,
                    })?;
            while let Some(entry) =
                entries
                    .next_entry()
                    .await
                    .map_err(|source| LifecycleError::Io {
                        operation: "read copy glob directory",
                        path: directory.clone(),
                        source,
                    })?
            {
                let name = entry.file_name();
                if name == ".git" {
                    continue;
                }
                let path = entry.path();
                let kind = entry
                    .file_type()
                    .await
                    .map_err(|source| LifecycleError::Io {
                        operation: "inspect copy glob entry",
                        path: path.clone(),
                        source,
                    })?;
                if kind.is_dir() {
                    pending.push(path);
                } else if kind.is_file() {
                    let relative = path
                        .strip_prefix(source)
                        .expect("walk remains under source");
                    if globs.is_match(relative) {
                        self.copy_missing(&path, &destination.join(relative), cancellation)
                            .await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn copy_missing(
        &self,
        source: &Path,
        destination: &Path,
        cancellation: &CancellationToken,
    ) -> Result<bool, LifecycleError> {
        let mut input = match fs::File::open(source).await {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(source_error) => {
                return Err(LifecycleError::Io {
                    operation: "open lifecycle copy source",
                    path: source.to_path_buf(),
                    source: source_error,
                });
            }
        };
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|source| LifecycleError::Io {
                    operation: "create copy destination directory",
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        let mut output = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .await
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(source) => {
                return Err(LifecycleError::Io {
                    operation: "create lifecycle copy destination",
                    path: destination.to_path_buf(),
                    source,
                });
            }
        };
        let copied: Result<(), LifecycleError> = tokio::select! {
            _ = cancellation.cancelled() => Err(LifecycleError::Cancelled),
            result = tokio::io::copy(&mut input, &mut output) => result.map(|_| ()).map_err(|source| LifecycleError::Io {
                operation: "copy lifecycle file",
                path: destination.to_path_buf(),
                source,
            }),
        };
        if let Err(error) = copied {
            drop(output);
            let _ = fs::remove_file(destination).await;
            return Err(error);
        }
        output.flush().await.map_err(|source| LifecycleError::Io {
            operation: "flush lifecycle copy",
            path: destination.to_path_buf(),
            source,
        })?;
        Ok(true)
    }

    async fn install(
        &self,
        path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        let policy = wt_platform::install::InstallPolicy {
            command: self.config.install_command.clone(),
            shell: self.config.shell.clone(),
        };
        let Some(plan) =
            policy
                .resolve(path, false)
                .await
                .map_err(|source| LifecycleError::Io {
                    operation: "detect dependency manager",
                    path: path.to_path_buf(),
                    source,
                })?
        else {
            return Ok(());
        };
        let spec = plan.spec;
        let output = self
            .runner
            .run(spec, cancellation)
            .await
            .map_err(|source| process_error("run package install", path, source))?;
        output
            .checked("package manager")
            .map(|_| ())
            .map_err(|source| process_error("run package install", path, source))
    }

    async fn rollback_create(
        &self,
        path: &Path,
        main: &Path,
        branch: &str,
        progress: CreateProgress,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        let checkout_is_ours = if progress.checkout_created {
            true
        } else if progress.checkout_attempted {
            match self.config.backend {
                BackendKind::GitWorktree => self
                    .repository
                    .inventory(cancellation)
                    .await?
                    .iter()
                    .any(|record| {
                        Path::new(&record.target.path) == path && record.target.branch == branch
                    }),
                BackendKind::Rift => fs::try_exists(path.join(".rift")).await.unwrap_or(false),
            }
        } else {
            false
        };
        if checkout_is_ours {
            match self.backend_for(path).await.unwrap_or(self.config.backend) {
                BackendKind::GitWorktree => {
                    self.remove_git_worktree(path, main, true, cancellation)
                        .await?
                }
                BackendKind::Rift => {
                    let _ = self.remove_rift(path, main, true, cancellation).await?;
                }
            }
        }
        if progress.branch_created
            && self
                .ref_exists(main, &format!("refs/heads/{branch}"), cancellation)
                .await?
        {
            self.run_git(
                main,
                ["branch", "-D", branch],
                cancellation,
                "remove branch after failed creation",
            )
            .await?;
        }
        Ok(())
    }

    async fn backend_for(&self, path: &Path) -> Result<BackendKind, LifecycleError> {
        let marker = path.join(".rift");
        fs::try_exists(&marker)
            .await
            .map(|exists| {
                if exists {
                    BackendKind::Rift
                } else {
                    BackendKind::GitWorktree
                }
            })
            .map_err(|source| LifecycleError::Io {
                operation: "detect worktree backend",
                path: marker,
                source,
            })
    }

    async fn remove_git_worktree(
        &self,
        path: &Path,
        main: &Path,
        force: bool,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        let path_text = path.to_string_lossy().to_string();
        let result = if force {
            self.run_git_raw(
                main,
                ["worktree", "remove", "--force", &path_text],
                cancellation,
                "remove Git worktree",
            )
            .await?
        } else {
            self.run_git_raw(
                main,
                ["worktree", "remove", &path_text],
                cancellation,
                "remove Git worktree",
            )
            .await?
        };
        if result.status.success() {
            return Ok(());
        }
        let prune = self
            .run_git_raw(
                main,
                ["worktree", "prune"],
                cancellation,
                "prune Git worktree metadata",
            )
            .await?;
        if !fs::try_exists(path).await.unwrap_or(false) && prune.status.success() {
            return Ok(());
        }
        Err(process_error(
            "remove Git worktree",
            main,
            result.checked("git").unwrap_err(),
        ))
    }

    async fn remove_rift(
        &self,
        path: &Path,
        main: &Path,
        force: bool,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, LifecycleError> {
        let mut spec = CommandSpec::new(self.config.rift_binary.clone());
        spec.args = if force {
            vec![
                "remove".into(),
                "--force".into(),
                path.as_os_str().to_os_string(),
            ]
        } else {
            vec!["remove".into(), path.as_os_str().to_os_string()]
        };
        spec.cwd = Some(main.to_path_buf());
        spec.timeout = PROCESS_TIMEOUT;
        let output = self
            .runner
            .run(spec, cancellation)
            .await
            .map_err(|source| process_error("remove Rift worktree", main, source))?;
        if !output.status.success() && fs::try_exists(path).await.unwrap_or(false) {
            return Err(process_error(
                "remove Rift worktree",
                main,
                output.checked("rift").unwrap_err(),
            ));
        }
        Ok(self
            .run_rift_gc(main, cancellation)
            .await
            .err()
            .map(|error| format!("Rift garbage collection failed: {error}")))
    }

    async fn run_rift_create(
        &self,
        main: &Path,
        path: &Path,
        slug: &str,
        cancellation: &CancellationToken,
    ) -> Result<wt_platform::process::ProcessOutput, LifecycleError> {
        let mut spec = CommandSpec::new(self.config.rift_binary.clone());
        spec.args = ["create", "--name", slug, "--into"]
            .into_iter()
            .map(OsString::from)
            .chain([path
                .parent()
                .unwrap_or(&self.config.worktree_root)
                .as_os_str()
                .to_os_string()])
            .chain([OsString::from("--copy-all")])
            .collect();
        spec.cwd = Some(main.to_path_buf());
        spec.timeout = PROCESS_TIMEOUT;
        spec.output_limit = PROCESS_OUTPUT_LIMIT;
        self.runner
            .run(spec, cancellation)
            .await
            .map_err(|source| process_error("create Rift clone", main, source))
    }

    async fn run_rift_gc(
        &self,
        main: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        let mut spec = prioritized_rift_gc(&self.config.rift_binary);
        spec.cwd = Some(main.to_path_buf());
        spec.timeout = Duration::from_secs(20 * 60);
        spec.output_limit = PROCESS_OUTPUT_LIMIT;
        let output = self
            .runner
            .run(spec, cancellation)
            .await
            .map_err(|source| process_error("reclaim Rift clone storage", main, source))?;
        output
            .checked("rift gc")
            .map_err(|source| process_error("reclaim Rift clone storage", main, source))?;
        Ok(())
    }

    async fn delete_branch(
        &self,
        main: &Path,
        branch: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, LifecycleError> {
        if !self
            .ref_exists(main, &format!("refs/heads/{branch}"), cancellation)
            .await?
        {
            return Ok(false);
        }
        self.run_git(
            main,
            ["branch", "-D", branch],
            cancellation,
            "delete worktree branch",
        )
        .await?;
        Ok(true)
    }

    async fn find_live_target(
        &self,
        target: &WorktreeTarget,
        cancellation: &CancellationToken,
    ) -> Result<WorktreeRecord, LifecycleError> {
        let inventory = self.repository.inventory(cancellation).await?;
        let expected = canonicalize(Path::new(&target.path), "resolve requested target").await?;
        inventory
            .into_iter()
            .find(|row| {
                row.target.slug() == target.slug() && Path::new(&row.target.path) == expected
            })
            .ok_or_else(|| {
                LifecycleError::Refused(format!(
                    "{} is not a live local worktree in the configured inventory",
                    target.slug()
                ))
            })
    }

    async fn capture_revision_for_row(
        &self,
        row: &WorktreeRecord,
        cancellation: &CancellationToken,
    ) -> Result<RemovalRevision, LifecycleError> {
        let path = Path::new(&row.target.path);
        let head = self
            .git_snapshot_output(path, ["rev-parse", "--verify", "HEAD"], cancellation)
            .await?
            .stdout_text()
            .trim()
            .to_owned();
        if head.is_empty() {
            return Err(LifecycleError::Refused(
                "checkout HEAD could not be captured".into(),
            ));
        }
        let status = self
            .git_snapshot_output(
                path,
                ["status", "--porcelain=v2", "-z", "--untracked-files=all"],
                cancellation,
            )
            .await?
            .stdout;
        let worktree_diff = self
            .git_snapshot_output(
                path,
                ["diff", "--binary", "--no-ext-diff", "--no-textconv", "--"],
                cancellation,
            )
            .await?
            .stdout;
        let index_diff = self
            .git_snapshot_output(
                path,
                [
                    "diff",
                    "--cached",
                    "--binary",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--",
                ],
                cancellation,
            )
            .await?
            .stdout;
        let untracked = self
            .git_snapshot_output(
                path,
                ["ls-files", "--others", "--exclude-standard", "-z"],
                cancellation,
            )
            .await?
            .stdout;
        let checkout = PathBuf::from(&row.target.path);
        let metadata =
            tokio::task::spawn_blocking(move || untracked_metadata(&checkout, &untracked))
                .await
                .map_err(|error| LifecycleError::Join(error.to_string()))?
                .map_err(|message| {
                    LifecycleError::Refused(format!("cannot safely snapshot checkout: {message}"))
                })?;
        let mut digest = Sha256::new();
        for bytes in [&status, &worktree_diff, &index_diff, &metadata] {
            digest.update((bytes.len() as u64).to_be_bytes());
            digest.update(bytes);
        }
        Ok(RemovalRevision {
            key: wt_core::worktree_target_key(&row.target),
            path: row.target.path.clone(),
            branch: row.target.branch.clone(),
            head,
            digest: format!("{:x}", digest.finalize()),
            hazards: Vec::new(),
        })
    }

    async fn verify_removal_revision(
        &self,
        row: &WorktreeRecord,
        landed: bool,
        expected: &RemovalRevision,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        let before = self.capture_revision_for_row(row, cancellation).await?;
        if !same_checkout_revision(&before, expected) {
            return Err(LifecycleError::Refused(
                "checkout changed after the removal warning; review the new state before removing"
                    .into(),
            ));
        }
        let hazards = self
            .collect_removal_hazards(row, landed, cancellation)
            .await?;
        let after = self.capture_revision_for_row(row, cancellation).await?;
        if hazards != expected.hazards || !same_checkout_revision(&after, expected) {
            return Err(LifecycleError::Refused(
                "checkout or removal hazards changed after the warning; review the new state before removing".into(),
            ));
        }
        Ok(())
    }

    async fn git_snapshot_output<const N: usize>(
        &self,
        path: &Path,
        args: [&str; N],
        cancellation: &CancellationToken,
    ) -> Result<wt_platform::process::ProcessOutput, LifecycleError> {
        let mut command = CommandSpec::new("git").args(args);
        command.cwd = Some(path.to_path_buf());
        command.timeout = Duration::from_secs(30);
        command.output_limit = PROCESS_OUTPUT_LIMIT;
        self.runner
            .run(command, cancellation)
            .await
            .map_err(|source| LifecycleError::Process {
                operation: "capture removal revision",
                path: path.to_path_buf(),
                source,
            })
            .and_then(|output| {
                output
                    .checked("git")
                    .map_err(|source| LifecycleError::Process {
                        operation: "capture removal revision",
                        path: path.to_path_buf(),
                        source,
                    })
            })
    }

    /// Read every removal hazard for a confirmation. Removal checks these again
    /// under its operation lock; a displayed plan never grants later safety.
    pub async fn removal_hazards(
        &self,
        target: &WorktreeTarget,
        landed: bool,
        cancellation: &CancellationToken,
    ) -> Result<Vec<String>, LifecycleError> {
        let row = self.find_live_target(target, cancellation).await?;
        self.collect_removal_hazards(&row, landed, cancellation)
            .await
    }

    async fn guard_removal(
        &self,
        row: &WorktreeRecord,
        landed: bool,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        let hazards = self
            .collect_removal_hazards(row, landed, cancellation)
            .await?;
        if hazards.is_empty() {
            Ok(())
        } else {
            Err(LifecycleError::Refused(format!(
                "{}: {}; explicit force is required",
                row.target.slug(),
                hazards.join("; ")
            )))
        }
    }

    async fn collect_removal_hazards(
        &self,
        row: &WorktreeRecord,
        landed: bool,
        cancellation: &CancellationToken,
    ) -> Result<Vec<String>, LifecycleError> {
        let mut hazards = Vec::new();
        let status = self.repository.status(row, cancellation).await?;
        if status.dirty {
            hazards.push("uncommitted changes".to_owned());
        }
        let slug = row.target.slug().to_owned();
        let state = self.read_slug_state(&slug, cancellation).await?;
        let work = state.as_ref().and_then(|value| value.get("work"));
        if work.is_some_and(|work| {
            work.get("verifyAfterMerge")
                .and_then(Value::as_str)
                .is_some_and(|steps| !steps.trim().is_empty())
                && !matches!(
                    work.get("state").and_then(Value::as_str),
                    Some("verified" | "dropped")
                )
        }) {
            hazards.push("post-merge verification still owed".to_owned());
        }
        if !landed {
            match self
                .unpushed_count(row, state.as_ref(), cancellation)
                .await?
            {
                Some(0) => {}
                Some(count) => {
                    hazards.push(format!("{count} unpushed commit(s)"));
                }
                None => {
                    hazards.push("could not verify pushed state".to_owned());
                }
            }
        }
        Ok(hazards)
    }

    async fn read_slug_state(
        &self,
        slug: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<Value>, LifecycleError> {
        let path = self.config.state.path.clone();
        let identity = self.config.state.identity.clone();
        let slug = slug.to_owned();
        if cancellation.is_cancelled() {
            return Err(LifecycleError::Cancelled);
        }
        tokio::task::spawn_blocking(move || {
            let mut store = Store::open_read_only(path, identity)?;
            store.read_slug_state(&slug)
        })
        .await
        .map_err(|error| LifecycleError::Join(error.to_string()))?
        .map_err(Into::into)
    }

    async fn unpushed_count(
        &self,
        row: &WorktreeRecord,
        state: Option<&Value>,
        cancellation: &CancellationToken,
    ) -> Result<Option<u32>, LifecycleError> {
        let path = Path::new(&row.target.path);
        let origin_branch = format!("refs/remotes/origin/{}", row.target.branch);
        let base = if self.ref_exists(path, &origin_branch, cancellation).await? {
            format!("origin/{}", row.target.branch)
        } else {
            let recorded_sha = state.and_then(|v| v.get("baseSha")).and_then(Value::as_str);
            let recorded_branch = state
                .and_then(|v| v.get("baseBranch"))
                .and_then(Value::as_str);
            if let Some(sha) = recorded_sha {
                sha.to_owned()
            } else if let Some(branch) = recorded_branch {
                format!("origin/{branch}")
            } else if self
                .ref_exists(
                    path,
                    &format!("refs/remotes/origin/{}", self.config.base_branch),
                    cancellation,
                )
                .await?
            {
                format!("origin/{}", self.config.base_branch)
            } else if self
                .ref_exists(
                    path,
                    &format!("refs/heads/{}", self.config.base_branch),
                    cancellation,
                )
                .await?
            {
                self.config.base_branch.clone()
            } else {
                return Ok(None);
            }
        };
        let count = self
            .run_git_raw(
                path,
                ["rev-list", "--count", &format!("{base}..HEAD")],
                cancellation,
                "count unpushed commits",
            )
            .await?;
        if !count.status.success() {
            return Ok(None);
        }
        Ok(String::from_utf8_lossy(&count.stdout)
            .trim()
            .parse::<u32>()
            .ok())
    }

    async fn run_destroy_hook(
        &self,
        path: &Path,
        slug: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        let Some(command) = &self.config.destroy_command else {
            return Ok(());
        };
        let expanded = command
            .replace("{{path}}", &path.to_string_lossy())
            .replace("{{slug}}", slug);
        let mut spec = CommandSpec::new(self.config.shell.clone());
        spec.args = vec!["-lc".into(), expanded.into()];
        spec.cwd = Some(path.to_path_buf());
        spec.timeout = Duration::from_secs(5 * 60);
        let output = self
            .runner
            .run(spec, cancellation)
            .await
            .map_err(|source| process_error("run destroy command", path, source))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(process_error(
                "run destroy command",
                path,
                output.checked("shell").unwrap_err(),
            ))
        }
    }

    async fn run_external<const N: usize>(
        &self,
        program: &OsString,
        cwd: &Path,
        args: [&str; N],
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<(), LifecycleError> {
        let mut spec = CommandSpec::new(program.clone()).args(args);
        spec.cwd = Some(cwd.to_path_buf());
        spec.timeout = PROCESS_TIMEOUT;
        spec.output_limit = PROCESS_OUTPUT_LIMIT;
        let output = self
            .runner
            .run(spec, cancellation)
            .await
            .map_err(|source| process_error(operation, cwd, source))?;
        output
            .checked(program)
            .map(|_| ())
            .map_err(|source| process_error(operation, cwd, source))
    }
}

fn process_error(operation: &'static str, path: &Path, source: ProcessError) -> LifecycleError {
    LifecycleError::Process {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn validate_slug(slug: &str) -> Result<(), LifecycleError> {
    if slug.is_empty()
        || slug == "."
        || slug == ".."
        || slug.contains('/')
        || slug.contains('\\')
        || slug.chars().any(char::is_control)
    {
        return Err(LifecycleError::Invalid(format!(
            "unsafe worktree slug {slug:?}"
        )));
    }
    Ok(())
}

fn truncate_slug(slug: String, limit: usize) -> String {
    if slug.chars().count() <= limit {
        return slug;
    }
    let cut = slug
        .char_indices()
        .take_while(|(index, _)| *index <= limit)
        .filter(|(_, ch)| *ch == '-')
        .map(|(index, _)| index)
        .last();
    let byte_end = cut
        .filter(|index| slug[..*index].chars().count() >= limit / 2)
        .unwrap_or_else(|| {
            slug.char_indices()
                .nth(limit)
                .map_or(slug.len(), |(index, _)| index)
        });
    slug[..byte_end].to_owned()
}

async fn canonicalize(path: &Path, operation: &'static str) -> Result<PathBuf, LifecycleError> {
    fs::canonicalize(path)
        .await
        .map_err(|source| LifecycleError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        })
}

const MAX_UNTRACKED_FILES: usize = 4096;
const MAX_UNTRACKED_BYTES: u64 = 16 * 1024 * 1024;

fn same_checkout_revision(current: &RemovalRevision, expected: &RemovalRevision) -> bool {
    current.key == expected.key
        && current.path == expected.path
        && current.branch == expected.branch
        && current.head == expected.head
        && current.digest == expected.digest
}

fn untracked_metadata(checkout: &Path, paths: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    let mut entries = paths
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    if entries.len() > MAX_UNTRACKED_FILES {
        return Err(format!("more than {MAX_UNTRACKED_FILES} untracked paths"));
    }
    entries.sort();
    let mut total_bytes = 0u64;
    let mut result = Vec::new();
    for relative in entries {
        let relative_path = git_path_from_bytes(&relative)?;
        if relative_path.is_absolute()
            || relative_path.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Err("Git reported an untracked path outside the checkout".into());
        }
        let full_path = checkout.join(relative_path);
        let metadata = std::fs::symlink_metadata(&full_path)
            .map_err(|error| format!("stat {}: {error}", full_path.display()))?;
        total_bytes = total_bytes.saturating_add(metadata.len());
        if total_bytes > MAX_UNTRACKED_BYTES {
            return Err(format!(
                "untracked files exceed {MAX_UNTRACKED_BYTES} bytes"
            ));
        }
        let modified = metadata
            .modified()
            .map_err(|error| format!("read mtime for {}: {error}", full_path.display()))?;
        let modified_ns = modified
            .duration_since(UNIX_EPOCH)
            .map_err(|_| format!("mtime predates Unix epoch: {}", full_path.display()))?
            .as_nanos();
        result.extend_from_slice(&(relative.len() as u64).to_be_bytes());
        result.extend_from_slice(&relative);
        result.extend_from_slice(&metadata.len().to_be_bytes());
        result.extend_from_slice(&modified_ns.to_be_bytes());
        #[cfg(unix)]
        result.extend_from_slice(&metadata.mode().to_be_bytes());
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&full_path)
                .map_err(|error| format!("read symlink {}: {error}", full_path.display()))?;
            let target = os_path_bytes(target.as_os_str());
            result.extend_from_slice(&(target.len() as u64).to_be_bytes());
            result.extend_from_slice(target);
        } else if metadata.is_file() {
            let contents = std::fs::read(&full_path)
                .map_err(|error| format!("read {}: {error}", full_path.display()))?;
            if contents.len() as u64 != metadata.len() {
                return Err(format!(
                    "{} changed while its removal snapshot was captured",
                    full_path.display()
                ));
            }
            let hash = Sha256::digest(&contents);
            result.extend_from_slice(&hash);
        } else {
            return Err(format!(
                "unsupported untracked file type: {}",
                full_path.display()
            ));
        }
    }
    Ok(result)
}

#[cfg(unix)]
fn git_path_from_bytes(path: &[u8]) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path)))
}

#[cfg(not(unix))]
fn git_path_from_bytes(path: &[u8]) -> Result<PathBuf, String> {
    String::from_utf8(path.to_vec())
        .map(PathBuf::from)
        .map_err(|_| "untracked path is not UTF-8".into())
}

#[cfg(unix)]
fn os_path_bytes(path: &std::ffi::OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_bytes()
}

#[cfg(not(unix))]
fn os_path_bytes(path: &std::ffi::OsStr) -> &[u8] {
    path.to_str().unwrap_or_default().as_bytes()
}

fn validate_relative_path(path: &str) -> Result<PathBuf, LifecycleError> {
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(LifecycleError::Invalid(format!(
            "copy path must remain inside the repository: {path:?}"
        )));
    }
    Ok(relative.to_path_buf())
}

async fn safe_stage(path: &Path, prefix: &str, default_personal: &str) -> Result<String, String> {
    let path = path.to_owned();
    let prefix = prefix.to_owned();
    let stage = tokio::task::spawn_blocking(move || wt_sst::safe_pinned_stage(&path, &prefix))
        .await
        .map_err(|error| format!("inspect stage pin: {error}"))?
        .map_err(|error| error.to_string())?;
    if stage == default_personal {
        return Err(format!("{stage:?} is the protected default personal stage"));
    }
    Ok(stage)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn now_iso() -> String {
    let millis = now_ms();
    let seconds = millis.div_euclid(1000);
    let day_seconds = seconds.rem_euclid(86_400);
    let days = seconds.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        day_seconds / 3600,
        day_seconds / 60 % 60,
        day_seconds % 60,
        millis.rem_euclid(1000)
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn normalize_head_ref(reference: &str) -> String {
    if reference.starts_with("refs/heads/") {
        reference.to_owned()
    } else {
        format!("refs/heads/{reference}")
    }
}

fn is_stale_rift_registry_error(stderr: &[u8], stdout: &[u8]) -> bool {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(stderr),
        String::from_utf8_lossy(stdout)
    );
    let lower = text.to_ascii_lowercase();
    lower.contains("unique constraint")
        || lower.contains("already registered")
        || lower.contains("already exists")
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn prioritized_rift_gc(rift_binary: &OsString) -> CommandSpec {
    let mut program = PathBuf::from(rift_binary);
    let mut args = vec![OsString::from("gc")];
    if let Some(nice) = find_executable("nice") {
        args.splice(
            0..0,
            [
                OsString::from("-n"),
                OsString::from("10"),
                rift_binary.clone(),
            ],
        );
        program = nice;
    }
    let mut spec = if cfg!(target_os = "macos") {
        find_executable("taskpolicy").map(|taskpolicy| {
            let mut spec = CommandSpec::new(taskpolicy);
            spec.args = [OsString::from("-b"), program.clone().into_os_string()]
                .into_iter()
                .chain(args.clone())
                .collect();
            spec
        })
    } else {
        None
    }
    .unwrap_or_else(|| {
        let mut spec = CommandSpec::new(program);
        spec.args = args;
        spec
    });
    spec.timeout = Duration::from_secs(20 * 60);
    spec
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn stage_removal_rejects_default_foreign_and_invalid_pins() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".sst")).unwrap();
        let pin = root.path().join(".sst/stage");
        for stage in [
            "m-personal",
            "production",
            "m-../production",
            "m-work\n--prod",
        ] {
            std::fs::write(&pin, stage).unwrap();
            assert!(
                super::safe_stage(root.path(), "m-", "m-personal")
                    .await
                    .is_err(),
                "{stage}"
            );
        }
        std::fs::write(&pin, "m-work\n").unwrap();
        assert_eq!(
            super::safe_stage(root.path(), "m-", "m-personal")
                .await
                .unwrap(),
            "m-work"
        );
        assert!(
            super::safe_stage(root.path(), "", "m-personal")
                .await
                .is_err()
        );
    }

    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;
    use wt_vcs::{RepositoryConfig, StageConfig};

    async fn run(runner: &ProcessRunner, cwd: &Path, args: &[&str]) -> Vec<u8> {
        let mut spec = CommandSpec::new("git");
        spec.args = args.iter().map(OsString::from).collect();
        spec.cwd = Some(cwd.to_path_buf());
        spec.timeout = Duration::from_secs(15);
        spec.env = vec![
            ("GIT_CONFIG_NOSYSTEM".into(), Some("1".into())),
            ("GIT_CONFIG_GLOBAL".into(), Some("/dev/null".into())),
            ("GIT_TERMINAL_PROMPT".into(), Some("0".into())),
            ("GIT_AUTHOR_NAME".into(), Some("wt test".into())),
            (
                "GIT_AUTHOR_EMAIL".into(),
                Some("wt-test@example.invalid".into()),
            ),
            ("GIT_COMMITTER_NAME".into(), Some("wt test".into())),
            (
                "GIT_COMMITTER_EMAIL".into(),
                Some("wt-test@example.invalid".into()),
            ),
        ];
        runner
            .run(spec, &CancellationToken::new())
            .await
            .unwrap()
            .checked("git fixture")
            .unwrap()
            .stdout
    }

    async fn setup() -> (TempDir, LifecycleService, ProcessRunner, PathBuf, PathBuf) {
        let scratch = tempfile::tempdir().unwrap();
        let main = scratch.path().join("main clone");
        let root = scratch.path().join("worktree root");
        let state = scratch.path().join("wt-state.db");
        let lock_dir = scratch.path().join("locks");
        fs::create_dir_all(&main).await.unwrap();
        fs::create_dir_all(&root).await.unwrap();
        let runner = ProcessRunner::default();
        run(&runner, &main, &["init", "-b", "main"]).await;
        run(&runner, &main, &["config", "user.name", "wt test"]).await;
        run(
            &runner,
            &main,
            &["config", "user.email", "wt-test@example.invalid"],
        )
        .await;
        fs::write(main.join("tracked.txt"), "base\n").await.unwrap();
        run(&runner, &main, &["add", "tracked.txt"]).await;
        run(&runner, &main, &["commit", "-m", "initial"]).await;
        let repository = GitRepository::new(
            RepositoryConfig {
                main_clone: main.clone(),
                worktree_root: root.clone(),
                trunk_branch: "main".into(),
                stage: StageConfig {
                    prefix: "stage".into(),
                    issue_id_pattern: r"([A-Z]+-\d+)".into(),
                },
            },
            runner.clone(),
        );
        let config = ServiceConfig {
            main_clone: main.clone(),
            worktree_root: root.clone(),
            lock_dir,
            state: StoreLocation {
                path: state,
                identity: RepositoryIdentity::new("fixture", scratch.path().to_string_lossy()),
            },
            branch_prefix: "michael".into(),
            base_branch: "main".into(),
            keep_fresh: Vec::new(),
            auto_regen_paths: Vec::new(),
            branch_id_pattern: r"([A-Z]+-\d+)".into(),
            slug_max_len: 50,
            stage_prefix: "stage".into(),
            default_personal_stage: "stagepersonal".into(),
            backend: BackendKind::GitWorktree,
            copy_files: vec![],
            copy_globs: vec![],
            install_command: None,
            destroy_command: None,
            has_sst: false,
            reserved_slugs: ["manager".into()].into(),
            rift_binary: "rift".into(),
            shell: "/bin/sh".into(),
        };
        (
            scratch,
            LifecycleService::new(config, repository, runner.clone()),
            runner,
            main,
            root,
        )
    }

    #[tokio::test]
    async fn create_and_remove_linked_worktree_preserves_fork_anchor() {
        let (_scratch, service, runner, main, root) = setup().await;
        let cancellation = CancellationToken::new();
        let created = service
            .create(
                "michael/ENG-42-feature",
                CreateOptions {
                    base: Some("refs/heads/main".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(created.target.slug(), "ENG-42-feature");
        assert!(Path::new(&created.target.path).join(".git").exists());
        let slug_state = tokio::task::spawn_blocking({
            let path = service.config.state.path.clone();
            let identity = service.config.state.identity.clone();
            move || {
                Store::open_read_only(path, identity)
                    .unwrap()
                    .read_slug_state("ENG-42-feature")
                    .unwrap()
            }
        })
        .await
        .unwrap();
        assert!(
            slug_state
                .as_ref()
                .and_then(|v| v.get("baseSha"))
                .and_then(Value::as_str)
                .is_some()
        );

        let removed = service
            .remove(
                &created.target,
                RemoveOptions {
                    delete_branch: true,
                    ..RemoveOptions::default()
                },
                &cancellation,
            )
            .await
            .unwrap();
        assert!(removed.removed && removed.deleted_branch);
        assert!(!Path::new(&created.target.path).exists());
        assert!(!root.join("ENG-42-feature").exists());
        assert!(
            run(
                &runner,
                &main,
                &["branch", "--list", "michael/ENG-42-feature"]
            )
            .await
            .is_empty()
        );
    }

    #[tokio::test]
    async fn default_origin_and_short_stacked_refs_resolve_as_commits() {
        let (_scratch, service, runner, main, _root) = setup().await;
        run(
            &runner,
            &main,
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        )
        .await;
        let cancellation = CancellationToken::new();
        let trunk = service
            .create(
                "michael/ENG-21-trunk-child",
                CreateOptions {
                    fetch_origin: false,
                    run_install: false,
                    ..CreateOptions::default()
                },
                &cancellation,
            )
            .await
            .unwrap();
        let parent = service
            .create(
                "michael/ENG-22-parent",
                CreateOptions {
                    base: Some("refs/heads/main".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        let child = service
            .create(
                "michael/ENG-23-child",
                CreateOptions {
                    base: Some("michael/ENG-22-parent".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        for (slug, expected_base) in [
            ("ENG-21-trunk-child", "main"),
            ("ENG-23-child", "michael/ENG-22-parent"),
        ] {
            let slug = slug.to_owned();
            let state = tokio::task::spawn_blocking({
                let path = service.config.state.path.clone();
                let identity = service.config.state.identity.clone();
                move || {
                    Store::open_read_only(path, identity)
                        .unwrap()
                        .read_slug_state(&slug)
                        .unwrap()
                        .unwrap()
                }
            })
            .await
            .unwrap();
            assert_eq!(state["baseBranch"], expected_base);
            assert!(state["baseSha"].as_str().is_some());
        }
        // Avoid leaving fixture checkouts around so worktree metadata is checked
        // by the same removal implementation this test exercises elsewhere.
        for target in [child.target, parent.target, trunk.target] {
            service
                .remove(
                    &target,
                    RemoveOptions {
                        force: true,
                        delete_branch: true,
                        landed: true,
                        destroy_stage: false,
                    },
                    &cancellation,
                )
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn dirty_checkout_refuses_removal_without_explicit_force() {
        let (_scratch, service, _runner, _main, _root) = setup().await;
        let cancellation = CancellationToken::new();
        let created = service
            .create(
                "michael/ENG-9-dirty",
                CreateOptions {
                    base: Some("refs/heads/main".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        fs::write(
            Path::new(&created.target.path).join("tracked.txt"),
            "dirty\n",
        )
        .await
        .unwrap();
        assert!(matches!(
            service.remove(&created.target, RemoveOptions::default(), &cancellation).await,
            Err(LifecycleError::Refused(message)) if message.contains("uncommitted")
        ));
        assert!(Path::new(&created.target.path).exists());
    }

    #[tokio::test]
    async fn force_removal_revision_rejects_changed_diff_even_when_hazard_label_is_same() {
        let (_scratch, service, _runner, _main, _root) = setup().await;
        let cancellation = CancellationToken::new();
        let created = service
            .create(
                "michael/ENG-77-revision",
                CreateOptions {
                    base: Some("refs/heads/main".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        let path = Path::new(&created.target.path);
        fs::write(path.join("tracked.txt"), "first dirty content\n")
            .await
            .unwrap();
        let first = service
            .removal_revision(&created.target, false, &cancellation)
            .await
            .unwrap();
        assert_eq!(first.hazards, ["uncommitted changes"]);
        fs::write(path.join("tracked.txt"), "second dirty content\n")
            .await
            .unwrap();
        let second = service
            .removal_revision(&created.target, false, &cancellation)
            .await
            .unwrap();
        assert_eq!(second.hazards, first.hazards);
        assert_ne!(second.digest, first.digest);
        fs::write(path.join("untracked same-size.txt"), "alpha\n")
            .await
            .unwrap();
        let third = service
            .removal_revision(&created.target, false, &cancellation)
            .await
            .unwrap();
        fs::write(path.join("untracked same-size.txt"), "bravo\n")
            .await
            .unwrap();
        let fourth = service
            .removal_revision(&created.target, false, &cancellation)
            .await
            .unwrap();
        assert_eq!(third.hazards, fourth.hazards);
        assert_ne!(third.digest, fourth.digest);
        let result = service
            .remove_with_revision(
                &created.target,
                RemoveOptions {
                    force: true,
                    delete_branch: true,
                    landed: false,
                    destroy_stage: false,
                },
                &first,
                &cancellation,
            )
            .await;
        assert!(matches!(result, Err(LifecycleError::Refused(_))));
        assert!(path.exists());
    }

    #[tokio::test]
    async fn committed_but_unpushed_work_refuses_removal() {
        let (_scratch, service, runner, _main, _root) = setup().await;
        let cancellation = CancellationToken::new();
        let created = service
            .create(
                "michael/ENG-10-unpushed",
                CreateOptions {
                    base: Some("refs/heads/main".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        let path = PathBuf::from(&created.target.path);
        fs::write(path.join("tracked.txt"), "new work\n")
            .await
            .unwrap();
        run(&runner, &path, &["add", "tracked.txt"]).await;
        run(&runner, &path, &["commit", "-m", "unpublished work"]).await;
        assert!(matches!(
            service
                .remove(&created.target, RemoveOptions::default(), &cancellation)
                .await,
            Err(LifecycleError::Refused(message)) if message.contains("unpushed commit")
        ));
        assert!(path.exists());
    }

    #[tokio::test]
    async fn archive_restore_are_idempotent_state_toggles() {
        let (_scratch, service, _runner, _main, _root) = setup().await;
        let cancellation = CancellationToken::new();
        assert!(service.archive("ENG-4", true, &cancellation).await.unwrap());
        assert!(!service.archive("ENG-4", true, &cancellation).await.unwrap());
        assert!(service.restore("ENG-4", &cancellation).await.unwrap());
        assert!(!service.restore("ENG-4", &cancellation).await.unwrap());
    }

    #[tokio::test]
    async fn real_rift_create_and_remove_use_isolated_registry() {
        let binary = Path::new("/opt/homebrew/bin/rift");
        if !binary.is_file() {
            eprintln!("skipping real Rift integration: /opt/homebrew/bin/rift is unavailable");
            return;
        }
        use std::os::unix::fs::PermissionsExt;

        let (scratch, mut service, runner, main, root) = setup().await;
        let home = scratch.path().join("isolated rift home");
        let data = home.join("xdg-data");
        let config = home.join("xdg-config");
        fs::create_dir_all(&data).await.unwrap();
        fs::create_dir_all(&config).await.unwrap();
        let wrapper = scratch.path().join("rift-isolated");
        let script = format!(
            "#!/bin/sh\nexport HOME='{}'\nexport XDG_DATA_HOME='{}'\nexport XDG_CONFIG_HOME='{}'\nexec '{}' \"$@\"\n",
            home.display(),
            data.display(),
            config.display(),
            binary.display()
        );
        fs::write(&wrapper, script).await.unwrap();
        let mut permissions = fs::metadata(&wrapper).await.unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&wrapper, permissions).await.unwrap();
        service.config.backend = BackendKind::Rift;
        service.config.rift_binary = wrapper.into_os_string();

        let cancellation = CancellationToken::new();
        let created = service
            .create(
                "michael/ENG-88-rift-integration",
                CreateOptions {
                    base: Some("refs/heads/main".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        assert!(Path::new(&created.target.path).join(".rift").is_file());
        assert!(
            Path::new(&created.target.path)
                .join("tracked.txt")
                .is_file()
        );
        let copied_ref = run(
            &runner,
            Path::new(&created.target.path),
            &["rev-parse", "refs/heads/main"],
        )
        .await;
        assert!(
            !copied_ref.is_empty(),
            "Rift clone should retain main-clone refs"
        );
        assert!(root.join("ENG-88-rift-integration").is_dir());
        let rows = service.repository.inventory(&cancellation).await.unwrap();
        assert!(
            rows.iter()
                .any(|row| row.target.slug() == "ENG-88-rift-integration")
        );

        service
            .remove(
                &created.target,
                RemoveOptions {
                    force: false,
                    delete_branch: true,
                    landed: false,
                    destroy_stage: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        assert!(!Path::new(&created.target.path).exists());
        assert!(!root.join("ENG-88-rift-integration").exists());

        let stale_slug = "ENG-89-stale";
        let mut stale_create = CommandSpec::new(service.config.rift_binary.clone());
        stale_create.args = ["create", "--name", stale_slug, "--into"]
            .into_iter()
            .map(OsString::from)
            .chain([
                root.as_os_str().to_os_string(),
                OsString::from("--copy-all"),
            ])
            .collect();
        stale_create.cwd = Some(main.clone());
        let stale = runner.run(stale_create, &cancellation).await.unwrap();
        stale.checked("rift create stale fixture").unwrap();
        fs::remove_dir_all(root.join(stale_slug)).await.unwrap();
        let retried = service
            .create(
                "michael/ENG-89-stale",
                CreateOptions {
                    base: Some("refs/heads/main".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        assert!(Path::new(&retried.target.path).join(".rift").is_file());
        service
            .remove(
                &retried.target,
                RemoveOptions {
                    force: false,
                    delete_branch: true,
                    landed: false,
                    destroy_stage: false,
                },
                &cancellation,
            )
            .await
            .unwrap();

        let parent = service
            .create(
                "michael/ENG-90-parent",
                CreateOptions {
                    base: Some("refs/heads/main".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        let parent_path = Path::new(&parent.target.path);
        fs::write(parent_path.join("parent-only.txt"), "stack base\n")
            .await
            .unwrap();
        run(&runner, parent_path, &["add", "parent-only.txt"]).await;
        run(
            &runner,
            parent_path,
            &["commit", "-m", "parent-only commit"],
        )
        .await;
        let parent_head = run(&runner, parent_path, &["rev-parse", "HEAD"]).await;
        let child = service
            .create(
                "michael/ENG-91-child",
                CreateOptions {
                    base: Some("michael/ENG-90-parent".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        let child_path = Path::new(&child.target.path);
        assert_eq!(
            run(&runner, child_path, &["rev-parse", "HEAD"]).await,
            parent_head
        );
        assert!(child_path.join("parent-only.txt").is_file());
        service
            .remove(
                &child.target,
                RemoveOptions {
                    force: true,
                    delete_branch: true,
                    landed: false,
                    destroy_stage: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        service
            .remove(
                &parent.target,
                RemoveOptions {
                    force: true,
                    delete_branch: true,
                    landed: false,
                    destroy_stage: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
    }
}
