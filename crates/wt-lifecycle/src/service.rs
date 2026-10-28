use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{collections::BTreeSet, future::Future, pin::Pin, sync::Arc};

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
    process::{CommandSpec, ProcessError, ProcessRunner, ProcessStream},
};
use wt_store::{RemovedWorktree, RepositoryIdentity, Store, StoreError};
use wt_vcs::{GitRepository, WorktreeRecord};

use crate::removal_trace::{RemovalPhase, timed};

/// Refusal when the removal shown to the user is no longer the current one.
/// Callers match it to report a stale request rather than a failure.
pub const REMOVED_CHANGED: &str = "removed worktree changed after the history view was rendered";
const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
const PROCESS_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
/// Bounded retries of `rm` on an already-verified quarantine. Only
/// `Directory not empty` (entries created mid-deletion) is retried.
const QUARANTINE_DELETE_ATTEMPTS: u32 = 4;

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
    /// Caller snapshot captured while the row was visible. Its display data is
    /// persisted at the removal commit point after identity is revalidated.
    pub removed_snapshot: Option<RemovedWorktree>,
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
    /// A local-ancestry landing claim must survive delayed/background execution.
    /// The worker rechecks this exact advertised base before destructive work.
    #[serde(default)]
    pub published_base: Option<Box<(String, String)>>,
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
    /// The checkout changed while its removal evidence was read: active
    /// work, not a broken checkout. Callers may retry later.
    #[error("refusing lifecycle operation: {0}")]
    CheckoutChanging(String),
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
    #[error(
        "removal did not finish for {path}; files may already be missing; inspect the remaining checkout before retrying: {source}"
    )]
    IncompleteRemoval {
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
    before_remove: Option<BeforeRemoveHook>,
    sst_program: OsString,
    resolved_rift: Arc<tokio::sync::OnceCell<OsString>>,
}

type BeforeRemoveHook = Arc<
    dyn Fn(WorktreeTarget, CancellationToken) -> Pin<Box<dyn Future<Output = Vec<String>> + Send>>
        + Send
        + Sync,
>;

#[derive(Default)]
struct CreateProgress {
    checkout_created: bool,
    checkout_attempted: bool,
    branch_created: bool,
}

struct ReviewHead {
    number: u64,
    expected: String,
}

impl LifecycleService {
    pub fn new(config: ServiceConfig, repository: GitRepository, runner: ProcessRunner) -> Self {
        let id_pattern = regex::RegexBuilder::new(&config.branch_id_pattern)
            .case_insensitive(true)
            .build()
            .ok();
        Self {
            config,
            repository,
            runner,
            id_pattern,
            dev: None,
            before_remove: None,
            sst_program: "pnpm".into(),
            resolved_rift: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    pub fn with_dev_server(mut self, dev: wt_dev::DevServerService) -> Self {
        self.dev = Some(dev);
        self
    }

    /// Install app-owned teardown that runs after the slug lock and removal
    /// revision/hazard checks have passed, before project hooks or deletion.
    /// Cleanup failures are retained as warnings; cancellation still aborts.
    pub fn with_before_remove<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(WorktreeTarget, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Vec<String>> + Send + 'static,
    {
        self.before_remove = Some(Arc::new(move |target, cancellation| {
            Box::pin(hook(target, cancellation))
        }));
        self
    }

    /// The stable inventory key a branch will use under this host's naming
    /// policy. Preparation does not create directories or Git objects.
    pub fn creation_key(&self, branch: &str) -> String {
        wt_core::worktree_ledger_key(&wt_core::WorktreeRef::Local {
            slug: self.dir_slug(branch),
        })
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
        self.create_locked(branch, &slug, options, cancellation)
            .await
    }

    /// Create a worktree from a pinned GitHub pull-request head when the
    /// request's branch is absent from both local and origin branch refs.
    /// The branch lock covers collision checks, fetch, and checkout. The
    /// synthetic fetch ref is never persisted as stack metadata.
    pub async fn create_from_pull_request(
        &self,
        branch: &str,
        pull_number: u64,
        expected_head: &str,
        options: CreateOptions,
        cancellation: &CancellationToken,
    ) -> Result<CreateResult, LifecycleError> {
        let branch = branch.trim();
        if branch.is_empty() {
            return Err(LifecycleError::Invalid("branch must not be empty".into()));
        }
        if !valid_object_id(expected_head) {
            return Err(LifecycleError::Invalid(
                "pull request head must be a full hexadecimal object id".into(),
            ));
        }
        let slug = self.dir_slug(branch);
        let _lock = self
            .acquire_lock(&slug, "create review worktree", cancellation)
            .await?;
        self.create_locked_inner(
            branch,
            &slug,
            options,
            Some(ReviewHead {
                number: pull_number,
                expected: expected_head.to_ascii_lowercase(),
            }),
            cancellation,
        )
        .await
    }

    /// Restore a removed checkout only if the exact removal shown to the user
    /// is still current. The check and create run under the same per-slug lock
    /// used by create/remove, so a newer removal cannot be consumed by a stale
    /// history selection.
    pub async fn create_from_removed(
        &self,
        slug: &str,
        expected_branch: &str,
        expected_removed_at: &str,
        options: CreateOptions,
        cancellation: &CancellationToken,
    ) -> Result<CreateResult, LifecycleError> {
        let _lock = self.acquire_lock(slug, "restore", cancellation).await?;
        let expected_slug = slug.to_owned();
        let expected_branch_owned = expected_branch.to_owned();
        let expected_removed_at_owned = expected_removed_at.to_owned();
        let record = self
            .mutate_store(cancellation, move |store| {
                Ok(store.read_removed_worktrees()?.into_iter().find(|entry| {
                    entry.slug == expected_slug
                        && entry.branch == expected_branch_owned
                        && entry.removed_at == expected_removed_at_owned
                }))
            })
            .await?
            .ok_or_else(|| LifecycleError::Refused(REMOVED_CHANGED.into()))?;
        let branch_slug = self.dir_slug(&record.branch);
        if branch_slug != slug {
            return Err(LifecycleError::Refused(format!(
                "removed branch {:?} no longer maps to the recorded slug {slug:?}",
                record.branch
            )));
        }
        match self
            .create_locked(&record.branch, slug, options, cancellation)
            .await
        {
            Ok(result) => Ok(result),
            Err(error) => {
                let record_to_restore = record.clone();
                let slug_to_restore = slug.to_owned();
                if let Err(restore_error) = self
                    .mutate_store(&CancellationToken::new(), move |store| {
                        store.restore_removed_worktree_if_absent(
                            &slug_to_restore,
                            &record_to_restore,
                        )?;
                        Ok(())
                    })
                    .await
                {
                    return Err(LifecycleError::Rollback {
                        primary: error.to_string(),
                        rollback: format!("preserve removed-worktree history: {restore_error}"),
                    });
                }
                Err(error)
            }
        }
    }

    async fn create_locked(
        &self,
        branch: &str,
        slug: &str,
        options: CreateOptions,
        cancellation: &CancellationToken,
    ) -> Result<CreateResult, LifecycleError> {
        self.create_locked_inner(branch, slug, options, None, cancellation)
            .await
    }

    async fn create_locked_inner(
        &self,
        branch: &str,
        slug: &str,
        options: CreateOptions,
        review_head: Option<ReviewHead>,
        cancellation: &CancellationToken,
    ) -> Result<CreateResult, LifecycleError> {
        let phase = crate::creation_trace::CreationPhase::new(
            &self.config.worktree_root.join(slug),
            "create-transaction",
        );
        let result = self
            .create_transaction(branch, slug, options, review_head, cancellation)
            .await;
        phase.lifecycle(&result);
        result
    }

    async fn create_transaction(
        &self,
        branch: &str,
        slug: &str,
        options: CreateOptions,
        review_head: Option<ReviewHead>,
        cancellation: &CancellationToken,
    ) -> Result<CreateResult, LifecycleError> {
        if self.config.reserved_slugs.contains(slug) {
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
        let path = root.join(slug);
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
            let phase = crate::creation_trace::CreationPhase::new(
                &path,
                "fetch-origin-including-maintenance",
            );
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
                .await
                .map_err(LifecycleError::from);
            phase.lifecycle(&report);
            let report = report?;
            for warning in report.warnings {
                tracing::warn!(%warning, "Git ref maintenance before creation");
            }
        }
        let local_ref = format!("refs/heads/{branch}");
        let remote_ref = format!("refs/remotes/origin/{branch}");
        let local_oid = self.resolve_ref(&main, &local_ref, cancellation).await?;
        let remote_oid = self.resolve_ref(&main, &remote_ref, cancellation).await?;
        let (existing, base_ref, logical_base) = if let Some(review_head) = &review_head {
            for (name, oid) in [("local", &local_oid), ("origin", &remote_oid)] {
                if let Some(oid) = oid
                    && !oid.eq_ignore_ascii_case(&review_head.expected)
                {
                    return Err(LifecycleError::Refused(format!(
                        "review branch {branch:?} already exists on {name} at {oid}, not the reviewed PR head {}; refusing to check out a collision",
                        review_head.expected
                    )));
                }
            }
            if local_oid.is_some() || remote_oid.is_some() {
                (true, None, None)
            } else {
                let pinned_ref = format!("refs/wt/reviews/{}", review_head.number);
                let refspec = format!("+refs/pull/{}/head:{pinned_ref}", review_head.number);
                let fetched_output = self
                    .run_git_raw(
                        &main,
                        ["fetch", "--no-tags", "origin", &refspec],
                        cancellation,
                        "fetch reviewed pull-request head",
                    )
                    .await?;
                if !fetched_output.status.success() {
                    return Err(process_error(
                        "fetch reviewed pull-request head",
                        &main,
                        fetched_output
                            .checked("fetch reviewed pull-request head")
                            .unwrap_err(),
                    ));
                }
                let fetched = self
                    .resolve_ref(&main, &pinned_ref, cancellation)
                    .await?
                    .ok_or_else(|| {
                        LifecycleError::Refused(
                            "GitHub pull-request head fetch produced no pinned commit".into(),
                        )
                    })?;
                if !fetched.eq_ignore_ascii_case(&review_head.expected) {
                    return Err(LifecycleError::Refused(format!(
                        "pull request #{} changed during checkout: expected head {}, fetched {fetched}",
                        review_head.number, review_head.expected
                    )));
                }
                let configured_base = self.config.base_branch.clone();
                let logical_name = configured_base
                    .strip_prefix("origin/")
                    .unwrap_or(&configured_base)
                    .to_owned();
                let logical_ref = if configured_base.starts_with("origin/") {
                    configured_base
                } else {
                    format!("origin/{configured_base}")
                };
                if !self.ref_exists(&main, &logical_ref, cancellation).await? {
                    return Err(LifecycleError::Refused(format!(
                        "configured base {logical_ref:?} is unavailable for the review branch anchor"
                    )));
                }
                (
                    false,
                    Some(review_head.expected.clone()),
                    Some((logical_ref, logical_name)),
                )
            }
        } else {
            let existing = local_oid.is_some() || remote_oid.is_some();
            let base = if existing {
                None
            } else {
                Some(options.base.clone().unwrap_or_else(|| {
                    if self.config.base_branch.starts_with("origin/") {
                        self.config.base_branch.clone()
                    } else {
                        format!("origin/{}", self.config.base_branch)
                    }
                }))
            };
            (existing, base, None)
        };
        let base_source = match (base_ref.as_deref(), review_head.is_some()) {
            (Some(base), false) if !base.starts_with("origin/") => {
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
        let stage = self.compute_stage(slug);
        let mut progress = CreateProgress::default();
        let created = self
            .create_inner(
                branch,
                slug,
                &path,
                &main,
                base_ref.as_deref(),
                base_source.as_deref(),
                existing,
                logical_base
                    .as_ref()
                    .map(|(reference, name)| (reference.as_str(), name.as_str())),
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
        logical_base: Option<(&str, &str)>,
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
        if let Some((base_reference, base_branch)) = logical_base {
            let anchor = self
                .checked_text(
                    path,
                    ["merge-base", base_reference, "HEAD"],
                    cancellation,
                    "record review branch fork point",
                )
                .await?;
            let slug = slug.to_owned();
            let base_branch = base_branch.to_owned();
            self.mutate_store(cancellation, move |store| {
                store
                    .set_slug_base(&slug, Some((&base_branch, Some(anchor.trim()))))
                    .map(|_| ())
            })
            .await?;
        } else if let Some(base) = base_ref {
            let base_branch = logical_base_branch(base);
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
        let configured_base = logical_base.map(|(reference, _)| reference).or(base_ref);
        self.configure_branch(branch, path, configured_base, existing, cancellation)
            .await?;
        let phase = crate::creation_trace::CreationPhase::new(path, "configured-file-copy");
        let copied = self.copy_files(main, path, cancellation).await;
        phase.lifecycle(&copied);
        copied?;
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
        if checkout_missing(&row).await {
            return Ok(RemovalRevision {
                key: wt_core::worktree_target_key(&row.target),
                path: row.target.path.clone(),
                branch: row.target.branch.clone(),
                head: String::new(),
                digest: MISSING_CHECKOUT_DIGEST.into(),
                hazards: vec![MISSING_CHECKOUT_HAZARD.into()],
                published_base: None,
            });
        }
        let phase = RemovalPhase::new(Path::new(&target.path), "prepare-revision");
        let result = self
            .stable_removal_revision(&row, landed, cancellation)
            .await;
        phase.lifecycle(&result);
        result
    }

    async fn stable_removal_revision(
        &self,
        row: &WorktreeRecord,
        landed: bool,
        cancellation: &CancellationToken,
    ) -> Result<RemovalRevision, LifecycleError> {
        for _ in 0..2 {
            let before = self.capture_revision_for_row(row, cancellation).await?;
            let hazards = self
                .collect_removal_hazards(row, landed, cancellation)
                .await?;
            let mut after = self.capture_revision_for_row(row, cancellation).await?;
            if before == after {
                after.hazards = hazards;
                return Ok(after);
            }
        }
        Err(LifecycleError::CheckoutChanging(
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
                        removed_snapshot: None,
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
        let total = RemovalPhase::new(Path::new(&target.path), "total");
        let result = self
            .remove_locked(target, options, cleanup, expected_revision, cancellation)
            .await;
        total.lifecycle(&result);
        result
    }

    async fn remove_locked(
        &self,
        target: &WorktreeTarget,
        options: RemoveOptions,
        cleanup: bool,
        expected_revision: Option<&RemovalRevision>,
        cancellation: &CancellationToken,
    ) -> Result<RemoveResult, LifecycleError> {
        let slug = target.slug();
        let trace = Path::new(&target.path);
        let lock = timed(
            trace,
            "slug-lock-wait",
            self.acquire_lock(slug, "remove", cancellation),
        )
        .await?;
        let root = canonicalize(&self.config.worktree_root, "resolve worktree root").await?;
        let main = canonicalize(&self.config.main_clone, "resolve main clone").await?;
        let requested = PathBuf::from(&target.path);
        // A checkout that is already gone keeps its registered path, which
        // is also how the inventory reports it.
        let path = match fs::canonicalize(&requested).await {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => requested.clone(),
            Err(source) => {
                return Err(LifecycleError::Io {
                    operation: "resolve target checkout",
                    path: requested,
                    source,
                });
            }
        };
        if path == main {
            return Err(LifecycleError::Refused(
                "cannot remove the configured main clone".into(),
            ));
        }
        self.ensure_managed_path(&path, &root)?;
        let row = timed(
            trace,
            "find-live-target",
            self.find_live_target(target, cancellation),
        )
        .await?;
        if row.is_main {
            return Err(LifecycleError::Refused(
                "cannot remove the configured main clone".into(),
            ));
        }
        if let Some(snapshot) = &options.removed_snapshot
            && (snapshot.slug != slug || snapshot.branch != row.target.branch)
        {
            return Err(LifecycleError::Refused(
                "removed-history snapshot no longer matches this worktree".into(),
            ));
        }
        if checkout_missing(&row).await {
            return self
                .finish_missing_checkout(
                    &row,
                    &main,
                    options,
                    expected_revision,
                    &lock,
                    cancellation,
                )
                .await;
        }
        if let Some(expected) = expected_revision {
            timed(
                trace,
                "verify-revision-initial",
                self.verify_removal_revision(
                    &row,
                    options.landed || cleanup,
                    expected,
                    cancellation,
                ),
            )
            .await?;
        }
        if !options.force {
            timed(
                trace,
                "guard-hazards",
                self.guard_removal(&row, options.landed || cleanup, cancellation),
            )
            .await?;
        }
        // A successful SST removal switches checkout removal to forced mode
        // (it rewrites generated files). Without a caller witness, pin the
        // clean checkout just verified so edits made during hooks or the
        // long-running SST command are still refused rather than forced away.
        let witness = if expected_revision.is_none()
            && !options.force
            && options.destroy_stage
            && self.config.has_sst
        {
            Some(
                timed(
                    trace,
                    "capture-sst-witness",
                    self.capture_revision_for_row(&row, cancellation),
                )
                .await?,
            )
        } else {
            None
        };
        let mut warnings = Vec::new();
        if let Some(before_remove) = &self.before_remove {
            let phase = RemovalPhase::new(trace, "before-remove-sessions-and-listeners");
            warnings.extend(before_remove(row.target.clone(), cancellation.clone()).await);
            phase.finish(if cancellation.is_cancelled() {
                "cancelled"
            } else {
                "success"
            });
            if cancellation.is_cancelled() {
                return Err(LifecycleError::Cancelled);
            }
        }
        if let Some(dev) = &self.dev {
            // Keep the slug lock through supervision cleanup and checkout
            // removal. A new server cannot start in the gap, and a failed
            // external teardown leaves its checkout available for recovery.
            timed(trace, "dev-server-stop", async {
                dev.stop_under_lifecycle_lock(&wt_dev::DevWorktree::from(&row), &lock, cancellation)
                    .await
                    .map_err(LifecycleError::from)
            })
            .await?;
        }
        let mut force = options.force;
        let mut destroyed_stage = false;
        let mut verified_revision = expected_revision.cloned().or(witness);
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
                        // Revalidate immediately before the external destructive
                        // operation. Hooks above may have changed the checkout
                        // since the user's confirmation was captured.
                        if let Some(expected) = verified_revision.as_ref() {
                            self.verify_removal_revision(
                                &row,
                                options.landed || cleanup,
                                expected,
                                cancellation,
                            )
                            .await?;
                        }
                        let before_sst = if verified_revision.is_some() {
                            let snapshot =
                                self.capture_revision_for_row(&row, cancellation).await?;
                            if let Some(expected) = verified_revision.as_ref()
                                && !same_checkout_revision(&snapshot, expected)
                            {
                                return Err(LifecycleError::Refused(
                                    "checkout changed before SST removal; review the new state before removing".into(),
                                ));
                            }
                            let outside = self
                                .capture_revision_for_row_excluding(
                                    &row,
                                    &self.config.auto_regen_paths,
                                    cancellation,
                                )
                                .await?;
                            Some((snapshot, outside))
                        } else {
                            None
                        };
                        let mut spec = CommandSpec::new(self.sst_program.clone())
                            .args(["sst", "remove", "--stage", &stage]);
                        spec.cwd = Some(path.clone());
                        spec.timeout = Duration::from_secs(20 * 60);
                        spec.output_limit = PROCESS_OUTPUT_LIMIT;
                        let phase = RemovalPhase::new(trace, "sst-remove");
                        let sst = self.runner.run(spec, cancellation).await;
                        phase.finish(match &sst {
                            Ok(output) if output.status.success() => "success",
                            _ => "error",
                        });
                        match sst {
                            Ok(output) if output.status.success() => {
                                destroyed_stage = true;
                                force = true;
                                if let (Some(expected), Some((before_sst, before_outside))) =
                                    (verified_revision.as_ref(), before_sst.as_ref())
                                {
                                    verified_revision = Some(
                                        self.rebase_revision_after_sst(
                                            &row,
                                            options.landed || cleanup,
                                            expected,
                                            before_sst,
                                            before_outside,
                                            cancellation,
                                        )
                                        .await?,
                                    );
                                }
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
        if let Err(error) = timed(
            trace,
            "destroy-hook",
            self.run_destroy_hook(&path, slug, cancellation),
        )
        .await
        {
            if cancellation.is_cancelled() {
                return Err(LifecycleError::Cancelled);
            }
            warnings.push(error.to_string());
        }
        let backend = self.backend_for(&path).await?;
        // Quiesce wt's own writers before the final witness. Background origin
        // sync skips this clone from here on and any in-flight sync of it
        // finishes first, so the verified state is also the deleted state.
        let _clone_maintenance = if backend == BackendKind::Rift {
            Some(
                timed(trace, "clone-maintenance-lock-wait", async {
                    self.repository
                        .lock_clone_maintenance(slug, cancellation)
                        .await
                        .map_err(|error| match error {
                            wt_vcs::RepositoryError::Cancelled => LifecycleError::Cancelled,
                            error => error.into(),
                        })
                })
                .await?,
            )
        } else {
            None
        };
        if let Some(expected) = verified_revision.as_ref() {
            timed(
                trace,
                "verify-revision-final",
                self.verify_removal_revision(
                    &row,
                    options.landed || cleanup,
                    expected,
                    cancellation,
                ),
            )
            .await?;
        }
        // Status reads above may have started an fsmonitor daemon; it writes
        // cookies under the Git dir and makes a concurrent `rm` fail.
        self.stop_fsmonitor(&row, &path, cancellation).await;
        match backend {
            BackendKind::GitWorktree => {
                timed(
                    trace,
                    "git-worktree-remove",
                    self.remove_git_worktree(&path, &main, force, cancellation),
                )
                .await?;
                // Git can exit 0 with the checkout gone and its metadata still
                // registered; that row would stay on the board as live.
                if let Err(error) = self
                    .prune_registration(&row, &main, &CancellationToken::new())
                    .await
                {
                    warnings.push(error.to_string());
                }
            }
            BackendKind::Rift => warnings.extend(
                timed(
                    trace,
                    "rift-remove",
                    self.remove_rift(&path, &main, force, cancellation),
                )
                .await?,
            ),
        }
        // The checkout removal is the irreversible commit point. Finish branch
        // cleanup and durable bookkeeping even if cancellation arrives now.
        Ok(self
            .finish_removal(&row, &main, backend, &options, destroyed_stage, warnings)
            .await)
    }

    /// Branch cleanup and durable bookkeeping after the checkout is gone.
    async fn finish_removal(
        &self,
        row: &WorktreeRecord,
        main: &Path,
        backend: BackendKind,
        options: &RemoveOptions,
        destroyed_stage: bool,
        mut warnings: Vec<String>,
    ) -> RemoveResult {
        let slug = row.target.slug();
        let durable_write = CancellationToken::new();
        let mut deleted_branch = false;
        if options.delete_branch
            && backend == BackendKind::GitWorktree
            && !row.target.branch.is_empty()
        {
            match self
                .delete_branch(main, &row.target.branch, &durable_write)
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
        let removed = options
            .removed_snapshot
            .clone()
            .unwrap_or_else(|| RemovedWorktree {
                slug: slug.to_owned(),
                branch: row.target.branch.clone(),
                removed_at: now_iso(),
                work: None,
                automations_paused: None,
                extra: Default::default(),
            });
        let removed = RemovedWorktree {
            removed_at: now_iso(),
            ..removed
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
        RemoveResult {
            removed: true,
            deleted_branch,
            destroyed_stage,
            warnings,
        }
    }

    /// Finish removing a worktree whose checkout is already gone (Git lists
    /// it as prunable). Nothing in the checkout can be lost, so this only
    /// stops what still runs for it, prunes Git's registration and records
    /// the removal. The branch can't be checked for unpushed commits without
    /// a checkout, so deleting it needs `force`.
    async fn finish_missing_checkout(
        &self,
        row: &WorktreeRecord,
        main: &Path,
        options: RemoveOptions,
        expected_revision: Option<&RemovalRevision>,
        lock: &FileLock,
        cancellation: &CancellationToken,
    ) -> Result<RemoveResult, LifecycleError> {
        if expected_revision.is_some_and(|expected| expected.digest != MISSING_CHECKOUT_DIGEST) {
            return Err(LifecycleError::Refused(
                "checkout changed after the removal warning; review the new state before removing"
                    .into(),
            ));
        }
        if options.delete_branch && !options.force {
            return Err(LifecycleError::Refused(format!(
                "{}; keep the branch, or force to delete it unchecked",
                MISSING_CHECKOUT_HAZARD
            )));
        }
        let trace = Path::new(&row.target.path);
        let mut warnings = Vec::new();
        if let Some(before_remove) = &self.before_remove {
            warnings.extend(before_remove(row.target.clone(), cancellation.clone()).await);
            if cancellation.is_cancelled() {
                return Err(LifecycleError::Cancelled);
            }
        }
        if let Some(dev) = &self.dev {
            timed(trace, "dev-server-stop", async {
                dev.stop_under_lifecycle_lock(&wt_dev::DevWorktree::from(row), lock, cancellation)
                    .await
                    .map_err(LifecycleError::from)
            })
            .await?;
        }
        // Both run inside the checkout; without it they can't run safely.
        if self.config.destroy_command.is_some() {
            warnings.push("skipped the destroy command: the checkout is already gone".into());
        }
        if options.destroy_stage {
            warnings.push("skipped sst remove: the checkout is already gone".into());
        }
        timed(
            trace,
            "prune-registration",
            self.prune_registration(row, main, cancellation),
        )
        .await?;
        Ok(self
            .finish_removal(
                row,
                main,
                BackendKind::GitWorktree,
                &options,
                false,
                warnings,
            )
            .await)
    }

    /// Make sure Git no longer registers `row`'s checkout, pruning metadata
    /// left behind for a checkout that no longer exists.
    async fn prune_registration(
        &self,
        row: &WorktreeRecord,
        main: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        if !self.still_registered(row, cancellation).await? {
            return Ok(());
        }
        tracing::warn!(target: "wt::removal", path = %row.target.path,
            "Git still registers a removed checkout; pruning its metadata");
        self.run_git(
            main,
            ["worktree", "prune"],
            cancellation,
            "prune Git worktree metadata",
        )
        .await?;
        if self.still_registered(row, cancellation).await? {
            return Err(LifecycleError::Refused(format!(
                "Git still registers {} after pruning; run `git worktree prune` in the main clone",
                row.target.path
            )));
        }
        Ok(())
    }

    async fn still_registered(
        &self,
        row: &WorktreeRecord,
        cancellation: &CancellationToken,
    ) -> Result<bool, LifecycleError> {
        Ok(self
            .repository
            .inventory(cancellation)
            .await?
            .iter()
            .any(|record| !record.is_main && record.target.path == row.target.path))
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

    async fn resolve_ref(
        &self,
        cwd: &Path,
        reference: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, LifecycleError> {
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
                "resolve Git ref",
            )
            .await?;
        match output.status.code() {
            Some(1) | Some(128) => Ok(None),
            Some(0) => {
                let resolved = output.stdout_text().trim().to_owned();
                if !valid_object_id(&resolved) {
                    return Err(LifecycleError::Refused(format!(
                        "Git ref {reference:?} resolved to an invalid object id"
                    )));
                }
                Ok(Some(resolved.to_ascii_lowercase()))
            }
            _ => Err(process_error(
                "resolve Git ref",
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
        let spec = git_spec(cwd, args);
        let phase = match operation {
            "refresh copied Git index" => Some("copied-index-refresh"),
            "switch Rift clone branch" | "create branch in Rift clone" => {
                Some("rift-branch-checkout")
            }
            "create Git worktree" => Some("git-worktree-checkout"),
            "fetch stacked base into Rift clone" => Some("rift-stacked-base-fetch"),
            _ => None,
        }
        .map(|phase| crate::creation_trace::CreationPhase::new(cwd, phase));
        let result = self.runner.run(spec, cancellation).await;
        if let Some(phase) = phase {
            phase.process(&result, operation == "refresh copied Git index");
        }
        result.map_err(|source| process_error(operation, cwd, source))
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
        wt_core::stage_name(slug, &self.config.stage_prefix, self.id_pattern.as_ref())
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
                let rift = self.rift_binary(cancellation).await?;
                if !fs::try_exists(main.join(".rift")).await.map_err(|source| {
                    LifecycleError::Io {
                        operation: "check Rift registration",
                        path: main.join(".rift"),
                        source,
                    }
                })? && let Err(error) = self
                    .run_external(
                        &rift,
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
                            [
                                "switch",
                                "--discard-changes",
                                "--no-track",
                                "-c",
                                branch,
                                "FETCH_HEAD",
                            ],
                            cancellation,
                            "create branch in Rift clone",
                        )
                        .await?
                    }
                    Some(base) => {
                        self.run_git(
                            path,
                            [
                                "switch",
                                "--discard-changes",
                                "--no-track",
                                "-c",
                                branch,
                                base,
                            ],
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
                .map(|base| logical_base_branch(base).to_owned())
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
        let globs = crate::copy_globs::CopyGlobs::new(&self.config.copy_globs)
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
                    let relative = path
                        .strip_prefix(source)
                        .expect("walk remains under source");
                    if globs.can_match_descendant(relative) {
                        pending.push(path);
                    }
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
        let phase = crate::creation_trace::CreationPhase::new(path, "dependency-install");
        let result = self
            .runner
            .run_streaming(spec, cancellation, |_, _| {})
            .await;
        phase.process(&result, false);
        let output = result.map_err(|source| process_error("run package install", path, source))?;
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

    /// Best-effort stop of the checkout's `git fsmonitor--daemon`. It is a
    /// long-lived writer of cookie files under the Git dir and survives the
    /// removal's listener reap. Only contacts a daemon whose IPC exists.
    async fn stop_fsmonitor(
        &self,
        row: &WorktreeRecord,
        path: &Path,
        cancellation: &CancellationToken,
    ) {
        let git_dir = row.git_dir.clone().unwrap_or_else(|| path.join(".git"));
        let ipc = git_dir.join("fsmonitor--daemon.ipc");
        if !matches!(fs::try_exists(&ipc).await, Ok(true)) {
            return;
        }
        let phase = RemovalPhase::new(path, "fsmonitor-stop");
        let mut spec = git_spec(path, args(["fsmonitor--daemon", "stop"]));
        spec.timeout = Duration::from_secs(10);
        match self.runner.run(spec, cancellation).await {
            Ok(output) if output.status.success() => phase.finish("success"),
            Ok(output) => {
                phase.finish("error");
                tracing::debug!(target: "wt::removal", path = %path.display(),
                    exit_code = output.status.code(), "fsmonitor daemon stop did not succeed");
            }
            Err(error) => {
                phase.finish("error");
                tracing::warn!(target: "wt::removal", path = %path.display(), %error,
                    "could not stop fsmonitor daemon before removal");
            }
        }
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
        let mut args = vec![OsString::from("worktree"), OsString::from("remove")];
        if force {
            args.push(OsString::from("--force"));
        }
        args.push(path.as_os_str().to_owned());
        // Git unlinks files during this command. A timeout or UI cancellation
        // after spawn cannot safely roll that back. Keep its owner and lock
        // until it exits; ordinary Git reads retain their short deadlines.
        let result = self
            .runner
            .run_to_completion(git_spec(main, args), cancellation)
            .await
            .map_err(|source| LifecycleError::IncompleteRemoval {
                path: path.to_owned(),
                source,
            })?;
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
            .await;
        if matches!(fs::try_exists(path).await, Ok(false))
            && prune.as_ref().is_ok_and(|output| output.status.success())
        {
            return Ok(());
        }
        if let Err(error) = prune {
            tracing::warn!(%error, "Git metadata prune after incomplete removal failed");
        }
        Err(LifecycleError::IncompleteRemoval {
            path: path.to_owned(),
            source: result.checked("git").unwrap_err(),
        })
    }

    async fn rift_binary(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<OsString, LifecycleError> {
        self.resolved_rift
            .get_or_try_init(|| async {
                crate::rift_binary::resolve(
                    &self.config.rift_binary,
                    &self.config.shell,
                    &self.config.main_clone,
                    &std::env::var_os("PATH").unwrap_or_default(),
                    &self.runner,
                    cancellation,
                )
                .await
            })
            .await
            .cloned()
    }

    async fn remove_rift(
        &self,
        path: &Path,
        main: &Path,
        force: bool,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, LifecycleError> {
        // A leftover quarantine is past the commit point: the removal
        // succeeded and its path is a warning, not a failure to retry.
        if self.config.backend == BackendKind::GitWorktree {
            return self.remove_retired_clone(path, cancellation).await;
        }
        let mut spec = CommandSpec::new(self.rift_binary(cancellation).await?);
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

    /// Retiring Rift must not make previously created independent checkouts
    /// undeletable. The caller holds the same lock and removal witnesses as the
    /// Rift executable path. Never interpret a linked worktree as a clone.
    ///
    /// The verified checkout is first renamed to a quarantine sibling (one
    /// atomic step on the same filesystem); that rename is the commit point.
    /// Deleting the quarantine retries only `Directory not empty`, which means
    /// some process created entries after `rm` listed a directory. Returns a
    /// warning naming the quarantine when it could not be deleted.
    async fn remove_retired_clone(
        &self,
        path: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, LifecycleError> {
        let root = canonicalize(&self.config.worktree_root, "resolve worktree root").await?;
        self.ensure_managed_path(path, &root)?;
        let mut identity = None;
        for (candidate, directory) in [
            (path.to_path_buf(), true),
            (path.join(".git"), true),
            (path.join(".rift"), false),
        ] {
            let metadata =
                fs::symlink_metadata(&candidate)
                    .await
                    .map_err(|source| LifecycleError::Io {
                        operation: "validate retired independent clone",
                        path: candidate.clone(),
                        source,
                    })?;
            if metadata.file_type().is_symlink()
                || (directory && !metadata.is_dir())
                || (!directory && !metadata.is_file())
            {
                return Err(LifecycleError::Refused(format!(
                    "retired clone identity changed: {}",
                    candidate.display()
                )));
            }
            identity.get_or_insert_with(|| directory_identity(&metadata));
        }
        if cancellation.is_cancelled() {
            return Err(LifecycleError::Cancelled);
        }
        let identity = identity.expect("checkout metadata was read");
        let quarantine = timed(
            path,
            "quarantine-rename",
            quarantine_checkout(path, identity),
        )
        .await?;
        tracing::info!(target: "wt::removal", path = %path.display(), quarantine = %quarantine.display(),
            "retired independent clone moved aside for deletion");
        let phase = RemovalPhase::new(path, "quarantine-delete");
        for attempt in 1..=QUARANTINE_DELETE_ATTEMPTS {
            let mut spec = quarantine_cleanup(
                &OsString::from("rm"),
                vec!["-rf".into(), "--".into(), quarantine.as_os_str().to_owned()],
            );
            spec.cwd = Some(root.clone());
            // Once recursive deletion starts, retain ownership until the process
            // exits. Cancelling halfway cannot restore an independent object store.
            let result = self
                .runner
                .run_to_completion(spec, cancellation)
                .await
                .and_then(|output| output.checked("rm").map(|_| ()));
            let source = match result {
                Ok(()) => {
                    phase.finish("success");
                    tracing::info!(target: "wt::lifecycle", path = %path.display(), attempts = attempt,
                        "retired independent clone removed; Rift registry unchanged");
                    return Ok(None);
                }
                Err(source) => source,
            };
            // Retry only a pure ENOTEMPTY race. Any other error (permissions,
            // I/O) also leaves parents non-empty but will not clear itself.
            let not_empty = matches!(&source, ProcessError::Exit { stderr, .. }
                if stderr.lines().any(|line| !line.trim().is_empty())
                    && stderr.lines().filter(|line| !line.trim().is_empty())
                        .all(|line| line.contains("Directory not empty")));
            let retry =
                not_empty && attempt < QUARANTINE_DELETE_ATTEMPTS && !cancellation.is_cancelled();
            if !retry {
                phase.finish("error");
                return Ok(Some(format!(
                    "{} was removed, but deleting the directory it was moved to failed; delete {} manually: {source}",
                    path.display(),
                    quarantine.display()
                )));
            }
            tracing::warn!(target: "wt::removal", quarantine = %quarantine.display(), attempt,
                "entries appeared during quarantine deletion; retrying");
            tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt))).await;
        }
        unreachable!("the last attempt always returns")
    }

    async fn run_rift_create(
        &self,
        main: &Path,
        path: &Path,
        slug: &str,
        cancellation: &CancellationToken,
    ) -> Result<wt_platform::process::ProcessOutput, LifecycleError> {
        let mut spec = CommandSpec::new(self.rift_binary(cancellation).await?);
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
        // Rift owns its internal postcreate hooks, so this measures its whole
        // command, not just the kernel clonefile call.
        let phase = crate::creation_trace::CreationPhase::new(path, "rift-create-including-hooks");
        let result = self.runner.run(spec, cancellation).await;
        phase.process(&result, false);
        result.map_err(|source| process_error("create Rift clone", main, source))
    }

    async fn run_rift_gc(
        &self,
        main: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), LifecycleError> {
        let mut spec = prioritized_rift_gc(&self.rift_binary(cancellation).await?);
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
        let requested = Path::new(&target.path);
        let expected = match fs::canonicalize(requested).await {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => requested.to_path_buf(),
            Err(source) => {
                return Err(LifecycleError::Io {
                    operation: "resolve requested target",
                    path: requested.to_path_buf(),
                    source,
                });
            }
        };
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
        self.capture_revision_for_row_excluding(row, &[], cancellation)
            .await
    }

    async fn capture_revision_for_row_excluding(
        &self,
        row: &WorktreeRecord,
        excluded_paths: &[String],
        cancellation: &CancellationToken,
    ) -> Result<RemovalRevision, LifecycleError> {
        let path = Path::new(&row.target.path);
        let exclusions = exclusion_pathspecs(excluded_paths)?;
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
        let mut status_args = args([
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--",
        ]);
        status_args.push(".".into());
        status_args.extend(exclusions.iter().cloned());
        let status = self
            .git_snapshot_digest(path, status_args, cancellation)
            .await?;
        let mut worktree_args = args(["diff", "--binary", "--no-ext-diff", "--no-textconv", "--"]);
        worktree_args.push(".".into());
        worktree_args.extend(exclusions.iter().cloned());
        let worktree_diff = self
            .git_snapshot_digest(path, worktree_args, cancellation)
            .await?;
        let mut index_args = args([
            "diff",
            "--cached",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            "--",
        ]);
        index_args.push(".".into());
        index_args.extend(exclusions.iter().cloned());
        let index_diff = self
            .git_snapshot_digest(path, index_args, cancellation)
            .await?;
        let mut untracked_args = args(["ls-files", "--others", "--exclude-standard", "-z", "--"]);
        untracked_args.push(".".into());
        untracked_args.extend(exclusions);
        let untracked = self
            .git_snapshot_output_vec(path, untracked_args, cancellation)
            .await?
            .stdout;
        let checkout = PathBuf::from(&row.target.path);
        let snapshot_cancellation = cancellation.clone();
        let metadata = tokio::task::spawn_blocking(move || {
            untracked_metadata(&checkout, &untracked, &snapshot_cancellation)
        })
        .await
        .map_err(|error| LifecycleError::Join(error.to_string()))?
        .map_err(|error| match error {
            UntrackedSnapshotError::Changed(message) => LifecycleError::CheckoutChanging(format!(
                "cannot safely snapshot checkout: {message}"
            )),
            UntrackedSnapshotError::Unsafe(message) => {
                LifecycleError::Refused(format!("cannot safely snapshot checkout: {message}"))
            }
        })?;
        let mut digest = Sha256::new();
        for bytes in [
            status.as_slice(),
            worktree_diff.as_slice(),
            index_diff.as_slice(),
            &metadata,
        ] {
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
            published_base: None,
        })
    }

    async fn rebase_revision_after_sst(
        &self,
        row: &WorktreeRecord,
        landed: bool,
        expected: &RemovalRevision,
        before_sst: &RemovalRevision,
        before_outside: &RemovalRevision,
        cancellation: &CancellationToken,
    ) -> Result<RemovalRevision, LifecycleError> {
        let allowed = &self.config.auto_regen_paths;
        if before_sst.key != before_outside.key
            || before_sst.path != before_outside.path
            || before_sst.branch != before_outside.branch
            || before_sst.head != before_outside.head
        {
            return Err(LifecycleError::Refused(
                "checkout identity changed immediately before SST removal; review the new state before removing".into(),
            ));
        }

        let after_outside = self
            .capture_revision_for_row_excluding(row, allowed, cancellation)
            .await?;
        if !same_checkout_revision(&after_outside, before_outside) {
            return Err(LifecycleError::Refused(
                "SST removal changed files outside the configured generated paths; review the checkout before removing".into(),
            ));
        }
        let hazards = self
            .collect_removal_hazards(row, landed, cancellation)
            .await?;
        if removal_hazards_without_checkout_dirty(&hazards)
            != removal_hazards_without_checkout_dirty(&expected.hazards)
        {
            return Err(LifecycleError::Refused(
                "removal hazards changed during SST removal; review the new state before removing"
                    .into(),
            ));
        }

        let full = self.capture_revision_for_row(row, cancellation).await?;
        let after_full_outside = self
            .capture_revision_for_row_excluding(row, allowed, cancellation)
            .await?;
        if !same_checkout_revision(&after_full_outside, &after_outside) {
            return Err(LifecycleError::Refused(
                "checkout changed while verifying SST removal; review the new state before removing".into(),
            ));
        }
        let mut rebased = full;
        rebased.hazards = hazards;
        rebased.published_base = expected.published_base.clone();
        Ok(rebased)
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
        if let Some((reference, tip)) = expected.published_base.as_deref() {
            if !reference.starts_with("refs/heads/")
                || !matches!(tip.len(), 40 | 64)
                || !tip.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(LifecycleError::Refused(
                    "invalid published-base removal proof".into(),
                ));
            }
            // Linked worktrees can override remote.origin through worktree
            // config. Revalidate against the same object store and origin that
            // supplied the plan; Rift clones own their independent origin.
            let proof_repository = match self.backend_for(Path::new(&row.target.path)).await? {
                BackendKind::GitWorktree => self.config.main_clone.as_path(),
                BackendKind::Rift => Path::new(&row.target.path),
            };
            let advertised = self
                .run_git_raw(
                    proof_repository,
                    ["ls-remote", "--exit-code", "--refs", "origin", reference],
                    cancellation,
                    "revalidate published removal base",
                )
                .await?;
            let still_published = advertised.status.success()
                && advertised.stdout_text().lines().any(|line| {
                    line.split_once('\t')
                        .is_some_and(|(oid, name)| oid == tip && name == reference)
                });
            if !still_published {
                return Err(LifecycleError::Refused(
                    "published base changed or is unavailable after removal planning; review current landing evidence before removing".into(),
                ));
            }
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
        self.git_snapshot_output_vec(
            path,
            args.into_iter().map(Into::into).collect(),
            cancellation,
        )
        .await
    }

    /// Fingerprint the entire stream, rather than retaining a patch that can
    /// grow with arbitrary tracked file contents. The runner still owns the
    /// timeout, cancellation, pipe draining and bounded error diagnostics.
    async fn git_snapshot_digest(
        &self,
        path: &Path,
        args: Vec<OsString>,
        cancellation: &CancellationToken,
    ) -> Result<[u8; 32], LifecycleError> {
        let digest = Arc::new(std::sync::Mutex::new(Sha256::new()));
        let observer_digest = digest.clone();
        let mut command = CommandSpec::new("git");
        command.args = args;
        command.cwd = Some(path.to_path_buf());
        command.timeout = Duration::from_secs(30);
        command.output_limit = 8192;
        self.runner
            .run_streaming(command, cancellation, move |stream, bytes| {
                if stream == ProcessStream::Stdout {
                    observer_digest
                        .lock()
                        .expect("digest lock poisoned")
                        .update(bytes);
                }
            })
            .await
            .and_then(|output| output.checked("git"))
            .map_err(|source| LifecycleError::Process {
                operation: "capture removal revision",
                path: path.to_path_buf(),
                source,
            })?;
        Ok(digest
            .lock()
            .expect("digest lock poisoned")
            .clone()
            .finalize()
            .into())
    }

    async fn git_snapshot_output_vec(
        &self,
        path: &Path,
        args: Vec<OsString>,
        cancellation: &CancellationToken,
    ) -> Result<wt_platform::process::ProcessOutput, LifecycleError> {
        let mut command = CommandSpec::new("git");
        command.args = args;
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
            hazards.push(UNCOMMITTED_HAZARD.to_owned());
        }
        // Commits a stopped rebase already replayed exist only on its
        // detached HEAD, which removal would discard.
        if wt_vcs::operation_in_progress(row.git_dir.as_deref()).await {
            hazards.push(IN_PROGRESS_HAZARD.to_owned());
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
        // Landing evidence covers what was pushed and merged. Commits beyond
        // the branch's own origin ref exist only here, landed or not, so a
        // planning-time `landed` claim never authorizes discarding them.
        let unpushed = if landed {
            self.commits_beyond_origin_branch(row, cancellation).await?
        } else {
            self.unpushed_count(row, state.as_ref(), cancellation)
                .await?
        };
        match unpushed {
            Some(0) => {}
            Some(count) => hazards.push(unpushed_hazard(count)),
            None => hazards.push("could not verify pushed state".to_owned()),
        }
        Ok(hazards)
    }

    /// Count commits on HEAD that the branch's origin ref lacks. A branch
    /// with no origin ref (deleted after merge, or local-only) relies on the
    /// caller's landing proof and counts as zero here.
    async fn commits_beyond_origin_branch(
        &self,
        row: &WorktreeRecord,
        cancellation: &CancellationToken,
    ) -> Result<Option<u32>, LifecycleError> {
        let path = Path::new(&row.target.path);
        let origin_branch = format!("refs/remotes/origin/{}", row.target.branch);
        if row.target.branch.is_empty()
            || !self.ref_exists(path, &origin_branch, cancellation).await?
        {
            return Ok(Some(0));
        }
        self.count_commits_since(path, &origin_branch, cancellation)
            .await
    }

    async fn count_commits_since(
        &self,
        path: &Path,
        base: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<u32>, LifecycleError> {
        let count = self
            .run_git_raw(
                path,
                [
                    "rev-list",
                    "--count",
                    "--end-of-options",
                    &format!("{base}..HEAD"),
                ],
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
        // Fully qualified refs: a local branch named `origin/<x>` must not
        // stand in for the remote-tracking ref.
        let base = if self.ref_exists(path, &origin_branch, cancellation).await? {
            origin_branch
        } else {
            let recorded_sha = state.and_then(|v| v.get("baseSha")).and_then(Value::as_str);
            let recorded_branch = state
                .and_then(|v| v.get("baseBranch"))
                .and_then(Value::as_str);
            if let Some(sha) = recorded_sha {
                sha.to_owned()
            } else if let Some(branch) = recorded_branch {
                format!("refs/remotes/origin/{branch}")
            } else {
                let remote_base = format!("refs/remotes/origin/{}", self.config.base_branch);
                let local_base = format!("refs/heads/{}", self.config.base_branch);
                if self.ref_exists(path, &remote_base, cancellation).await? {
                    remote_base
                } else if self.ref_exists(path, &local_base, cancellation).await? {
                    local_base
                } else {
                    return Ok(None);
                }
            }
        };
        self.count_commits_since(path, &base, cancellation).await
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

fn git_spec(cwd: &Path, args: Vec<OsString>) -> CommandSpec {
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
    spec
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

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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

/// Git still registers this linked worktree, but its checkout is gone.
pub async fn checkout_missing(row: &WorktreeRecord) -> bool {
    !row.is_main
        && row.kind == wt_vcs::RepositoryKind::LinkedWorktree
        && row.prunable
        && matches!(fs::try_exists(&row.target.path).await, Ok(false))
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

fn same_checkout_revision(current: &RemovalRevision, expected: &RemovalRevision) -> bool {
    current.key == expected.key
        && current.path == expected.path
        && current.branch == expected.branch
        && current.head == expected.head
        && current.digest == expected.digest
}

fn args<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(Into::into).collect()
}

fn exclusion_pathspecs(paths: &[String]) -> Result<Vec<OsString>, LifecycleError> {
    let mut result = Vec::with_capacity(paths.len());
    for path in paths {
        let relative = Path::new(path);
        if path.is_empty()
            || path == "."
            || relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(LifecycleError::Refused(format!(
                "configured SST generated path is not safely relative: {path:?}"
            )));
        }
        result.push(OsString::from(format!(":(exclude,literal){path}")));
    }
    Ok(result)
}

fn removal_hazards_without_checkout_dirty(hazards: &[String]) -> Vec<&str> {
    hazards
        .iter()
        .map(String::as_str)
        .filter(|hazard| *hazard != UNCOMMITTED_HAZARD)
        .collect()
}

#[derive(Debug)]
enum UntrackedSnapshotError {
    /// A file vanished or changed size mid-read.
    Changed(String),
    Unsafe(String),
}

impl From<String> for UntrackedSnapshotError {
    fn from(message: String) -> Self {
        Self::Unsafe(message)
    }
}

fn untracked_metadata(
    checkout: &Path,
    paths: &[u8],
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, UntrackedSnapshotError> {
    use std::io::Read;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    let mut entries = paths
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    if entries.len() > MAX_UNTRACKED_FILES {
        return Err(UntrackedSnapshotError::Unsafe(format!(
            "more than {MAX_UNTRACKED_FILES} untracked paths"
        )));
    }
    entries.sort();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let check_budget = || {
        if cancellation.is_cancelled() {
            Err("untracked snapshot cancelled".to_owned())
        } else if std::time::Instant::now() >= deadline {
            Err("untracked snapshot timed out after 30 seconds".to_owned())
        } else {
            Ok(())
        }
    };
    let mut result = Vec::new();
    for relative in entries {
        check_budget()?;
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
            return Err(UntrackedSnapshotError::Unsafe(
                "Git reported an untracked path outside the checkout".into(),
            ));
        }
        let full_path = checkout.join(relative_path);
        let metadata = std::fs::symlink_metadata(&full_path).map_err(|error| {
            let message = format!("stat {}: {error}", full_path.display());
            if error.kind() == std::io::ErrorKind::NotFound {
                UntrackedSnapshotError::Changed(message)
            } else {
                UntrackedSnapshotError::Unsafe(message)
            }
        })?;
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
            let mut file = std::fs::File::open(&full_path)
                .map_err(|error| format!("read {}: {error}", full_path.display()))?;
            let mut digest = Sha256::new();
            let mut buffer = [0; 64 * 1024];
            let mut total = 0u64;
            loop {
                check_budget()?;
                let read = file
                    .read(&mut buffer)
                    .map_err(|error| format!("read {}: {error}", full_path.display()))?;
                if read == 0 {
                    break;
                }
                total += read as u64;
                digest.update(&buffer[..read]);
            }
            if total != metadata.len() {
                return Err(UntrackedSnapshotError::Changed(format!(
                    "{} changed while its removal snapshot was captured",
                    full_path.display()
                )));
            }
            let hash = digest.finalize();
            result.extend_from_slice(&hash);
        } else {
            return Err(UntrackedSnapshotError::Unsafe(format!(
                "unsupported untracked file type: {}",
                full_path.display()
            )));
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

/// The branch name a base ref denotes, as recorded in `baseBranch` and
/// `gh-merge-base`. `baseBranch == trunk` means "no parent", so every spelling
/// of the trunk must normalize to the same name.
fn logical_base_branch(base: &str) -> &str {
    ["refs/remotes/origin/", "origin/", "refs/heads/"]
        .into_iter()
        .find_map(|prefix| base.strip_prefix(prefix))
        .unwrap_or(base)
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
    prioritized_cleanup(rift_binary, vec![OsString::from("gc")])
}

fn prioritized_cleanup(binary: &OsString, args: Vec<OsString>) -> CommandSpec {
    cleanup_with_policy(binary, args, &["-b"])
}

/// Deletion of a checkout already moved aside. The user waits on this job,
/// so it runs at utility QoS: below interactive work, but without the
/// background clamp, whose disk I/O throttling stretched large deletions
/// to minutes. CPU priority stays lowered with `nice`.
fn quarantine_cleanup(binary: &OsString, args: Vec<OsString>) -> CommandSpec {
    cleanup_with_policy(binary, args, &["-c", "utility"])
}

/// `nice -n 10`, plus the given macOS `taskpolicy` arguments when available.
fn cleanup_with_policy(
    binary: &OsString,
    mut args: Vec<OsString>,
    taskpolicy_args: &[&str],
) -> CommandSpec {
    let mut program = PathBuf::from(binary);
    if let Some(nice) = find_executable("nice") {
        args.splice(
            0..0,
            [OsString::from("-n"), OsString::from("10"), binary.clone()],
        );
        program = nice;
    }
    let mut spec = if cfg!(target_os = "macos") {
        find_executable("taskpolicy").map(|taskpolicy| {
            let mut spec = CommandSpec::new(taskpolicy);
            spec.args = taskpolicy_args
                .iter()
                .map(OsString::from)
                .chain([program.clone().into_os_string()])
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
fn directory_identity(metadata: &std::fs::Metadata) -> DirectoryIdentity {
    use std::os::unix::fs::MetadataExt;
    DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(not(unix))]
fn directory_identity(_metadata: &std::fs::Metadata) -> DirectoryIdentity {
    DirectoryIdentity {
        device: 0,
        inode: 0,
    }
}

/// Move a verified checkout to a fresh `.wt-removing-*` sibling in the same
/// directory, so the rename is atomic and stays on one filesystem. Nothing is
/// deleted here. If the moved directory is not the one that was verified, it
/// is moved back and the removal is refused.
async fn quarantine_checkout(
    path: &Path,
    expected: DirectoryIdentity,
) -> Result<PathBuf, LifecycleError> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(LifecycleError::Invalid(format!(
            "checkout has no parent directory: {}",
            path.display()
        )));
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut quarantine_name = OsString::from(GitRepository::REMOVAL_QUARANTINE_PREFIX);
    quarantine_name.push(name);
    quarantine_name.push(format!("-{}-{nanos}", std::process::id()));
    let quarantine = parent.join(quarantine_name);
    match fs::symlink_metadata(&quarantine).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(LifecycleError::Refused(format!(
                "removal quarantine already exists: {}",
                quarantine.display()
            )));
        }
        Err(source) => {
            return Err(LifecycleError::Io {
                operation: "inspect removal quarantine",
                path: quarantine,
                source,
            });
        }
    }
    let current = fs::symlink_metadata(path)
        .await
        .map_err(|source| LifecycleError::Io {
            operation: "revalidate checkout before quarantine",
            path: path.to_owned(),
            source,
        })?;
    if current.file_type().is_symlink()
        || !current.is_dir()
        || directory_identity(&current) != expected
    {
        return Err(LifecycleError::Refused(format!(
            "checkout identity changed before removal: {}",
            path.display()
        )));
    }
    fs::rename(path, &quarantine)
        .await
        .map_err(|source| LifecycleError::Io {
            operation: "move checkout aside for removal (nothing was deleted)",
            path: path.to_owned(),
            source,
        })?;
    let moved = fs::symlink_metadata(&quarantine).await;
    if moved
        .as_ref()
        .is_ok_and(|metadata| directory_identity(metadata) == expected && metadata.is_dir())
    {
        return Ok(quarantine);
    }
    // Something replaced the checkout between the check and the rename.
    // Put it back untouched and refuse; nothing was deleted.
    if let Err(error) = fs::rename(&quarantine, path).await {
        tracing::error!(target: "wt::removal", %error, quarantine = %quarantine.display(),
            "could not restore an unexpected directory moved aside for removal");
        return Err(LifecycleError::Refused(format!(
            "checkout identity changed during removal; the unexpected directory is at {}",
            quarantine.display()
        )));
    }
    Err(LifecycleError::Refused(format!(
        "checkout identity changed during removal: {}",
        path.display()
    )))
}

/// Removal hazard for a dirty worktree.
pub const UNCOMMITTED_HAZARD: &str = "uncommitted changes";
pub const IN_PROGRESS_HAZARD: &str = "a rebase, merge or cherry-pick is in progress";
/// Removal hazard for a registration whose checkout is already gone: its
/// branch could not be checked for unpushed commits.
pub const MISSING_CHECKOUT_HAZARD: &str =
    "the checkout is already gone, so its branch was not checked for unpushed commits";
/// Revision digest that witnesses a plan made against a missing checkout.
const MISSING_CHECKOUT_DIGEST: &str = "missing-checkout";

/// Removal hazard for commits that exist only locally.
pub fn unpushed_hazard(count: u32) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} unpushed commit{plural}")
}

/// Whether a removal hazard means work that removal destroys, as opposed to
/// an obligation or an unknown that the confirm states on its own line.
pub fn is_lost_work_hazard(hazard: &str) -> bool {
    hazard == UNCOMMITTED_HAZARD
        || hazard == IN_PROGRESS_HAZARD
        || hazard == MISSING_CHECKOUT_HAZARD
        || hazard.split_once(' ').is_some_and(|(count, rest)| {
            count.parse::<u64>().is_ok()
                && (rest == "unpushed commit" || rest == "unpushed commits")
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_trunk_spelling_records_the_same_base_branch() {
        use super::logical_base_branch;
        for base in [
            "main",
            "origin/main",
            "refs/heads/main",
            "refs/remotes/origin/main",
        ] {
            assert_eq!(logical_base_branch(base), "main", "{base}");
        }
        assert_eq!(logical_base_branch("team/parent"), "team/parent");
    }

    #[test]
    fn lost_work_hazards_are_recognized_by_their_own_wording() {
        use super::{UNCOMMITTED_HAZARD, is_lost_work_hazard, unpushed_hazard};
        assert_eq!(unpushed_hazard(1), "1 unpushed commit");
        assert_eq!(unpushed_hazard(3), "3 unpushed commits");
        assert!(is_lost_work_hazard(UNCOMMITTED_HAZARD));
        assert!(is_lost_work_hazard(super::IN_PROGRESS_HAZARD));
        assert!(is_lost_work_hazard(&unpushed_hazard(1)));
        assert!(is_lost_work_hazard(&unpushed_hazard(12)));
        assert!(!is_lost_work_hazard("post-merge verification still owed"));
        assert!(!is_lost_work_hazard("could not verify pushed state"));
    }

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

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_git_removal_reports_possible_partial_loss_without_forcing_a_retry() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (_scratch, service, runner, main, root) = setup().await;
        let path = root.join("partial");
        run(
            &runner,
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "michael/partial",
                path.to_str().unwrap(),
            ],
        )
        .await;
        let protected = path.join("protected");
        fs::create_dir(&protected).await.unwrap();
        fs::write(protected.join("keep.txt"), "fixture only")
            .await
            .unwrap();
        fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o555))
            .await
            .unwrap();
        let result = service
            .remove_git_worktree(&path, &main, true, &CancellationToken::new())
            .await;
        // Restore fixture permissions even when an assertion below fails.
        fs::set_permissions(&protected, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();
        let error = result.unwrap_err();
        assert!(matches!(error, LifecycleError::IncompleteRemoval { .. }));
        let message = error.to_string();
        assert!(message.contains("files may already be missing"));
        assert!(message.contains("inspect the remaining checkout before retrying"));
        assert!(protected.join("keep.txt").exists());
        assert!(path.exists());
    }

    #[tokio::test]
    async fn configured_copy_prunes_unrelated_trees_and_preserves_existing_files() {
        let (_scratch, mut service, _runner, main, root) = setup().await;
        service.config.copy_globs = vec![".env*".into(), "config/*.json".into()];
        for (relative, contents) in [
            (".env.local", "source"),
            ("config/deep/nested.json", "nested"),
            ("config/skip.txt", "skip"),
            ("node_modules/pkg/config.json", "unrelated"),
            ("config/.git/secret.json", "git metadata"),
        ] {
            let path = main.join(relative);
            fs::create_dir_all(path.parent().unwrap()).await.unwrap();
            fs::write(path, contents).await.unwrap();
        }
        fs::write(root.join(".env.local"), "destination")
            .await
            .unwrap();
        // An unreadable, provably unrelated subtree must not be visited at all.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                main.join("node_modules"),
                std::fs::Permissions::from_mode(0o0),
            )
            .await
            .unwrap();
            std::os::unix::fs::symlink(main.join("config"), main.join("config/link")).unwrap();
        }
        let result = service
            .copy_files(&main, &root, &CancellationToken::new())
            .await;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                main.join("node_modules"),
                std::fs::Permissions::from_mode(0o700),
            )
            .await
            .unwrap();
        }
        result.unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".env.local")).await.unwrap(),
            "destination"
        );
        assert_eq!(
            fs::read_to_string(root.join("config/deep/nested.json"))
                .await
                .unwrap(),
            "nested"
        );
        for absent in [
            "node_modules",
            "config/skip.txt",
            "config/.git",
            "config/link",
        ] {
            assert!(!root.join(absent).exists(), "{absent}");
        }
    }

    #[tokio::test]
    async fn create_and_remove_linked_worktree_preserves_fork_anchor() {
        let (_scratch, service, runner, main, root) = setup().await;
        let cleanup_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = cleanup_ran.clone();
        let service = service.with_before_remove(move |target, _cancellation| {
            let observed = observed.clone();
            async move {
                assert!(Path::new(&target.path).exists());
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                vec!["host resource cleaned".into()]
            }
        });
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

        service
            .mutate_store(&cancellation, |store| {
                store.set_slug_title(
                    "ENG-42-feature",
                    "Latest manual title",
                    wt_store::TitleSource::User,
                    None,
                )?;
                store.set_slug_issue_id("ENG-42-feature", Some("COZ-42"))?;
                store.set_slug_github_issue("ENG-42-feature", Some(42))?;
                store.set_slug_work_status(
                    "ENG-42-feature",
                    Some(&wt_store::WorkStatusRecord {
                        state: "ready".into(),
                        at: "2026-10-09T00:00:00Z".into(),
                        note: Some("latest note".into()),
                        risk: Some("low".into()),
                        sha: None,
                        by: None,
                        blocked_on: None,
                        verify_after_merge: Some("verify prod".into()),
                        extra: serde_json::Map::new(),
                    }),
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let snapshot = RemovedWorktree {
            slug: "ENG-42-feature".into(),
            branch: "michael/ENG-42-feature".into(),
            removed_at: String::new(),
            work: None,
            automations_paused: None,
            extra: serde_json::Map::from_iter([
                ("title".into(), serde_json::json!("stale title")),
                ("issueId".into(), serde_json::json!("OLD-1")),
                ("githubIssue".into(), serde_json::json!(1)),
                ("prNumber".into(), serde_json::json!(84)),
                (
                    "prUrl".into(),
                    serde_json::json!("https://example.invalid/pr/84"),
                ),
            ]),
        };

        let removed = service
            .remove(
                &created.target,
                RemoveOptions {
                    force: true,
                    delete_branch: true,
                    removed_snapshot: Some(snapshot),
                    ..RemoveOptions::default()
                },
                &cancellation,
            )
            .await
            .unwrap();
        assert!(removed.removed && removed.deleted_branch);
        assert!(cleanup_ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            removed
                .warnings
                .iter()
                .any(|warning| warning == "host resource cleaned")
        );
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
        let history = service
            .mutate_store(&cancellation, |store| store.read_removed_worktrees())
            .await
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].extra["title"], "Latest manual title");
        assert_eq!(history[0].extra["issueId"], "COZ-42");
        assert_eq!(history[0].extra["githubIssue"], 42);
        assert_eq!(history[0].extra["prNumber"], 84);
        assert_eq!(
            history[0].work.as_ref().unwrap().note.as_deref(),
            Some("latest note")
        );

        let restored = service
            .create_from_removed(
                "ENG-42-feature",
                "michael/ENG-42-feature",
                &history[0].removed_at,
                CreateOptions {
                    base: Some("refs/heads/main".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        assert!(Path::new(&restored.target.path).exists());
        assert!(
            service
                .mutate_store(&cancellation, |store| store.read_removed_worktrees())
                .await
                .unwrap()
                .is_empty()
        );
        service
            .remove(
                &restored.target,
                RemoveOptions {
                    force: true,
                    delete_branch: true,
                    ..RemoveOptions::default()
                },
                &cancellation,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_review_worktree_fetches_and_pins_verified_pr_head_anchor() {
        let (_scratch, service, runner, main, root) = setup().await;
        let cancellation = CancellationToken::new();
        let remote = root.parent().unwrap().join("review-origin.git");
        fs::create_dir_all(&remote).await.unwrap();
        run(&runner, &remote, &["init", "--bare"]).await;
        run(
            &runner,
            &main,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .await;
        run(&runner, &main, &["push", "origin", "main"]).await;
        run(&runner, &main, &["checkout", "-b", "contributor/review-42"]).await;
        fs::write(main.join("review.txt"), "reviewed head\n")
            .await
            .unwrap();
        run(&runner, &main, &["add", "review.txt"]).await;
        run(&runner, &main, &["commit", "-m", "review head"]).await;
        run(
            &runner,
            &main,
            &["push", "origin", "HEAD:refs/pull/42/head"],
        )
        .await;
        let expected_head = String::from_utf8(run(&runner, &main, &["rev-parse", "HEAD"]).await)
            .unwrap()
            .trim()
            .to_owned();
        run(&runner, &main, &["checkout", "main"]).await;
        run(&runner, &main, &["branch", "-D", "contributor/review-42"]).await;

        let created = service
            .create_from_pull_request(
                "contributor/review-42",
                42,
                &expected_head,
                CreateOptions {
                    fetch_origin: false,
                    run_install: false,
                    ..CreateOptions::default()
                },
                &cancellation,
            )
            .await
            .unwrap();
        let checked_out = String::from_utf8(
            run(
                &runner,
                Path::new(&created.target.path),
                &["rev-parse", "HEAD"],
            )
            .await,
        )
        .unwrap();
        assert_eq!(checked_out.trim(), expected_head);

        let anchor = String::from_utf8(
            run(
                &runner,
                Path::new(&created.target.path),
                &["merge-base", "origin/main", "HEAD"],
            )
            .await,
        )
        .unwrap();
        let main_head =
            String::from_utf8(run(&runner, &main, &["rev-parse", "main"]).await).unwrap();
        assert_eq!(anchor.trim(), main_head.trim());
        let repository_state: serde_json::Value = {
            let store = Store::open(
                &service.config.state.path,
                service.config.state.identity.clone(),
            )
            .unwrap();
            serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap()
        };
        assert_eq!(
            repository_state["slugs"][created.target.slug()]["baseBranch"],
            "main",
            "state = {repository_state}"
        );
        assert_eq!(
            repository_state["slugs"][created.target.slug()]["baseSha"],
            anchor.trim()
        );
    }

    #[tokio::test]
    async fn create_review_worktree_refuses_branch_collision_and_moved_head() {
        let (_scratch, service, runner, main, root) = setup().await;
        let remote = root.parent().unwrap().join("review-origin.git");
        fs::create_dir_all(&remote).await.unwrap();
        run(&runner, &remote, &["init", "--bare"]).await;
        run(
            &runner,
            &main,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .await;
        run(&runner, &main, &["push", "origin", "main"]).await;
        run(&runner, &main, &["checkout", "-b", "pr/source"]).await;
        fs::write(main.join("review.txt"), "actual reviewed head\n")
            .await
            .unwrap();
        run(&runner, &main, &["add", "review.txt"]).await;
        run(&runner, &main, &["commit", "-m", "actual review head"]).await;
        let actual_head = String::from_utf8(run(&runner, &main, &["rev-parse", "HEAD"]).await)
            .unwrap()
            .trim()
            .to_owned();
        run(
            &runner,
            &main,
            &["push", "origin", "HEAD:refs/pull/43/head"],
        )
        .await;
        run(&runner, &main, &["checkout", "main"]).await;
        run(&runner, &main, &["branch", "-D", "pr/source"]).await;

        let stale = String::from_utf8(run(&runner, &main, &["rev-parse", "HEAD"]).await)
            .unwrap()
            .trim()
            .to_owned();
        let moved = service
            .create_from_pull_request(
                "pr/source",
                43,
                &stale,
                CreateOptions {
                    fetch_origin: false,
                    run_install: false,
                    ..CreateOptions::default()
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            moved.to_string().contains("changed during checkout"),
            "unexpected error: {moved}"
        );
        assert!(!root.join("pr-source").exists());

        run(&runner, &main, &["branch", "pr/source", "main"]).await;
        let collision = service
            .create_from_pull_request(
                "pr/source",
                43,
                &actual_head,
                CreateOptions {
                    fetch_origin: false,
                    run_install: false,
                    ..CreateOptions::default()
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(collision.to_string().contains("already exists on local"));
        assert!(!root.join("pr-source").exists());
    }

    #[tokio::test]
    async fn restore_requires_exact_removed_snapshot_and_consumes_it_only_on_success() {
        let (_scratch, service, _runner, _main, _root) = setup().await;
        let cancellation = CancellationToken::new();
        let slug = "ENG-42-restore";
        let branch = "michael/ENG-42-restore";
        let removed_at = now_iso();
        let saved = RemovedWorktree {
            slug: slug.into(),
            branch: branch.into(),
            removed_at: removed_at.clone(),
            work: None,
            automations_paused: Some(true),
            extra: serde_json::Map::from_iter([("title".into(), serde_json::json!("saved title"))]),
        };
        service
            .mutate_store(&cancellation, {
                let saved = saved.clone();
                move |store| store.record_removed_worktrees(&[saved], now_ms())
            })
            .await
            .unwrap();

        let stale = service
            .create_from_removed(
                slug,
                branch,
                "2026-10-08T00:00:00Z",
                CreateOptions {
                    base: Some("refs/heads/main".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await;
        assert!(matches!(stale, Err(LifecycleError::Refused(_))));
        let after_stale = service
            .mutate_store(&cancellation, |store| store.read_removed_worktrees())
            .await
            .unwrap();
        assert_eq!(after_stale, vec![saved.clone()]);

        let restored = service
            .create_from_removed(
                slug,
                branch,
                &removed_at,
                CreateOptions {
                    base: Some("refs/heads/main".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        assert_eq!(restored.target.slug(), slug);
        let after_restore = service
            .mutate_store(&cancellation, |store| store.read_removed_worktrees())
            .await
            .unwrap();
        assert!(after_restore.is_empty());
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
                        removed_snapshot: None,
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
        let cleanup_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = cleanup_ran.clone();
        let service = service.with_before_remove(move |_target, _cancellation| {
            let observed = observed.clone();
            async move {
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                Vec::new()
            }
        });
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
        assert!(!cleanup_ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(Path::new(&created.target.path).exists());
    }

    /// Commits a stopped rebase already replayed live only on its detached
    /// HEAD; a clean, pushed branch mid-rebase must not be removable.
    #[tokio::test]
    async fn a_stopped_rebase_refuses_removal_without_explicit_force() {
        let (_scratch, service, runner, _main, _root) = setup().await;
        let cancellation = CancellationToken::new();
        let created = service
            .create(
                "michael/ENG-10-rebasing",
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
        let git_dir = runner
            .run(
                CommandSpec::new("git")
                    .args(["rev-parse", "--absolute-git-dir"])
                    .cwd(path),
                &cancellation,
            )
            .await
            .unwrap()
            .stdout_text();
        let clean = service
            .removal_revision(&created.target, false, &cancellation)
            .await
            .unwrap();
        assert!(!clean.hazards.iter().any(|h| h == IN_PROGRESS_HAZARD));
        fs::create_dir(Path::new(git_dir.trim()).join("rebase-merge"))
            .await
            .unwrap();
        let rebasing = service
            .removal_revision(&created.target, false, &cancellation)
            .await
            .unwrap();
        assert!(rebasing.hazards.iter().any(|h| h == IN_PROGRESS_HAZARD));
        assert!(matches!(
            service.remove(&created.target, RemoveOptions::default(), &cancellation).await,
            Err(LifecycleError::Refused(message)) if message.contains("rebase")
        ));
        assert!(path.exists());
    }

    /// A checkout deleted while Git kept its registration stays on the board
    /// as live. `rm` must finish it without the checkout, keeping the branch
    /// unless forced, since its commits can no longer be checked.
    #[tokio::test]
    async fn a_registration_whose_checkout_is_gone_can_still_be_removed() {
        let (_scratch, service, runner, main, _root) = setup().await;
        let cancellation = CancellationToken::new();
        let created = service
            .create(
                "michael/ENG-11-orphaned",
                CreateOptions {
                    base: Some("refs/heads/main".into()),
                    fetch_origin: false,
                    run_install: false,
                },
                &cancellation,
            )
            .await
            .unwrap();
        fs::remove_dir_all(&created.target.path).await.unwrap();
        let registered = |service: &LifecycleService| {
            let cancellation = cancellation.clone();
            let slug = created.target.slug().to_owned();
            let service = service.clone();
            async move {
                service
                    .repository
                    .inventory(&cancellation)
                    .await
                    .unwrap()
                    .iter()
                    .any(|row| row.target.slug() == slug)
            }
        };
        assert!(registered(&service).await);

        let revision = service
            .removal_revision(&created.target, false, &cancellation)
            .await
            .unwrap();
        assert_eq!(revision.hazards, [MISSING_CHECKOUT_HAZARD]);
        assert!(is_lost_work_hazard(MISSING_CHECKOUT_HAZARD));
        let delete_branch = RemoveOptions {
            delete_branch: true,
            ..RemoveOptions::default()
        };
        assert!(matches!(
            service
                .remove_with_revision(&created.target, delete_branch, &revision, &cancellation)
                .await,
            Err(LifecycleError::Refused(message)) if message.contains("already gone")
        ));
        assert!(registered(&service).await);

        let result = service
            .remove_with_revision(
                &created.target,
                RemoveOptions::default(),
                &revision,
                &cancellation,
            )
            .await
            .unwrap();
        assert!(result.removed && !result.deleted_branch);
        assert!(!registered(&service).await);
        let branch = runner
            .run(
                CommandSpec::new("git")
                    .args([
                        "show-ref",
                        "--verify",
                        "--quiet",
                        "refs/heads/michael/ENG-11-orphaned",
                    ])
                    .cwd(&main),
                &cancellation,
            )
            .await
            .unwrap();
        assert!(branch.status.success());
    }

    #[tokio::test]
    async fn removal_revision_hashes_large_patches_and_rejects_changes_after_confirmation() {
        let (_scratch, service, runner, _main, _root) = setup().await;
        let cancellation = CancellationToken::new();
        let created = service
            .create(
                "michael/ENG-78-large-revision",
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
        let mut content = vec![b'a'; PROCESS_OUTPUT_LIMIT + 1024];
        content.push(b'\n');
        fs::write(path.join("tracked.txt"), &content).await.unwrap();
        let unstaged = service
            .removal_revision(&created.target, false, &cancellation)
            .await
            .unwrap();
        assert_eq!(unstaged.hazards, [UNCOMMITTED_HAZARD]);
        runner
            .run(
                CommandSpec::new("git")
                    .args(["add", "tracked.txt"])
                    .cwd(path),
                &cancellation,
            )
            .await
            .unwrap()
            .checked("git")
            .unwrap();
        let staged = service
            .removal_revision(&created.target, false, &cancellation)
            .await
            .unwrap();
        assert_ne!(unstaged.digest, staged.digest);
        // A change beyond the old capture boundary must remain part of the
        // proof, including when forced removal was confirmed while dirty.
        content[PROCESS_OUTPUT_LIMIT + 1] = b'b';
        fs::write(path.join("tracked.txt"), &content).await.unwrap();
        let changed = service
            .removal_revision(&created.target, false, &cancellation)
            .await
            .unwrap();
        assert_ne!(staged.digest, changed.digest);
        let result = service
            .remove_with_revision(
                &created.target,
                RemoveOptions {
                    force: true,
                    ..RemoveOptions::default()
                },
                &staged,
                &cancellation,
            )
            .await;
        assert!(
            matches!(result, Err(LifecycleError::Refused(message)) if message.contains("changed"))
        );
        assert!(path.exists());
    }

    #[test]
    fn untracked_snapshot_hashes_large_files_and_honors_cancellation() {
        let scratch = tempfile::tempdir().unwrap();
        let mut content = vec![b'a'; PROCESS_OUTPUT_LIMIT + 1024];
        let path = scratch.path().join("large");
        std::fs::write(&path, &content).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let cancellation = CancellationToken::new();
        let first = untracked_metadata(scratch.path(), b"large\0", &cancellation).unwrap();
        content[PROCESS_OUTPUT_LIMIT + 1] = b'b';
        std::fs::write(&path, &content).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        let second = untracked_metadata(scratch.path(), b"large\0", &cancellation).unwrap();
        assert_ne!(first, second);
        cancellation.cancel();
        assert!(matches!(
            untracked_metadata(scratch.path(), b"large\0", &cancellation),
            Err(UntrackedSnapshotError::Unsafe(message)) if message.contains("cancelled")
        ));
        assert!(matches!(
            untracked_metadata(scratch.path(), b"vanished\0", &CancellationToken::new()),
            Err(UntrackedSnapshotError::Changed(_))
        ));
    }

    #[tokio::test]
    async fn force_removal_revision_rejects_changed_diff_even_when_hazard_label_is_same() {
        let (_scratch, service, _runner, _main, _root) = setup().await;
        let cleanup_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = cleanup_ran.clone();
        let service = service.with_before_remove(move |_target, _cancellation| {
            let observed = observed.clone();
            async move {
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                Vec::new()
            }
        });
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
                    removed_snapshot: None,
                },
                &first,
                &cancellation,
            )
            .await;
        assert!(matches!(result, Err(LifecycleError::Refused(_))));
        assert!(!cleanup_ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(path.exists());
    }

    #[tokio::test]
    async fn successful_sst_removal_allows_only_configured_generated_file_changes() {
        let (_scratch, mut service, runner, main, _root) = setup().await;
        let cancellation = CancellationToken::new();
        fs::write(main.join("generated.txt"), "generated baseline\n")
            .await
            .unwrap();
        run(&runner, &main, &["add", "generated.txt"]).await;
        run(&runner, &main, &["commit", "-m", "add generated file"]).await;
        service.config.has_sst = true;
        service.config.auto_regen_paths = vec!["generated.txt".into()];

        let created = service
            .create(
                "michael/ENG-101-sst-remove",
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
        fs::create_dir_all(path.join(".sst")).await.unwrap();
        fs::write(path.join(".sst/stage"), "stage-eng-101-sst\n")
            .await
            .unwrap();
        let script = _scratch.path().join("fake-pnpm-generated-only");
        write_executable(
            &script,
            "#!/bin/sh\nprintf 'regenerated by SST\\n' > generated.txt\n",
        )
        .await;
        service.sst_program = script.into_os_string();
        let revision = service
            .removal_revision(&created.target, false, &cancellation)
            .await
            .unwrap();

        let removed = service
            .remove_with_revision(
                &created.target,
                RemoveOptions {
                    force: true,
                    delete_branch: true,
                    landed: false,
                    destroy_stage: true,
                    removed_snapshot: None,
                },
                &revision,
                &cancellation,
            )
            .await
            .unwrap();
        assert!(removed.destroyed_stage, "{:?}", removed.warnings);
        assert!(removed.removed);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn successful_sst_removal_still_refuses_unrelated_checkout_changes() {
        let (_scratch, mut service, runner, main, _root) = setup().await;
        let cancellation = CancellationToken::new();
        for (name, contents) in [
            ("generated.txt", "generated baseline\n"),
            ("unrelated.txt", "unrelated baseline\n"),
        ] {
            fs::write(main.join(name), contents).await.unwrap();
            run(&runner, &main, &["add", name]).await;
        }
        run(&runner, &main, &["commit", "-m", "add generated files"]).await;
        service.config.has_sst = true;
        service.config.auto_regen_paths = vec!["generated.txt".into()];

        let created = service
            .create(
                "michael/ENG-102-sst-unrelated",
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
        fs::create_dir_all(path.join(".sst")).await.unwrap();
        fs::write(path.join(".sst/stage"), "stage-eng-102-sst\n")
            .await
            .unwrap();
        let script = _scratch.path().join("fake-pnpm-with-unrelated-change");
        write_executable(
            &script,
            "#!/bin/sh\nprintf 'regenerated by SST\\n' > generated.txt\nprintf 'unexpected edit\\n' > unrelated.txt\n",
        )
        .await;
        service.sst_program = script.into_os_string();
        let revision = service
            .removal_revision(&created.target, false, &cancellation)
            .await
            .unwrap();

        let result = service
            .remove_with_revision(
                &created.target,
                RemoveOptions {
                    force: true,
                    delete_branch: true,
                    landed: false,
                    destroy_stage: true,
                    removed_snapshot: None,
                },
                &revision,
                &cancellation,
            )
            .await;
        assert!(
            matches!(result, Err(LifecycleError::Refused(ref message)) if message.contains("outside the configured generated paths")),
            "{result:?}"
        );
        assert!(path.exists());
        assert_eq!(
            fs::read_to_string(path.join("unrelated.txt"))
                .await
                .unwrap(),
            "unexpected edit\n"
        );
    }

    async fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, contents).await.unwrap();
        let mut permissions = fs::metadata(path).await.unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).await.unwrap();
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

    /// Landing evidence covers the pushed head. A commit made after the push
    /// exists only locally, so even a confirmed-landed cleanup refuses it.
    #[tokio::test]
    async fn landed_cleanup_still_refuses_commits_beyond_the_origin_branch() {
        let (scratch, service, runner, main, _root) = setup().await;
        let cancellation = CancellationToken::new();
        let remote = scratch.path().join("origin.git");
        run(
            &runner,
            scratch.path(),
            &["init", "--bare", remote.to_str().unwrap()],
        )
        .await;
        run(
            &runner,
            &main,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        )
        .await;
        run(&runner, &main, &["push", "origin", "main"]).await;
        let created = service
            .create(
                "michael/ENG-11-landed",
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
        fs::write(path.join("tracked.txt"), "merged work\n")
            .await
            .unwrap();
        run(&runner, &path, &["commit", "-am", "merged work"]).await;
        run(&runner, &path, &["push", "origin", "michael/ENG-11-landed"]).await;
        fs::write(path.join("tracked.txt"), "after the merge\n")
            .await
            .unwrap();
        run(&runner, &path, &["commit", "-am", "local only"]).await;
        let candidate = CleanupCandidate {
            target: created.target.clone(),
            landed: true,
            destroy_stage: false,
        };
        let refused = service
            .cleanup(std::slice::from_ref(&candidate), &cancellation)
            .await;
        assert!(
            matches!(&refused[..], [Err(LifecycleError::Refused(message))] if message.contains("1 unpushed commit")),
            "{refused:?}"
        );
        assert!(path.exists());

        run(&runner, &path, &["push", "origin", "michael/ENG-11-landed"]).await;
        let removed = service.cleanup(&[candidate], &cancellation).await;
        assert!(
            matches!(&removed[..], [Ok(result)] if result.removed),
            "{removed:?}"
        );
        assert!(!path.exists());
    }

    /// Without a caller witness, a successful SST removal must not turn the
    /// checkout removal into an unverified force that discards new edits.
    #[tokio::test]
    async fn unwitnessed_sst_removal_refuses_edits_outside_generated_paths() {
        let (_scratch, mut service, runner, main, _root) = setup().await;
        let cancellation = CancellationToken::new();
        fs::write(main.join("generated.txt"), "generated baseline\n")
            .await
            .unwrap();
        run(&runner, &main, &["add", "generated.txt"]).await;
        run(&runner, &main, &["commit", "-m", "add generated file"]).await;
        fs::write(main.join(".git/info/exclude"), ".sst/\n")
            .await
            .unwrap();
        service.config.has_sst = true;
        service.config.auto_regen_paths = vec!["generated.txt".into()];
        let created = service
            .create(
                "michael/ENG-103-sst-unwitnessed",
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
        fs::write(path.join(".sst/stage"), "stage-eng-103-sst\n")
            .await
            .unwrap();
        let script = _scratch.path().join("fake-pnpm-edits-during-sst");
        write_executable(
            &script,
            "#!/bin/sh\nprintf 'regenerated by SST\\n' > generated.txt\nprintf 'edited meanwhile\\n' > tracked.txt\n",
        )
        .await;
        service.sst_program = script.into_os_string();

        let result = service
            .remove(
                &created.target,
                RemoveOptions {
                    force: false,
                    delete_branch: true,
                    landed: false,
                    destroy_stage: true,
                    removed_snapshot: None,
                },
                &cancellation,
            )
            .await;
        assert!(
            matches!(result, Err(LifecycleError::Refused(ref message)) if message.contains("outside the configured generated paths")),
            "{result:?}"
        );
        assert_eq!(
            fs::read_to_string(path.join("tracked.txt")).await.unwrap(),
            "edited meanwhile\n"
        );
    }

    #[tokio::test]
    async fn retired_rift_clones_remove_without_rift_and_keep_normal_safety_guards() {
        let (_scratch, mut service, runner, main, root) = setup().await;
        service.config.rift_binary = "/nonexistent/rift-must-not-run".into();
        let cancellation = CancellationToken::new();
        let mut targets = Vec::new();
        for name in [
            "legacy-clean",
            "legacy-dirty",
            "legacy-unpushed",
            "legacy-verify",
        ] {
            let path = root.join(name);
            run(
                &runner,
                &main,
                &[
                    "clone",
                    "--no-hardlinks",
                    main.to_str().unwrap(),
                    path.to_str().unwrap(),
                ],
            )
            .await;
            run(&runner, &path, &["switch", "-c", name]).await;
            fs::write(path.join(".rift"), "fixture marker")
                .await
                .unwrap();
            fs::write(path.join(".git/info/exclude"), ".rift\n")
                .await
                .unwrap();
            targets.push(local_worktree_target(
                name,
                name,
                path.to_string_lossy(),
                "stagepersonal",
            ));
        }
        let dirty = Path::new(&targets[1].path);
        fs::write(dirty.join("tracked.txt"), "uncommitted work")
            .await
            .unwrap();
        let unpushed = Path::new(&targets[2].path);
        fs::write(unpushed.join("tracked.txt"), "unpublished work")
            .await
            .unwrap();
        run(&runner, unpushed, &["add", "tracked.txt"]).await;
        run(
            &runner,
            unpushed,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "private commit",
            ],
        )
        .await;
        // Publish the verify fixture's branch so its only hazard is the gate.
        run(
            &runner,
            Path::new(&targets[3].path),
            &["update-ref", "refs/remotes/origin/legacy-verify", "HEAD"],
        )
        .await;
        run(
            &runner,
            Path::new(&targets[3].path),
            &["branch", "--set-upstream-to", "origin/legacy-verify"],
        )
        .await;
        service
            .mutate_store(&cancellation, |store| {
                store.set_slug_work_status(
                    "legacy-verify",
                    Some(&wt_store::WorkStatusRecord {
                        state: "ready".into(),
                        at: "fixture".into(),
                        note: None,
                        risk: Some("low".into()),
                        sha: None,
                        by: None,
                        blocked_on: None,
                        verify_after_merge: Some("check the deployment".into()),
                        extra: Default::default(),
                    }),
                )
            })
            .await
            .unwrap();
        for (target, hazard) in [
            (&targets[1], "uncommitted changes"),
            (&targets[2], "unpushed commit"),
            (&targets[3], "post-merge verification still owed"),
        ] {
            let result = service
                .remove(target, RemoveOptions::default(), &cancellation)
                .await;
            assert!(
                matches!(result, Err(LifecycleError::Refused(ref message)) if message.contains(hazard)),
                "{result:?}"
            );
            assert!(Path::new(&target.path).exists());
        }
        service
            .remove(&targets[0], RemoveOptions::default(), &cancellation)
            .await
            .unwrap();
        assert!(!Path::new(&targets[0].path).exists());
        assert_eq!(
            fs::read_to_string(dirty.join("tracked.txt")).await.unwrap(),
            "uncommitted work"
        );
        assert_eq!(
            fs::read_to_string(unpushed.join("tracked.txt"))
                .await
                .unwrap(),
            "unpublished work"
        );
        assert!(main.join(".git").is_dir());
    }

    #[tokio::test]
    async fn retired_clone_removal_refuses_linked_git_metadata_and_cancellation() {
        let (_scratch, service, _runner, _main, root) = setup().await;
        let path = root.join("not-an-independent-clone");
        fs::create_dir(&path).await.unwrap();
        let path = fs::canonicalize(path).await.unwrap();
        fs::write(path.join(".rift"), "marker").await.unwrap();
        fs::write(path.join(".git"), "gitdir: elsewhere")
            .await
            .unwrap();
        assert!(matches!(
            service
                .remove_retired_clone(&path, &CancellationToken::new())
                .await,
            Err(LifecycleError::Refused(_))
        ));
        assert!(path.exists());
        fs::remove_file(path.join(".git")).await.unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(_main.join(".git"), path.join(".git")).unwrap();
            assert!(matches!(
                service
                    .remove_retired_clone(&path, &CancellationToken::new())
                    .await,
                Err(LifecycleError::Refused(_))
            ));
            assert!(_main.join(".git/HEAD").exists());
            fs::remove_file(path.join(".git")).await.unwrap();
        }
        fs::create_dir(path.join(".git")).await.unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            service.remove_retired_clone(&path, &cancellation).await,
            Err(LifecycleError::Cancelled)
        ));
        assert!(path.exists());
    }

    async fn retired_clone_fixture(
        runner: &ProcessRunner,
        main: &Path,
        root: &Path,
        name: &str,
    ) -> WorktreeTarget {
        let path = root.join(name);
        run(
            runner,
            main,
            &[
                "clone",
                "--no-hardlinks",
                main.to_str().unwrap(),
                path.to_str().unwrap(),
            ],
        )
        .await;
        run(runner, &path, &["switch", "-c", name]).await;
        fs::write(path.join(".rift"), "fixture marker")
            .await
            .unwrap();
        fs::write(path.join(".git/info/exclude"), ".rift\n")
            .await
            .unwrap();
        local_worktree_target(name, name, path.to_string_lossy(), "stagepersonal")
    }

    fn quarantines(root: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| GitRepository::is_removal_quarantine(&entry.file_name()))
            .map(|entry| entry.path())
            .collect()
    }

    #[tokio::test]
    async fn retired_clone_removal_survives_a_writer_recreating_git_entries() {
        let (_scratch, mut service, runner, main, root) = setup().await;
        service.config.rift_binary = "/nonexistent/rift-must-not-run".into();
        let target = retired_clone_fixture(&runner, &main, &root, "busy-clone").await;
        let path = PathBuf::from(&target.path);
        let cookies = path.join(".git/fsmonitor--daemon/cookies");
        std::fs::create_dir_all(&cookies).unwrap();
        for index in 0..2000 {
            std::fs::write(cookies.join(format!("seed-{index}")), "").unwrap();
        }
        // Model a daemon that keeps writing cookies into the directory it
        // already holds, wherever it has been renamed to. It never recreates
        // parents, so it cannot resurrect the original checkout path.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let writer = std::thread::spawn({
            let stop = Arc::clone(&stop);
            let writes = Arc::clone(&writes);
            let root = root.clone();
            move || {
                let mut first_quarantine_write = None::<std::time::Instant>;
                let mut index = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if first_quarantine_write
                        .is_some_and(|at| at.elapsed() > Duration::from_millis(300))
                    {
                        break;
                    }
                    for directory in quarantines(&root) {
                        let cookies = directory.join(".git/fsmonitor--daemon/cookies");
                        index += 1;
                        if std::fs::write(cookies.join(format!("live-{index}")), "").is_ok() {
                            first_quarantine_write.get_or_insert_with(std::time::Instant::now);
                            writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    std::thread::yield_now();
                }
            }
        });
        let result = service
            .remove(&target, RemoveOptions::default(), &CancellationToken::new())
            .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
        assert!(!path.exists(), "original checkout path must be gone");
        // The writer stops 300ms after its first quarantine write, before the
        // third bounded attempt, so deletion must finish. Without the retry,
        // this fixture fails with `Directory not empty`.
        let removed = result.unwrap();
        assert!(removed.removed);
        assert!(quarantines(&root).is_empty(), "{:?}", quarantines(&root));
        let removed = service
            .mutate_store(&CancellationToken::new(), |store| {
                store.read_removed_worktrees()
            })
            .await
            .unwrap();
        assert!(removed.iter().any(|entry| entry.slug == "busy-clone"));
        assert!(
            writes.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "the writer never raced the deletion"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn undeletable_quarantine_is_a_warning_after_a_successful_removal() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (_scratch, mut service, runner, main, root) = setup().await;
        service.config.rift_binary = "/nonexistent/rift-must-not-run".into();
        let target = retired_clone_fixture(&runner, &main, &root, "stuck-clone").await;
        let locked = Path::new(&target.path).join(".git/locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("file"), "").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = service
            .remove(&target, RemoveOptions::default(), &CancellationToken::new())
            .await;
        let left = quarantines(&root);
        for directory in &left {
            let _ = std::fs::set_permissions(
                directory.join(".git/locked"),
                std::fs::Permissions::from_mode(0o700),
            );
        }
        let result = result.expect("the removal is past its commit point");
        assert!(result.removed);
        assert_eq!(left.len(), 1);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains(&*left[0].to_string_lossy())
                    && warning.contains("manually")),
            "{:?}",
            result.warnings
        );
        assert!(!Path::new(&target.path).exists());
        let removed = service
            .mutate_store(&CancellationToken::new(), |store| {
                store.read_removed_worktrees()
            })
            .await
            .unwrap();
        assert!(removed.iter().any(|entry| entry.slug == "stuck-clone"));
    }

    #[tokio::test]
    async fn quarantine_rename_refuses_a_replaced_checkout_and_inventory_ignores_quarantines() {
        let (_scratch, service, runner, main, root) = setup().await;
        let path = root.join("swapped");
        fs::create_dir(&path).await.unwrap();
        let verified = directory_identity(&std::fs::symlink_metadata(&path).unwrap());
        // Keep the original inode alive elsewhere so it cannot be reused.
        fs::rename(&path, root.join("held-original")).await.unwrap();
        fs::create_dir(&path).await.unwrap();
        fs::write(path.join("replacement.txt"), "keep me")
            .await
            .unwrap();
        assert!(matches!(
            quarantine_checkout(&path, verified).await,
            Err(LifecycleError::Refused(ref message)) if message.contains("identity changed")
        ));
        assert_eq!(
            fs::read_to_string(path.join("replacement.txt"))
                .await
                .unwrap(),
            "keep me"
        );
        assert!(quarantines(&root).is_empty());

        // A matching identity is moved aside, not deleted, and the moved
        // clone is invisible to inventory and therefore to maintenance.
        let target = retired_clone_fixture(&runner, &main, &root, "aside").await;
        let clone = PathBuf::from(&target.path);
        let identity = directory_identity(&std::fs::symlink_metadata(&clone).unwrap());
        let inventory = service
            .repository
            .inventory(&CancellationToken::new())
            .await
            .unwrap();
        assert!(inventory.iter().any(|row| row.target.slug() == "aside"));
        let quarantine = quarantine_checkout(&clone, identity).await.unwrap();
        assert!(!clone.exists());
        assert!(quarantine.join(".git/HEAD").exists());
        let inventory = service
            .repository
            .inventory(&CancellationToken::new())
            .await
            .unwrap();
        assert!(
            inventory
                .iter()
                .all(|row| !GitRepository::is_removal_quarantine(
                    Path::new(&row.target.path).file_name().unwrap()
                )),
            "{inventory:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn removal_stops_the_checkout_fsmonitor_daemon_before_deleting() {
        let (_scratch, mut service, runner, main, root) = setup().await;
        service.config.rift_binary = "/nonexistent/rift-must-not-run".into();
        let target = retired_clone_fixture(&runner, &main, &root, "watched").await;
        let path = PathBuf::from(&target.path);
        run(&runner, &path, &["-c", "core.fsmonitor=true", "status"]).await;
        let ipc = path.join(".git/fsmonitor--daemon.ipc");
        if !ipc.exists() {
            // This Git build has no builtin fsmonitor; nothing to stop.
            return;
        }
        let row = service
            .find_live_target(&target, &CancellationToken::new())
            .await
            .unwrap();
        service
            .stop_fsmonitor(&row, &path, &CancellationToken::new())
            .await;
        assert!(!ipc.exists(), "daemon IPC must be gone after stop");
        run(&runner, &path, &["-c", "core.fsmonitor=true", "status"]).await;
        assert!(ipc.exists());
        service
            .remove(&target, RemoveOptions::default(), &CancellationToken::new())
            .await
            .unwrap();
        assert!(!path.exists());
        assert!(quarantines(&root).is_empty());
    }

    #[tokio::test]
    async fn retired_clone_removal_waits_for_in_flight_clone_maintenance() {
        let (_scratch, mut service, runner, main, root) = setup().await;
        service.config.rift_binary = "/nonexistent/rift-must-not-run".into();
        let target = retired_clone_fixture(&runner, &main, &root, "maintained").await;
        let held = service
            .repository
            .lock_clone_maintenance("maintained", &CancellationToken::new())
            .await
            .unwrap();
        let removal = tokio::spawn({
            let service = service.clone();
            let target = target.clone();
            async move {
                service
                    .remove(&target, RemoveOptions::default(), &CancellationToken::new())
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            Path::new(&target.path).join(".git/HEAD").exists(),
            "removal must not delete while clone maintenance is running"
        );
        assert!(!removal.is_finished());
        drop(held);
        removal.await.unwrap().unwrap();
        assert!(!Path::new(&target.path).exists());
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
                    removed_snapshot: None,
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
                    removed_snapshot: None,
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
                    removed_snapshot: None,
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
                    removed_snapshot: None,
                },
                &cancellation,
            )
            .await
            .unwrap();
    }
}
