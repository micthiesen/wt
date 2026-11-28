//! Fetch `origin` and safely advance local base references.

use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use wt_platform::install::InstallPolicy;
use wt_platform::lock::{
    DEPENDENCY_INSTALL_POOL, DEPENDENCY_INSTALL_SLOTS, FileLock, LockError, MachineSlots,
};
use wt_platform::process::{CommandSpec, ProcessError, ProcessRunner};

use crate::{
    repository::{
        GitRepository, RepositoryError, RepositoryKind, WorktreeRecord, git_metadata_paths,
    },
    status::common_git_env,
};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Waiting for admission only: a started fast-forward always finishes.
const FAST_FORWARD_ADMISSION: Duration = Duration::from_secs(60);
const GIT_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
/// Beside the fetch lock: when the last successful `git fetch` started, in
/// wall-clock nanoseconds. Written only while holding that lock, and read
/// under it wherever a fetch may follow the read.
const FETCH_STAMP: &str = "fetch-origin.stamp";
/// Held for the main clone's dependency install, beside the fetch lock.
const MAIN_INSTALL_LOCK: &str = "main-dependency-sync";
/// Beside that lock: the main clone commit its last successful install ran
/// at, or the commit before a pull whose install is still owed. Written only
/// under the stamp lock.
const MAIN_INSTALL_STAMP: &str = "main-dependency-sync.stamp";
/// Serializes writers of that stamp.
const MAIN_INSTALL_STAMP_LOCK: &str = "main-dependency-sync-stamp";

/// An install the main clone owes.
struct InstallDue {
    /// The commit to install at.
    head: String,
    /// Whether the stamp, not this pull's own range, showed it.
    stamped: bool,
}

/// What one pull hands the main clone's dependency sync.
struct DependencySync<'a> {
    policy: &'a InstallPolicy,
    install_lock_dir: Option<&'a Path>,
    repository_lock_dir: &'a Path,
    /// The main clone's HEAD before the pull.
    before: &'a str,
}

fn lock_error(path: &Path, source: LockError) -> RepositoryError {
    if matches!(source, LockError::Cancelled) {
        RepositoryError::Cancelled
    } else {
        RepositoryError::FetchLock {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Wall clock, because the stamp is compared across processes. `None` (a
/// clock before the epoch) disables both stamping and skipping.
fn wall_clock_nanos() -> Option<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_nanos())
}

/// Whether a fetch some process already completed began at or after
/// `requested`, so it saw everything this caller could be asking for. A
/// stamp from the future (a clock that stepped back) proves nothing.
async fn stamped_fetch_covers(stamp: &Path, requested: Option<u128>) -> bool {
    let (Some(requested), Some(now)) = (requested, wall_clock_nanos()) else {
        return false;
    };
    tokio::fs::read_to_string(stamp)
        .await
        .ok()
        .and_then(|text| text.trim().parse::<u128>().ok())
        .is_some_and(|started| started >= requested && started <= now)
}

#[derive(Clone, Debug, Default)]
pub struct FetchOriginOptions {
    /// Local branches to keep in sync with their `origin/<branch>` refs.
    pub keep_fresh: Vec<String>,
    /// Explicitly configured generated paths; staged edits are never discarded.
    pub auto_regen_paths: Vec<String>,
    /// Frozen dependency maintenance of the main clone after a pull that
    /// changed a lockfile. It runs after the fetch lock is released.
    pub sync_install: Option<InstallPolicy>,
    /// The configured lock directory, where the machine's dependency-install
    /// slots live ([`wt_platform::lock::DEPENDENCY_INSTALL_POOL`]). With it
    /// the main clone's install takes one of them like every other install;
    /// without it the install is unbounded across processes.
    pub install_lock_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchOriginReport {
    pub before_main_head: Option<String>,
    pub after_main_head: Option<String>,
    pub warnings: Vec<String>,
    pub rift_refs_updated: usize,
}

/// Whether [`GitRepository::fetch_origin_refs`] ran a fetch of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchedRefs {
    Fetched,
    /// A fetch another caller completed stands in for it.
    Reused,
}

#[derive(Clone, Debug)]
enum FetchFlightError {
    Cancelled,
    Failed(String),
}

pub(crate) struct FetchFlight {
    result: watch::Receiver<Option<Result<FetchOriginReport, FetchFlightError>>>,
}

fn is_cancelled(error: &RepositoryError) -> bool {
    matches!(
        error,
        RepositoryError::Cancelled
            | RepositoryError::Process {
                source: ProcessError::Cancelled { .. },
                ..
            }
            | RepositoryError::FetchLock {
                source: LockError::Cancelled,
                ..
            }
    )
}

impl GitRepository {
    /// Fetch and safely advance local base refs. Concurrent callers on this
    /// repository instance join the same operation; separate instances and
    /// processes coordinate through a repository-local advisory lock.
    pub async fn fetch_origin(
        &self,
        options: FetchOriginOptions,
        cancellation: &CancellationToken,
    ) -> Result<FetchOriginReport, RepositoryError> {
        if cancellation.is_cancelled() {
            return Err(RepositoryError::Cancelled);
        }
        let (flight, leader, sender) = {
            let mut slot = self.fetch_flight.lock().await;
            if let Some(flight) = slot.as_ref() {
                (Arc::clone(flight), false, None)
            } else {
                #[expect(
                    clippy::disallowed_methods,
                    reason = "single-flight result for concurrent fetch callers, not board data"
                )]
                let (sender, receiver) = watch::channel(None);
                let flight = Arc::new(FetchFlight { result: receiver });
                *slot = Some(Arc::clone(&flight));
                (flight, true, Some(sender))
            }
        };

        if leader {
            // The caller that owns the ProcessRunner operation also waits for
            // child reaping and lock release before this future returns.
            let result = self
                .fetch_origin_locked(options, cancellation)
                .await
                .map_err(|error| {
                    if is_cancelled(&error) {
                        RepositoryError::Cancelled
                    } else {
                        error
                    }
                });
            let shared = result.as_ref().map(Clone::clone).map_err(|error| {
                if is_cancelled(error) {
                    FetchFlightError::Cancelled
                } else {
                    FetchFlightError::Failed(error.to_string())
                }
            });
            if let Some(sender) = sender {
                let _ = sender.send(Some(shared));
            }
            let mut slot = self.fetch_flight.lock().await;
            if slot
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &flight))
            {
                *slot = None;
            }
            return result;
        }

        let mut result = flight.result.clone();
        loop {
            if let Some(result) = result.borrow().clone() {
                return result.map_err(|error| match error {
                    FetchFlightError::Cancelled => RepositoryError::Cancelled,
                    FetchFlightError::Failed(message) => RepositoryError::FetchOrigin {
                        path: self.origin_config().main_clone.clone(),
                        message,
                    },
                });
            }
            tokio::select! {
                _ = cancellation.cancelled() => return Err(RepositoryError::Cancelled),
                changed = result.changed() => {
                    if changed.is_err() {
                        // A dropped leader did not publish a result. Remove its
                        // dead flight and retry as the new owner.
                        let mut slot = self.fetch_flight.lock().await;
                        if slot.as_ref().is_some_and(|active| Arc::ptr_eq(active, &flight)) {
                            *slot = None;
                        }
                        drop(slot);
                        return Box::pin(self.fetch_origin(options.clone(), cancellation)).await;
                    }
                }
            }
        }
    }

    /// Repository-local advisory lock directory shared by fetch maintenance
    /// and lifecycle removal. It lives in the main clone's common Git dir.
    async fn repository_lock_dir(&self) -> PathBuf {
        let (git_dir, common_dir) = git_metadata_paths(&self.origin_config().main_clone).await;
        common_dir
            .or(git_dir)
            .unwrap_or_else(|| self.origin_config().main_clone.join(".git"))
            .join("wt-locks")
    }

    /// Exclude background ref maintenance from one independent clone while a
    /// lifecycle operation deletes it. Removal waits here, under its slug lock,
    /// until any in-flight maintenance of the clone has finished; maintenance
    /// only try-acquires and skips a held clone, so the order cannot deadlock.
    pub async fn lock_clone_maintenance(
        &self,
        slug: &str,
        cancellation: &CancellationToken,
    ) -> Result<FileLock, RepositoryError> {
        let lock_dir = self.repository_lock_dir().await;
        FileLock::acquire(
            &lock_dir,
            &clone_maintenance_key(slug),
            "lock clone maintenance",
            cancellation,
        )
        .await
        .map_err(|source| {
            if matches!(source, LockError::Cancelled) {
                RepositoryError::Cancelled
            } else {
                RepositoryError::FetchLock {
                    path: lock_dir.clone(),
                    source,
                }
            }
        })
    }

    async fn try_lock_clone_maintenance(
        &self,
        slug: &str,
    ) -> Result<Option<FileLock>, RepositoryError> {
        let lock_dir = self.repository_lock_dir().await;
        FileLock::try_acquire(
            &lock_dir,
            &clone_maintenance_key(slug),
            "lock clone maintenance",
        )
        .await
        .map_err(|source| RepositoryError::FetchLock {
            path: lock_dir.clone(),
            source,
        })
    }

    /// Whether a worktree-root entry is a checkout that a removal has already
    /// moved aside. Such entries are never inventory and never maintained.
    pub fn is_removal_quarantine(name: &std::ffi::OsStr) -> bool {
        name.as_encoded_bytes()
            .starts_with(Self::REMOVAL_QUARANTINE_PREFIX.as_bytes())
    }

    /// Name prefix of the sibling directory a removal renames a verified
    /// checkout to before deleting it.
    pub const REMOVAL_QUARANTINE_PREFIX: &'static str = ".wt-removing-";

    async fn fetch_origin_locked(
        &self,
        options: FetchOriginOptions,
        cancellation: &CancellationToken,
    ) -> Result<FetchOriginReport, RepositoryError> {
        // Noted before queueing for the lock: a wave of processes then shares
        // the one fetch that starts after they all asked.
        let requested = wall_clock_nanos();
        self.fetch_origin_requested_at(options, requested, cancellation)
            .await
    }

    /// Bring origin's remote-tracking refs up to date and nothing else: no
    /// checkout, local branch or dependency is touched, so a caller that only
    /// needs to resolve a ref does not wait on that maintenance. A fetch that
    /// started within `reuse` before this call stands in for one of its own;
    /// `Duration::ZERO` accepts only a fetch that started after the call.
    /// Whoever performs a fetch owes the maintenance that follows it
    /// ([`Self::maintain_fetched_refs`]).
    pub async fn fetch_origin_refs(
        &self,
        reuse: Duration,
        cancellation: &CancellationToken,
    ) -> Result<FetchedRefs, RepositoryError> {
        if cancellation.is_cancelled() {
            return Err(RepositoryError::Cancelled);
        }
        let requested = wall_clock_nanos().map(|now| now.saturating_sub(reuse.as_nanos()));
        let lock_dir = self.repository_lock_dir().await;
        // Read without the lock, which its holder keeps through maintenance
        // that can run for minutes. The stamp is written by truncating, so a
        // torn read is empty or a shorter number: an older fetch, never a
        // newer one.
        if stamped_fetch_covers(&lock_dir.join(FETCH_STAMP), requested).await {
            return Ok(FetchedRefs::Reused);
        }
        let _lock = self.lock_fetch(&lock_dir, cancellation).await?;
        Ok(
            if self
                .fetch_unless_covered(&lock_dir, requested, cancellation)
                .await?
            {
                FetchedRefs::Fetched
            } else {
                FetchedRefs::Reused
            },
        )
    }

    /// The maintenance half of [`Self::fetch_origin`] over the refs as they
    /// were last fetched: it fetches only when no fetch has ever completed.
    /// Never joins or leads a shared flight, whose callers are promised a
    /// fetch that started after they asked.
    pub async fn maintain_fetched_refs(
        &self,
        options: FetchOriginOptions,
        cancellation: &CancellationToken,
    ) -> Result<FetchOriginReport, RepositoryError> {
        self.fetch_origin_requested_at(options, Some(0), cancellation)
            .await
    }

    async fn lock_fetch(
        &self,
        lock_dir: &Path,
        cancellation: &CancellationToken,
    ) -> Result<FileLock, RepositoryError> {
        FileLock::acquire(lock_dir, "fetch-origin", "fetch origin", cancellation)
            .await
            .map_err(|source| {
                if matches!(source, LockError::Cancelled) {
                    RepositoryError::Cancelled
                } else {
                    RepositoryError::FetchLock {
                        path: lock_dir.to_path_buf(),
                        source,
                    }
                }
            })
    }

    /// Fetch unless a completed fetch already covers `requested`. Callers
    /// hold the fetch lock. Returns whether it fetched.
    async fn fetch_unless_covered(
        &self,
        lock_dir: &Path,
        requested: Option<u128>,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        let stamp = lock_dir.join(FETCH_STAMP);
        if stamped_fetch_covers(&stamp, requested).await {
            return Ok(false);
        }
        let started = wall_clock_nanos();
        self.run_git_args_on(
            &self.origin_runner().bulk(),
            &self.origin_config().main_clone,
            ["fetch", "origin", "--prune"].map(Into::into).to_vec(),
            cancellation,
            "fetch origin",
        )
        .await?;
        // An unwritten stamp only costs the next waiter its own fetch.
        if let Some(started) = started {
            let _ = tokio::fs::write(&stamp, started.to_string()).await;
        }
        Ok(true)
    }

    async fn fetch_origin_requested_at(
        &self,
        options: FetchOriginOptions,
        requested: Option<u128>,
        cancellation: &CancellationToken,
    ) -> Result<FetchOriginReport, RepositoryError> {
        let lock_dir = self.repository_lock_dir().await;
        let lock = self.lock_fetch(&lock_dir, cancellation).await?;

        let before_main_head = self
            .rev_parse(&self.origin_config().main_clone, "HEAD", cancellation)
            .await?;
        self.fetch_unless_covered(&lock_dir, requested, cancellation)
            .await?;

        let checked_out = self
            .run_git_optional(
                &self.origin_config().main_clone,
                ["symbolic-ref", "--quiet", "--short", "HEAD"],
                cancellation,
                "read checked out branch",
            )
            .await?;
        let checked_out = checked_out.map(|branch| branch.trim().to_owned());
        let mut warnings = Vec::new();
        self.restore_generated(&options.auto_regen_paths, cancellation, &mut warnings)
            .await?;
        let mut branches = BTreeSet::new();
        branches.insert(self.origin_config().trunk_branch.clone());
        branches.extend(options.keep_fresh);
        for branch in branches {
            self.sync_local_branch(
                &branch,
                checked_out.as_deref() == Some(&branch),
                cancellation,
                &mut warnings,
            )
            .await?;
        }

        let mut rift_refs_updated = 0;
        if let Some(tip) = self
            .rev_parse(
                &self.origin_config().main_clone,
                &format!("origin/{}", self.origin_config().trunk_branch),
                cancellation,
            )
            .await?
        {
            match self.inventory(cancellation).await {
                Ok(worktrees) => {
                    let outcomes = stream::iter(
                        worktrees
                            .into_iter()
                            .filter(|row| !row.is_main && row.kind == RepositoryKind::RiftClone)
                            .map(|worktree| {
                                let tip = tip.clone();
                                async move {
                                    let result = self
                                        .freshen_idle_rift_trunk(&worktree, &tip, cancellation)
                                        .await;
                                    (worktree.target.path, result)
                                }
                            }),
                    )
                    .buffer_unordered(4)
                    .collect::<Vec<_>>()
                    .await;
                    for (path, result) in outcomes {
                        match result {
                            Ok(true) => rift_refs_updated += 1,
                            Ok(false) => {}
                            Err(error) if is_cancelled(&error) => return Err(error),
                            Err(error) => warnings.push(format!(
                                "could not freshen origin/{} in {}: {error}",
                                self.origin_config().trunk_branch,
                                path
                            )),
                        }
                    }
                }
                Err(error) if is_cancelled(&error) => return Err(error),
                Err(error) => warnings.push(format!(
                    "could not list Rift clones for trunk-ref refresh: {error}"
                )),
            }
        }

        let after_main_head = self
            .rev_parse(&self.origin_config().main_clone, "HEAD", cancellation)
            .await?;
        if let (Some(policy), Some(before), Some(after)) = (
            options.sync_install.as_ref(),
            before_main_head.as_deref(),
            after_main_head.as_deref(),
        ) && before != after
        {
            // The fetch lock orders fetches, the stamp, and the ref and
            // checkout updates above. The install reads only the pulled
            // tree, and holding the lock through it would keep every other
            // process's fetch (each new worktree runs one) waiting for
            // minutes. Installs have a lock of their own instead.
            drop(lock);
            let sync = DependencySync {
                policy,
                install_lock_dir: options.install_lock_dir.as_deref(),
                repository_lock_dir: &lock_dir,
                before,
            };
            self.sync_main_dependencies(sync, cancellation, &mut warnings)
                .await?;
        }
        Ok(FetchOriginReport {
            before_main_head,
            after_main_head,
            warnings,
            rift_refs_updated,
        })
    }

    async fn restore_generated(
        &self,
        paths: &[String],
        cancellation: &CancellationToken,
        warnings: &mut Vec<String>,
    ) -> Result<(), RepositoryError> {
        let root = &self.origin_config().main_clone;
        for path in paths {
            if !Path::new(path)
                .components()
                .any(|c| matches!(c, Component::Normal(_)))
                || Path::new(path)
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
            {
                warnings.push(format!(
                    "auto_regen_paths: refusing non-relative file path {path:?}"
                ));
                continue;
            }
            let literal = format!(":(literal){path}");
            let tracked = self
                .run_git_args(
                    root,
                    ["ls-files", "--", &literal].map(Into::into).to_vec(),
                    cancellation,
                    "inspect generated tracked path",
                )
                .await?;
            if tracked.is_empty() {
                continue;
            }
            let unstaged = self
                .run_git_args(
                    root,
                    ["diff", "--name-only", "--", &literal]
                        .map(Into::into)
                        .to_vec(),
                    cancellation,
                    "inspect generated changes",
                )
                .await?;
            if unstaged.is_empty() {
                continue;
            }
            let staged = self
                .run_git_args(
                    root,
                    ["diff", "--cached", "--name-only", "--", &literal]
                        .map(Into::into)
                        .to_vec(),
                    cancellation,
                    "inspect staged generated changes",
                )
                .await?;
            if !staged.is_empty() {
                warnings.push(format!(
                    "auto_regen_paths: preserving staged changes in {path}"
                ));
                continue;
            }
            // Restore only tracked, explicitly opted-in generated files. Git's
            // literal pathspec keeps a configured filename from becoming a glob.
            let ok = self
                .run_git_status(
                    root,
                    ["restore", "--worktree", "--", &literal],
                    cancellation,
                    "restore generated path before fast-forward",
                )
                .await?;
            if !ok {
                warnings.push(format!("could not restore generated path {path}"));
            }
        }
        Ok(())
    }

    /// Hold off the main clone's dependency install. A backend that copies
    /// the main clone's packages takes this for the copy, so it never reads
    /// a tree a package manager is writing.
    pub async fn lock_main_dependency_sync(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<FileLock, RepositoryError> {
        let lock_dir = self.repository_lock_dir().await;
        FileLock::acquire(
            &lock_dir,
            MAIN_INSTALL_LOCK,
            "sync main clone dependencies",
            cancellation,
        )
        .await
        .map_err(|source| lock_error(&lock_dir, source))
    }

    /// Install the main clone's dependencies when a lockfile changed since
    /// they were last installed. Runs without the fetch lock, under a lock
    /// of its own, against whatever HEAD the clone has by then: a pass that
    /// queued behind another installs for every pull since the last install,
    /// or finds nothing left to do.
    async fn sync_main_dependencies(
        &self,
        sync: DependencySync<'_>,
        cancellation: &CancellationToken,
        warnings: &mut Vec<String>,
    ) -> Result<(), RepositoryError> {
        let root = &self.origin_config().main_clone;
        let plan = match sync.policy.resolve(root, true).await {
            Ok(Some(plan)) => plan,
            Ok(None) => return Ok(()),
            Err(error) => {
                warnings.push(format!(
                    "could not detect main clone dependency manager: {error}"
                ));
                return Ok(());
            }
        };
        // Looked at first without the lock, so a pull with nothing to
        // install takes neither a slot nor the lock.
        let Some(due) = self.install_due(&plan, &sync, cancellation).await? else {
            return Ok(());
        };
        // With no usable stamp the packages are those of the commit before
        // this pull. Say so before queueing: if this pass is cancelled while
        // it waits, or its install fails, a later pull has no other way to
        // know what it owes.
        if !due.stamped {
            self.record_owed_install(&plan, &sync, cancellation).await?;
        }
        // Every install on the machine shares one bound, so this one waits
        // its turn among the installs of new worktrees. It runs as bulk and
        // not heavy: a heavy slot held for minutes would leave checkouts
        // queued behind it. The slot comes before the lock: a copy of this
        // clone's packages needs the lock, and must not wait out the queue
        // for a slot as well as the install.
        let _slot = match sync.install_lock_dir {
            Some(dir) => Some(
                MachineSlots::open(dir, DEPENDENCY_INSTALL_POOL, DEPENDENCY_INSTALL_SLOTS)
                    .await
                    .map_err(|source| lock_error(dir, source))?
                    .acquire(cancellation)
                    .await
                    .map_err(|source| lock_error(dir, source))?,
            ),
            None => None,
        };
        // Two package managers must not write one `node_modules` at once,
        // which the fetch lock used to prevent.
        let _installing = self.lock_main_dependency_sync(cancellation).await?;
        // Another pass may have installed while this one waited.
        let Some(due) = self.install_due(&plan, &sync, cancellation).await? else {
            return Ok(());
        };
        let head = due.head;
        let result = self
            .origin_runner()
            .bulk()
            // Package managers can be verbose. Keep a bounded diagnostic
            // capture while draining the rest, never terminate an install just
            // because it printed more than the capture budget.
            .run_streaming(plan.spec, cancellation, |_, _| {})
            .await
            .and_then(|output| output.checked("package manager").map(|_| ()));
        match result {
            // An unwritten stamp only costs a repeated install.
            Ok(()) => {
                if let Ok(_writing) = self.lock_install_stamp(&sync, cancellation).await {
                    let stamp = sync.repository_lock_dir.join(MAIN_INSTALL_STAMP);
                    let _ = tokio::fs::write(&stamp, &head).await;
                }
            }
            Err(ProcessError::Cancelled { .. }) => return Err(RepositoryError::Cancelled),
            Err(error) => warnings.push(format!(
                "main clone dependency sync failed ({}): {error}",
                plan.label
            )),
        }
        Ok(())
    }

    /// Held only around a write of the install stamp, never across a wait
    /// for a slot or an install, so a pass that is about to queue can
    /// record what it owes without waiting for a running install.
    async fn lock_install_stamp(
        &self,
        sync: &DependencySync<'_>,
        cancellation: &CancellationToken,
    ) -> Result<FileLock, RepositoryError> {
        FileLock::acquire(
            sync.repository_lock_dir,
            MAIN_INSTALL_STAMP_LOCK,
            "write main clone install stamp",
            cancellation,
        )
        .await
        .map_err(|source| lock_error(sync.repository_lock_dir, source))
    }

    /// Stamp the commit before this pull as the one the packages belong to,
    /// unless a usable stamp has appeared since the caller looked: an
    /// install that finished meanwhile must not be marked undone.
    async fn record_owed_install(
        &self,
        plan: &wt_platform::install::InstallCommand,
        sync: &DependencySync<'_>,
        cancellation: &CancellationToken,
    ) -> Result<(), RepositoryError> {
        let _writing = self.lock_install_stamp(sync, cancellation).await?;
        if self
            .install_due(plan, sync, cancellation)
            .await?
            .is_some_and(|due| !due.stamped)
        {
            // An unwritten stamp leaves things as they were before stamps.
            let stamp = sync.repository_lock_dir.join(MAIN_INSTALL_STAMP);
            let _ = tokio::fs::write(&stamp, sync.before).await;
        }
        Ok(())
    }

    /// The main clone's HEAD when a lockfile differs between the commit its
    /// packages were last installed at and that HEAD. The answer is final
    /// only under the install lock; without it, it is a hint.
    async fn install_due(
        &self,
        plan: &wt_platform::install::InstallCommand,
        sync: &DependencySync<'_>,
        cancellation: &CancellationToken,
    ) -> Result<Option<InstallDue>, RepositoryError> {
        let root = &self.origin_config().main_clone;
        let Some(head) = self.rev_parse(root, "HEAD", cancellation).await? else {
            return Ok(None);
        };
        let head = head.trim().to_owned();
        // The commit the last install here ran at, written under the lock.
        // Without one, or when that commit is gone (or the stamp was read
        // mid-write), this pull's own range.
        let installed =
            tokio::fs::read_to_string(sync.repository_lock_dir.join(MAIN_INSTALL_STAMP))
                .await
                .ok();
        let installed = installed.as_deref().map(str::trim);
        for (since, stamped) in [(installed, true), (Some(sync.before), false)] {
            let Some(since) = since else { continue };
            let mut args = vec![
                "diff".into(),
                "--name-only".into(),
                since.into(),
                head.as_str().into(),
                "--".into(),
            ];
            args.extend(plan.gate_lockfiles.iter().map(|file| (*file).into()));
            match self
                .run_git_args(
                    root,
                    args,
                    cancellation,
                    "check pulled dependency lockfiles",
                )
                .await
            {
                Ok(names) => return Ok((!names.is_empty()).then_some(InstallDue { head, stamped })),
                Err(error) if is_cancelled(&error) => return Err(error),
                Err(error) if !stamped => return Err(error),
                Err(_) => {}
            }
        }
        Ok(None)
    }

    async fn sync_local_branch(
        &self,
        branch: &str,
        is_main_checked_out: bool,
        cancellation: &CancellationToken,
        warnings: &mut Vec<String>,
    ) -> Result<(), RepositoryError> {
        let local_ref = format!("refs/heads/{branch}");
        let remote_ref = format!("refs/remotes/origin/{branch}");
        let local = self
            .rev_parse(&self.origin_config().main_clone, &local_ref, cancellation)
            .await?;
        if local.is_none() && branch == self.origin_config().trunk_branch {
            return Ok(());
        }
        let remote = self
            .rev_parse(&self.origin_config().main_clone, &remote_ref, cancellation)
            .await?;
        let Some(remote) = remote else {
            if branch != self.origin_config().trunk_branch {
                warnings.push(format!("keep_fresh: no origin/{branch} to track"));
            }
            return Ok(());
        };
        if let Some(local) = &local
            && !self
                .is_ancestor(
                    &self.origin_config().main_clone,
                    local,
                    &remote,
                    cancellation,
                )
                .await?
        {
            warnings.push(format!(
                "Local {branch} has diverged from origin/{branch}; not updating."
            ));
            return Ok(());
        }

        if is_main_checked_out {
            let status = self
                .run_git_args(
                    &self.origin_config().main_clone,
                    ["status", "--porcelain"].map(Into::into).to_vec(),
                    cancellation,
                    "check main clone before fast-forward",
                )
                .await?;
            if !status.is_empty() {
                if local.as_deref() != Some(remote.as_str()) {
                    warnings.push(format!(
                        "The main clone has local changes, so {branch} stays behind origin/{branch}; clean it to resume updates."
                    ));
                }
                return Ok(());
            }
            let result = self.fast_forward_checkout(branch, cancellation).await?;
            if !result {
                warnings.push(format!(
                    "could not fast-forward checked out branch {branch}"
                ));
            }
        } else {
            let result = self
                .run_git_status(
                    &self.origin_config().main_clone,
                    [
                        "branch",
                        "--force",
                        "--",
                        branch,
                        &format!("origin/{branch}"),
                    ],
                    cancellation,
                    "advance local branch",
                )
                .await?;
            if !result {
                warnings.push(format!(
                    "could not advance local branch {branch}; it may be checked out elsewhere"
                ));
            }
        }
        Ok(())
    }

    /// Freshen one clone only while no lifecycle operation owns it. A clone
    /// being removed is skipped rather than written into mid-deletion; a
    /// later sync catches up any clone that survives.
    async fn freshen_idle_rift_trunk(
        &self,
        worktree: &WorktreeRecord,
        target: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        let Some(_maintenance) = self
            .try_lock_clone_maintenance(worktree.target.slug())
            .await?
        else {
            return Ok(false);
        };
        // The inventory may predate a removal that finished while this task
        // waited. Never run Git (or create anything) at a vanished path.
        if !independent_clone_present(Path::new(&worktree.target.path)).await {
            return Ok(false);
        }
        self.freshen_rift_trunk(worktree, target, cancellation)
            .await
    }

    async fn freshen_rift_trunk(
        &self,
        worktree: &WorktreeRecord,
        target: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        let path = Path::new(&worktree.target.path);
        let ref_name = format!("refs/remotes/origin/{}", self.origin_config().trunk_branch);
        let have = self.rev_parse(path, &ref_name, cancellation).await?;
        if have.as_deref() == Some(target) {
            return Ok(false);
        }

        if !self.commit_exists(path, target, cancellation).await? {
            let temp_ref = format!(
                "refs/wt/fetch-origin/{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            );
            let fetch_result = self
                .run_git_args_on(
                    &self.origin_runner().bulk(),
                    path,
                    [
                        "fetch".into(),
                        "--no-tags".into(),
                        "--quiet".into(),
                        // A detached auto-gc would keep writing under the
                        // clone's `.git` after the maintenance lock is released.
                        "--no-auto-gc".into(),
                        self.origin_config().main_clone.as_os_str().to_owned(),
                        format!("+{ref_name}:{temp_ref}").into(),
                    ]
                    .to_vec(),
                    cancellation,
                    "fetch trunk object into Rift clone",
                )
                .await;
            let fetched = if fetch_result.is_ok() {
                self.rev_parse(path, &temp_ref, cancellation).await
            } else {
                Ok(None)
            };
            // Cleanup still runs if the caller cancels after the fetch command
            // has created this unique temporary ref.
            let cleanup = self
                .run_git_status(
                    path,
                    ["update-ref", "-d", &temp_ref],
                    &CancellationToken::new(),
                    "remove temporary Rift fetch ref",
                )
                .await;
            fetch_result?;
            let fetched = fetched?;
            let _ = cleanup;
            if fetched.as_deref() != Some(target) {
                return Ok(false);
            }
        }

        if let Some(have) = have.as_deref()
            && !self.is_ancestor(path, have, target, cancellation).await?
        {
            return Ok(false);
        }
        let expected = have.clone().unwrap_or_else(|| "".to_owned());
        self.run_git_args(
            path,
            [
                "update-ref".into(),
                ref_name.into(),
                target.into(),
                expected.into(),
            ]
            .to_vec(),
            cancellation,
            "advance Rift trunk ref",
        )
        .await?;
        Ok(true)
    }

    async fn rev_parse(
        &self,
        cwd: &Path,
        reference: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, RepositoryError> {
        self.run_git_optional(
            cwd,
            ["rev-parse", "--verify", reference],
            cancellation,
            "resolve Git ref",
        )
        .await
        .map(|value| value.map(|value| value.trim().to_owned()))
    }

    async fn is_ancestor(
        &self,
        cwd: &Path,
        ancestor: &str,
        descendant: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        let mut spec =
            CommandSpec::new("git").args(["merge-base", "--is-ancestor", ancestor, descendant]);
        spec.cwd = Some(cwd.to_path_buf());
        spec.env = common_git_env();
        spec.timeout = GIT_TIMEOUT;
        spec.output_limit = GIT_OUTPUT_LIMIT;
        let output = self
            .origin_runner()
            .run(spec, cancellation)
            .await
            .map_err(|source| RepositoryError::Process {
                operation: "check fast-forward ancestry",
                path: cwd.to_path_buf(),
                source,
            })?;
        if output.status.success() {
            Ok(true)
        } else if output.status.code() == Some(1) {
            Ok(false)
        } else {
            Err(RepositoryError::Parse {
                operation: "check fast-forward ancestry",
                path: cwd.to_path_buf(),
                message: output.stderr_text().trim().to_owned(),
            })
        }
    }

    async fn commit_exists(
        &self,
        cwd: &Path,
        commit: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        self.run_git_status(
            cwd,
            ["cat-file", "-e", &format!("{commit}^{{commit}}")],
            cancellation,
            "check commit availability",
        )
        .await
    }

    /// A merge killed partway leaves the work tree updated with the index and
    /// ref behind, and every later fetch then sees a dirty clone. A large
    /// update can outlast any fixed limit, so this one runs to completion.
    async fn fast_forward_checkout(
        &self,
        branch: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, RepositoryError> {
        let cwd = &self.origin_config().main_clone;
        let operation = "fast-forward checked out branch";
        let mut spec = CommandSpec::new("git").args([
            "merge",
            "--ff-only",
            "--quiet",
            "--",
            &format!("origin/{branch}"),
        ]);
        spec.cwd = Some(cwd.clone());
        spec.env = common_git_env();
        spec.timeout = FAST_FORWARD_ADMISSION;
        spec.output_limit = GIT_OUTPUT_LIMIT;
        let output = self
            .origin_runner()
            .bulk()
            .run_to_completion(spec, cancellation)
            .await
            .map_err(|source| RepositoryError::Process {
                operation,
                path: cwd.clone(),
                source,
            })?;
        Ok(output.status.success())
    }

    async fn run_git_status<const N: usize>(
        &self,
        cwd: &Path,
        args: [&str; N],
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<bool, RepositoryError> {
        let mut spec = CommandSpec::new("git").args(args);
        spec.cwd = Some(cwd.to_path_buf());
        spec.env = common_git_env();
        spec.timeout = GIT_TIMEOUT;
        spec.output_limit = GIT_OUTPUT_LIMIT;
        let output = self
            .origin_runner()
            .run(spec, cancellation)
            .await
            .map_err(|source| RepositoryError::Process {
                operation,
                path: cwd.to_path_buf(),
                source,
            })?;
        Ok(output.status.success())
    }

    async fn run_git_optional<const N: usize>(
        &self,
        cwd: &Path,
        args: [&str; N],
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<Option<String>, RepositoryError> {
        let mut spec = CommandSpec::new("git").args(args);
        spec.cwd = Some(cwd.to_path_buf());
        spec.env = common_git_env();
        spec.timeout = GIT_TIMEOUT;
        spec.output_limit = GIT_OUTPUT_LIMIT;
        let output = self
            .origin_runner()
            .run(spec, cancellation)
            .await
            .map_err(|source| RepositoryError::Process {
                operation,
                path: cwd.to_path_buf(),
                source,
            })?;
        if output.status.success() {
            Ok(Some(output.stdout_text()))
        } else if output.status.code() == Some(1) || output.status.code() == Some(128) {
            Ok(None)
        } else {
            Err(RepositoryError::Parse {
                operation,
                path: cwd.to_path_buf(),
                message: output.stderr_text().trim().to_owned(),
            })
        }
    }

    async fn run_git_args(
        &self,
        cwd: &Path,
        args: Vec<std::ffi::OsString>,
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<Vec<u8>, RepositoryError> {
        self.run_git_args_on(self.origin_runner(), cwd, args, cancellation, operation)
            .await
    }

    /// `runner` is the repository's own for short commands and its
    /// `bulk()` handle for a fetch, so a slow network transfer holds one of
    /// the few bulk slots and never a reader's.
    async fn run_git_args_on(
        &self,
        runner: &ProcessRunner,
        cwd: &Path,
        args: Vec<std::ffi::OsString>,
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<Vec<u8>, RepositoryError> {
        let mut spec = CommandSpec::new("git").args(args);
        spec.cwd = Some(cwd.to_path_buf());
        spec.env = common_git_env();
        spec.timeout = GIT_TIMEOUT;
        spec.output_limit = GIT_OUTPUT_LIMIT;
        let output =
            runner
                .run(spec, cancellation)
                .await
                .map_err(|source| RepositoryError::Process {
                    operation,
                    path: cwd.to_path_buf(),
                    source,
                })?;
        Ok(output
            .checked("git")
            .map_err(|source| RepositoryError::Process {
                operation,
                path: cwd.to_path_buf(),
                source,
            })?
            .stdout)
    }
}

fn clone_maintenance_key(slug: &str) -> String {
    format!("clone-maintenance-{slug}")
}

async fn independent_clone_present(path: &Path) -> bool {
    let git = tokio::fs::symlink_metadata(path.join(".git")).await;
    let marker = tokio::fs::symlink_metadata(path.join(".rift")).await;
    git.is_ok_and(|metadata| metadata.is_dir()) && marker.is_ok_and(|metadata| metadata.is_file())
}

#[cfg(test)]
mod tests {
    use super::{FetchOriginOptions, GitRepository};
    use crate::repository::{RepositoryConfig, RepositoryError, StageConfig};
    use std::{
        ffi::OsString,
        io::Read,
        path::{Path, PathBuf},
        process::Command,
        sync::mpsc,
        thread,
        time::Duration,
    };
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;
    use wt_platform::install::InstallPolicy;
    use wt_platform::lock::FileLock;
    use wt_platform::process::{CommandSpec, ProcessRunner};

    async fn git(runner: &ProcessRunner, cwd: &Path, args: &[&str]) -> String {
        let mut spec = CommandSpec::new("git");
        spec.args = args.iter().map(OsString::from).collect();
        spec.cwd = Some(cwd.to_path_buf());
        spec.timeout = Duration::from_secs(15);
        for (key, value) in [
            ("GIT_AUTHOR_NAME", "wt test"),
            ("GIT_AUTHOR_EMAIL", "wt-test@example.invalid"),
            ("GIT_COMMITTER_NAME", "wt test"),
            ("GIT_COMMITTER_EMAIL", "wt-test@example.invalid"),
        ] {
            spec.env.push((key.into(), Some(value.into())));
        }
        let output = runner
            .run(spec, &CancellationToken::new())
            .await
            .unwrap()
            .checked("git fixture")
            .unwrap();
        output.stdout_text().trim().to_owned()
    }

    async fn fixture() -> (TempDir, ProcessRunner, PathBuf, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("remote.git");
        let seed = root.path().join("seed");
        let main = root.path().join("main clone");
        let worktrees = root.path().join("worktrees");
        std::fs::create_dir_all(&seed).unwrap();
        std::fs::create_dir_all(&worktrees).unwrap();
        let runner = ProcessRunner::default();
        git(
            &runner,
            root.path(),
            &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        )
        .await;
        git(&runner, &seed, &["init", "-b", "main"]).await;
        git(&runner, &seed, &["config", "user.name", "wt test"]).await;
        git(
            &runner,
            &seed,
            &["config", "user.email", "wt-test@example.invalid"],
        )
        .await;
        std::fs::write(seed.join("base.txt"), "base\n").unwrap();
        git(&runner, &seed, &["add", "base.txt"]).await;
        git(&runner, &seed, &["commit", "-m", "initial"]).await;
        git(
            &runner,
            &seed,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        )
        .await;
        git(&runner, &seed, &["push", "-u", "origin", "main"]).await;
        git(
            &runner,
            root.path(),
            &["clone", bare.to_str().unwrap(), main.to_str().unwrap()],
        )
        .await;
        git(&runner, &main, &["config", "user.name", "wt test"]).await;
        git(
            &runner,
            &main,
            &["config", "user.email", "wt-test@example.invalid"],
        )
        .await;
        (root, runner, seed, main, worktrees)
    }

    fn repository(main: &Path, worktrees: &Path) -> GitRepository {
        GitRepository::new(
            RepositoryConfig {
                main_clone: main.to_path_buf(),
                worktree_root: worktrees.to_path_buf(),
                trunk_branch: "main".into(),
                stage: StageConfig {
                    prefix: "stage".into(),
                    issue_id_pattern: r"([A-Z]+-\d+)".into(),
                },
            },
            ProcessRunner::default(),
        )
    }

    async fn commit_file(
        runner: &ProcessRunner,
        repo: &Path,
        name: &str,
        contents: &str,
        message: &str,
    ) -> String {
        std::fs::write(repo.join(name), contents).unwrap();
        git(runner, repo, &["add", name]).await;
        git(runner, repo, &["commit", "-m", message]).await;
        git(runner, repo, &["rev-parse", "HEAD"]).await
    }

    #[tokio::test]
    async fn generated_cleanup_and_dependency_sync_follow_the_pulled_lockfile() {
        let (_root, runner, seed, main, worktrees) = fixture().await;
        commit_file(
            &runner,
            &seed,
            "generated.ts",
            "generated one\n",
            "generated",
        )
        .await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        let repo = repository(&main, &worktrees);
        let cancel = CancellationToken::new();
        repo.fetch_origin(Default::default(), &cancel)
            .await
            .unwrap();
        std::fs::write(main.join("generated.ts"), "local regenerated\n").unwrap();
        // The package lockfile did not exist when the fetch began.
        commit_file(
            &runner,
            &seed,
            "pnpm-lock.yaml",
            "new lock\n",
            "dependencies",
        )
        .await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        let options = FetchOriginOptions {
            auto_regen_paths: vec!["generated.ts".into()],
            sync_install: Some(InstallPolicy {
                command: Some(
                    "printf '%3145728s' ''; cat pnpm-lock.yaml >> .git/installed-locks".into(),
                ),
                shell: "sh".into(),
            }),
            ..Default::default()
        };
        let report = repo.fetch_origin(options.clone(), &cancel).await.unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_ne!(report.before_main_head, report.after_main_head);
        assert_eq!(
            std::fs::read_to_string(main.join("generated.ts")).unwrap(),
            "generated one\n"
        );
        assert_eq!(
            std::fs::read_to_string(main.join(".git/installed-locks")).unwrap(),
            "new lock\n"
        );
        // Ordinary code updates and repeated refreshes cannot reinstall dependencies.
        commit_file(&runner, &seed, "code.rs", "code\n", "code only").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        repo.fetch_origin(options.clone(), &cancel).await.unwrap();
        repo.fetch_origin(options, &cancel).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(main.join(".git/installed-locks")).unwrap(),
            "new lock\n"
        );
        commit_file(
            &runner,
            &seed,
            "pnpm-lock.yaml",
            "second lock\n",
            "more dependencies",
        )
        .await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        let failed = repo
            .fetch_origin(
                FetchOriginOptions {
                    sync_install: Some(InstallPolicy {
                        command: Some("exit 23".into()),
                        shell: "sh".into(),
                    }),
                    ..Default::default()
                },
                &cancel,
            )
            .await
            .unwrap();
        assert!(
            failed
                .warnings
                .iter()
                .any(|warning| warning.contains("dependency sync failed"))
        );
        assert_eq!(
            failed.after_main_head.unwrap(),
            git(&runner, &seed, &["rev-parse", "HEAD"]).await
        );
    }

    #[tokio::test]
    async fn a_lockfile_pull_whose_install_did_not_happen_is_installed_by_the_next_pull() {
        let (_root, runner, seed, main, worktrees) = fixture().await;
        let repo = repository(&main, &worktrees);
        let cancel = CancellationToken::new();
        let install = |command: &str| FetchOriginOptions {
            sync_install: Some(InstallPolicy {
                command: Some(command.into()),
                shell: "sh".into(),
            }),
            ..Default::default()
        };
        let record = "cat pnpm-lock.yaml >> .git/installed-locks";
        commit_file(&runner, &seed, "pnpm-lock.yaml", "one\n", "dependencies").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        repo.fetch_origin(install(record), &cancel).await.unwrap();
        commit_file(&runner, &seed, "pnpm-lock.yaml", "two\n", "more").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        let failed = repo
            .fetch_origin(install("exit 23"), &cancel)
            .await
            .unwrap();
        assert!(!failed.warnings.is_empty());
        // This pull changes no lockfile, but the clone's packages are still
        // those of the first one.
        commit_file(&runner, &seed, "code.rs", "code\n", "code only").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        repo.fetch_origin(install(record), &cancel).await.unwrap();
        commit_file(&runner, &seed, "more.rs", "code\n", "code again").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        repo.fetch_origin(install(record), &cancel).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(main.join(".git/installed-locks")).unwrap(),
            "one\ntwo\n"
        );
    }

    #[tokio::test]
    async fn an_install_cancelled_while_queued_for_a_slot_is_still_owed_by_the_next_pull() {
        use wt_platform::lock::{DEPENDENCY_INSTALL_POOL, DEPENDENCY_INSTALL_SLOTS, MachineSlots};
        let (root, runner, seed, main, worktrees) = fixture().await;
        let repo = repository(&main, &worktrees);
        repo.fetch_origin(Default::default(), &CancellationToken::new())
            .await
            .unwrap();
        let before = git(&runner, &main, &["rev-parse", "HEAD"]).await;
        commit_file(&runner, &seed, "pnpm-lock.yaml", "lock\n", "dependencies").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        let locks = root.path().join("configured-locks");
        let others = MachineSlots::open(&locks, DEPENDENCY_INSTALL_POOL, DEPENDENCY_INSTALL_SLOTS)
            .await
            .unwrap();
        let held: Vec<_> = (0..DEPENDENCY_INSTALL_SLOTS.get())
            .map(|_| others.try_acquire().unwrap().unwrap())
            .collect();
        let options = FetchOriginOptions {
            sync_install: Some(InstallPolicy {
                command: Some("cat pnpm-lock.yaml >> .git/installed-locks".into()),
                shell: "sh".into(),
            }),
            install_lock_dir: Some(locks.clone()),
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let queued = tokio::spawn({
            let (repo, cancel, options) = (repo.clone(), cancel.clone(), options.clone());
            async move { repo.fetch_origin(options, &cancel).await }
        });
        // What the clone owes is on record while the install only queues.
        let stamp = repo
            .repository_lock_dir()
            .await
            .join(super::MAIN_INSTALL_STAMP);
        tokio::time::timeout(Duration::from_secs(20), async {
            while std::fs::read_to_string(&stamp).ok().as_deref() != Some(before.trim()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the pre-pull commit is stamped before the wait for a slot");
        assert!(!queued.is_finished());
        cancel.cancel();
        assert!(matches!(
            queued.await.unwrap(),
            Err(RepositoryError::Cancelled)
        ));
        assert!(!main.join(".git/installed-locks").exists());
        drop(held);
        // This pull changes no lockfile.
        commit_file(&runner, &seed, "code.rs", "code\n", "code only").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        repo.fetch_origin(options, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(main.join(".git/installed-locks")).unwrap(),
            "lock\n"
        );
    }

    #[tokio::test]
    async fn the_main_clone_install_takes_a_shared_install_slot_and_not_the_fetch_lock() {
        let (root, runner, seed, main, worktrees) = fixture().await;
        let repo = repository(&main, &worktrees);
        let cancel = CancellationToken::new();
        repo.fetch_origin(Default::default(), &cancel)
            .await
            .unwrap();
        commit_file(
            &runner,
            &seed,
            "pnpm-lock.yaml",
            "lock
",
            "dependencies",
        )
        .await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        use wt_platform::lock::{DEPENDENCY_INSTALL_POOL, DEPENDENCY_INSTALL_SLOTS, MachineSlots};
        let locks = root.path().join("configured-locks");
        let pool = || MachineSlots::open(&locks, DEPENDENCY_INSTALL_POOL, DEPENDENCY_INSTALL_SLOTS);
        // Other installs hold every slot: this one waits, not holding the
        // fetch lock while it does.
        let others = pool().await.unwrap();
        let mut held: Vec<_> = (0..DEPENDENCY_INSTALL_SLOTS.get())
            .map(|_| others.try_acquire().unwrap().unwrap())
            .collect();
        let options = FetchOriginOptions {
            sync_install: Some(InstallPolicy {
                command: Some("touch .git/install-started; sleep 1; touch .git/installed".into()),
                shell: "sh".into(),
            }),
            install_lock_dir: Some(locks.clone()),
            ..Default::default()
        };
        let fetching = tokio::spawn({
            let (repo, cancel, options) = (repo.clone(), cancel.clone(), options.clone());
            async move { repo.fetch_origin(options, &cancel).await }
        });
        let lock_dir = repo.repository_lock_dir().await;
        let fetch_lock_free = || async {
            FileLock::try_acquire(&lock_dir, "fetch-origin", "test")
                .await
                .unwrap()
                .is_some()
        };
        tokio::time::timeout(Duration::from_secs(20), async {
            while !(main.join("pnpm-lock.yaml").exists() && fetch_lock_free().await) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the fetch lock is released before the install's turn comes");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!main.join(".git/install-started").exists());
        assert!(!fetching.is_finished());
        // Queued for a slot, it does not hold off a copy of the clone.
        drop(
            tokio::time::timeout(
                Duration::from_secs(5),
                repo.lock_main_dependency_sync(&cancel),
            )
            .await
            .expect("the install lock is free while the install waits for a slot")
            .unwrap(),
        );
        held.pop();
        tokio::time::timeout(Duration::from_secs(20), async {
            while !main.join(".git/install-started").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the install starts once a slot frees");
        // It holds the freed slot, and another fetch is not kept waiting.
        assert!(others.try_acquire().unwrap().is_none());
        assert!(fetch_lock_free().await);
        let install_lock_free = || async {
            FileLock::try_acquire(&lock_dir, super::MAIN_INSTALL_LOCK, "test")
                .await
                .unwrap()
                .is_some()
        };
        assert!(!install_lock_free().await);
        // Cancelled mid-install, it gives back the slot and the lock.
        cancel.cancel();
        assert!(matches!(
            fetching.await.unwrap(),
            Err(RepositoryError::Cancelled)
        ));
        assert!(!main.join(".git/installed").exists());
        assert!(install_lock_free().await);
        assert!(others.try_acquire().unwrap().is_some());
        // Nothing was installed, so the next pull that moves the clone
        // still owes it, though that pull changes no lockfile.
        commit_file(&runner, &seed, "code.rs", "code\n", "code only").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        let report = repo
            .fetch_origin(options, &CancellationToken::new())
            .await
            .unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(main.join(".git/installed").exists());
    }

    #[tokio::test]
    async fn generated_cleanup_keeps_staged_work_and_refuses_root_pathspecs() {
        let (_root, runner, seed, main, worktrees) = fixture().await;
        let repo = repository(&main, &worktrees);
        let cancel = CancellationToken::new();
        std::fs::write(main.join("base.txt"), "staged edit\n").unwrap();
        git(&runner, &main, &["add", "base.txt"]).await;
        std::fs::write(main.join("base.txt"), "working edit\n").unwrap();
        commit_file(&runner, &seed, "remote.txt", "new\n", "remote").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        let report = repo
            .fetch_origin(
                FetchOriginOptions {
                    auto_regen_paths: vec!["base.txt".into(), "./".into(), "../".into()],
                    ..Default::default()
                },
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(report.before_main_head, report.after_main_head);
        assert_eq!(report.warnings.len(), 4, "{:?}", report.warnings);
        assert!(report.warnings[3].contains("stays behind origin/main"));
        assert_eq!(
            std::fs::read_to_string(main.join("base.txt")).unwrap(),
            "working edit\n"
        );
        assert_eq!(
            git(&runner, &main, &["show", ":base.txt"]).await,
            "staged edit"
        );
    }

    #[tokio::test]
    async fn a_dirty_main_clone_says_it_is_falling_behind_only_when_it_is() {
        let (_root, runner, seed, main, worktrees) = fixture().await;
        std::fs::write(main.join("scratch.txt"), "local\n").unwrap();
        let repository = repository(&main, &worktrees);
        let cancellation = CancellationToken::new();
        let fetch = || repository.fetch_origin(FetchOriginOptions::default(), &cancellation);
        assert!(fetch().await.unwrap().warnings.is_empty());

        commit_file(&runner, &seed, "next.txt", "next\n", "advance trunk").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        let report = fetch().await.unwrap();
        assert_eq!(report.before_main_head, report.after_main_head);
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0]
                .to_string()
                .contains("stays behind origin/main")
        );
    }

    #[tokio::test]
    async fn fetch_origin_fast_forwards_trunk_and_creates_keep_fresh() {
        let (_root, runner, seed, main, worktrees) = fixture().await;
        git(&runner, &seed, &["checkout", "-b", "topic"]).await;
        commit_file(&runner, &seed, "topic.txt", "topic\n", "topic").await;
        git(&runner, &seed, &["push", "-u", "origin", "topic"]).await;
        git(&runner, &seed, &["checkout", "main"]).await;
        let expected_main =
            commit_file(&runner, &seed, "next.txt", "next\n", "advance trunk").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;

        let repository = repository(&main, &worktrees);
        let mut calls = Vec::new();
        for _ in 0..6 {
            let repository = repository.clone();
            calls.push(tokio::spawn(async move {
                repository
                    .fetch_origin(
                        FetchOriginOptions {
                            keep_fresh: vec!["topic".into()],
                            ..Default::default()
                        },
                        &CancellationToken::new(),
                    )
                    .await
            }));
        }
        let mut reports = Vec::new();
        for call in calls {
            reports.push(call.await.unwrap().unwrap());
        }
        let report = reports.first().unwrap().clone();
        assert!(reports.iter().all(|value| value == &report));
        assert_ne!(report.before_main_head, report.after_main_head);
        assert_eq!(
            git(&runner, &main, &["rev-parse", "HEAD"]).await,
            expected_main
        );
        assert_eq!(
            git(&runner, &main, &["rev-parse", "topic"]).await,
            git(&runner, &main, &["rev-parse", "origin/topic"]).await,
        );
    }

    #[tokio::test]
    async fn fetch_origin_preserves_diverged_and_dirty_checked_out_heads() {
        let (_root, runner, seed, main, worktrees) = fixture().await;
        let local_head = commit_file(&runner, &main, "local.txt", "local\n", "local-only").await;
        commit_file(&runner, &seed, "remote.txt", "remote\n", "remote-only").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        let report = repository(&main, &worktrees)
            .fetch_origin(FetchOriginOptions::default(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            git(&runner, &main, &["rev-parse", "HEAD"]).await,
            local_head
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("diverged"))
        );

        // Reset to a clean ancestor, then verify a dirty checked-out trunk is also left alone.
        git(&runner, &main, &["reset", "--hard", "origin/main"]).await;
        let before_dirty_fetch = git(&runner, &main, &["rev-parse", "HEAD"]).await;
        std::fs::write(main.join("uncommitted.txt"), "keep me\n").unwrap();
        commit_file(
            &runner,
            &seed,
            "third.txt",
            "third\n",
            "third remote commit",
        )
        .await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        repository(&main, &worktrees)
            .fetch_origin(FetchOriginOptions::default(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            git(&runner, &main, &["rev-parse", "HEAD"]).await,
            before_dirty_fetch
        );
        assert_eq!(
            std::fs::read_to_string(main.join("uncommitted.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[tokio::test]
    async fn fetch_origin_does_not_force_move_branch_checked_out_in_another_worktree() {
        let (_root, runner, seed, main, worktrees) = fixture().await;
        git(&runner, &seed, &["checkout", "-b", "topic"]).await;
        commit_file(&runner, &seed, "topic.txt", "first\n", "topic one").await;
        git(&runner, &seed, &["push", "-u", "origin", "topic"]).await;
        git(&runner, &seed, &["checkout", "main"]).await;
        git(&runner, &main, &["fetch", "origin"]).await;
        git(&runner, &main, &["branch", "topic", "origin/topic"]).await;
        let checkout = worktrees.join("checked-out-topic");
        git(
            &runner,
            &main,
            &["worktree", "add", checkout.to_str().unwrap(), "topic"],
        )
        .await;
        let checked_out_before = git(&runner, &checkout, &["rev-parse", "HEAD"]).await;

        commit_file(&runner, &seed, "topic2.txt", "second\n", "topic two").await;
        git(&runner, &seed, &["push", "origin", "topic"]).await;
        let report = repository(&main, &worktrees)
            .fetch_origin(
                FetchOriginOptions {
                    keep_fresh: vec!["topic".into()],
                    ..Default::default()
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            git(&runner, &main, &["rev-parse", "topic"]).await,
            checked_out_before
        );
        assert_eq!(
            git(&runner, &checkout, &["rev-parse", "HEAD"]).await,
            checked_out_before
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("checked out elsewhere"))
        );
    }

    async fn push_late_commit(runner: &ProcessRunner, seed: &Path) -> String {
        let pushed = commit_file(runner, seed, "late.txt", "late\n", "late").await;
        git(runner, seed, &["push", "origin", "main"]).await;
        pushed
    }

    async fn fetch_stamp(main: &Path) -> PathBuf {
        let (_, common_dir) = super::super::repository::git_metadata_paths(main).await;
        common_dir
            .unwrap()
            .join("wt-locks")
            .join(super::FETCH_STAMP)
    }

    #[tokio::test]
    async fn a_waiter_skips_the_fetch_that_started_after_its_request() {
        let (_root, runner, seed, main, worktrees) = fixture().await;
        let cancel = CancellationToken::new();
        let requested = super::wall_clock_nanos();
        // Another process's fetch, begun while this request waited for the lock.
        repository(&main, &worktrees)
            .fetch_origin(FetchOriginOptions::default(), &cancel)
            .await
            .unwrap();
        let fetched = git(&runner, &main, &["rev-parse", "origin/main"]).await;
        let pushed = push_late_commit(&runner, &seed).await;

        repository(&main, &worktrees)
            .fetch_origin_requested_at(FetchOriginOptions::default(), requested, &cancel)
            .await
            .unwrap();

        assert_ne!(fetched, pushed);
        assert_eq!(
            git(&runner, &main, &["rev-parse", "origin/main"]).await,
            fetched,
            "a covered request must not fetch again"
        );
    }

    #[tokio::test]
    async fn a_request_newer_than_the_stamp_or_a_future_stamp_fetches() {
        let (_root, runner, seed, main, worktrees) = fixture().await;
        let cancel = CancellationToken::new();
        let repository = repository(&main, &worktrees);
        repository
            .fetch_origin(FetchOriginOptions::default(), &cancel)
            .await
            .unwrap();
        let stamp = fetch_stamp(&main).await;
        let first = std::fs::read_to_string(&stamp).unwrap();
        let pushed = push_late_commit(&runner, &seed).await;

        repository
            .fetch_origin(FetchOriginOptions::default(), &cancel)
            .await
            .unwrap();

        assert_eq!(
            git(&runner, &main, &["rev-parse", "origin/main"]).await,
            pushed
        );
        let second = std::fs::read_to_string(&stamp).unwrap();
        assert!(second.parse::<u128>().unwrap() > first.parse::<u128>().unwrap());

        let future = super::wall_clock_nanos().unwrap() + 3_600_000_000_000;
        std::fs::write(&stamp, future.to_string()).unwrap();
        let later = commit_file(&runner, &seed, "later.txt", "later\n", "later").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        repository
            .fetch_origin_requested_at(FetchOriginOptions::default(), second.parse().ok(), &cancel)
            .await
            .unwrap();
        assert_eq!(
            git(&runner, &main, &["rev-parse", "origin/main"]).await,
            later,
            "a stamp from the future must not stand in for a fetch"
        );
    }

    #[tokio::test]
    async fn a_failed_fetch_leaves_no_stamp() {
        let (root, runner, _seed, main, worktrees) = fixture().await;
        let missing = root.path().join("missing.git");
        git(
            &runner,
            &main,
            &["remote", "set-url", "origin", missing.to_str().unwrap()],
        )
        .await;

        assert!(
            repository(&main, &worktrees)
                .fetch_origin(FetchOriginOptions::default(), &CancellationToken::new())
                .await
                .is_err()
        );

        assert!(!fetch_stamp(&main).await.exists());
    }

    #[tokio::test]
    async fn a_refs_only_fetch_reuses_a_recent_one_and_leaves_maintenance_to_its_owner() {
        let (_root, runner, seed, main, worktrees) = fixture().await;
        let repository = repository(&main, &worktrees);
        let cancel = CancellationToken::new();
        let local_main = git(&runner, &main, &["rev-parse", "HEAD"]).await;
        commit_file(&runner, &seed, "next.txt", "next\n", "next remote commit").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        let pushed = git(&runner, &seed, &["rev-parse", "HEAD"]).await;

        assert_eq!(
            repository
                .fetch_origin_refs(Duration::from_secs(30), &cancel)
                .await
                .unwrap(),
            super::FetchedRefs::Fetched
        );
        assert_eq!(
            git(&runner, &main, &["rev-parse", "origin/main"]).await,
            pushed
        );
        // Only the remote-tracking ref moved: the checkout is maintenance's.
        assert_eq!(
            git(&runner, &main, &["rev-parse", "HEAD"]).await,
            local_main
        );

        commit_file(
            &runner,
            &seed,
            "later.txt",
            "later\n",
            "later remote commit",
        )
        .await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        assert_eq!(
            repository
                .fetch_origin_refs(Duration::from_secs(30), &cancel)
                .await
                .unwrap(),
            super::FetchedRefs::Reused
        );
        assert_eq!(
            git(&runner, &main, &["rev-parse", "origin/main"]).await,
            pushed
        );
        // Maintenance runs over the refs as fetched, without another fetch.
        repository
            .maintain_fetched_refs(FetchOriginOptions::default(), &cancel)
            .await
            .unwrap();
        assert_eq!(git(&runner, &main, &["rev-parse", "HEAD"]).await, pushed);
        assert_eq!(
            git(&runner, &main, &["rev-parse", "origin/main"]).await,
            pushed
        );

        // No window: only a fetch that starts after the call will do.
        assert_eq!(
            repository
                .fetch_origin_refs(Duration::ZERO, &cancel)
                .await
                .unwrap(),
            super::FetchedRefs::Fetched
        );
        assert_ne!(
            git(&runner, &main, &["rev-parse", "origin/main"]).await,
            pushed
        );
    }

    #[tokio::test]
    async fn fetch_origin_returns_cancellation_instead_of_a_success_placeholder() {
        let (_root, _runner, _seed, main, worktrees) = fixture().await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            repository(&main, &worktrees)
                .fetch_origin(FetchOriginOptions::default(), &cancel)
                .await,
            Err(super::RepositoryError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn cancel_waits_for_fetch_child_reap_and_releases_repository_lock() {
        let (_root, runner, _seed, main, worktrees) = fixture().await;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        git(
            &runner,
            &main,
            &[
                "remote",
                "set-url",
                "origin",
                &format!("http://127.0.0.1:{port}/slow.git"),
            ],
        )
        .await;

        let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
        let (closed_tx, closed_rx) = mpsc::sync_channel(1);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            accepted_tx.send(()).unwrap();
            let mut buffer = [0; 4096];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let _ = closed_tx.send(());
        });

        let repository = repository(&main, &worktrees);
        let cancellation = CancellationToken::new();
        let task_cancel = cancellation.clone();
        let fetch = tokio::spawn(async move {
            repository
                .fetch_origin(FetchOriginOptions::default(), &task_cancel)
                .await
        });
        tokio::task::spawn_blocking(move || {
            accepted_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("git fetch should reach the delayed loopback server")
        })
        .await
        .unwrap();

        let child_pid = tokio::task::spawn_blocking(|| {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                let output = Command::new("ps")
                    .args(["-Ao", "pid=,ppid=,command="])
                    .output()
                    .expect("ps should be available in the test environment");
                let text = String::from_utf8_lossy(&output.stdout);
                for line in text.lines() {
                    let mut fields = line.split_whitespace();
                    let (Some(pid), Some(parent)) = (fields.next(), fields.next()) else {
                        continue;
                    };
                    if parent == std::process::id().to_string()
                        && line.contains("fetch origin --prune")
                        && let Ok(pid) = pid.parse::<u32>()
                    {
                        return Some(pid);
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            None
        })
        .await
        .unwrap();

        cancellation.cancel();
        assert!(matches!(
            fetch.await.unwrap(),
            Err(RepositoryError::Cancelled)
        ));
        let child_pid = child_pid.expect("capture the live git fetch PID before cancellation");
        let process_check = Command::new("ps")
            .args(["-p", &child_pid.to_string()])
            .output()
            .unwrap()
            .stdout;
        let still_alive = String::from_utf8_lossy(&process_check)
            .lines()
            .skip(1)
            .any(|line| !line.trim().is_empty());
        assert!(
            !still_alive,
            "fetch_origin must return after reaping git PID {child_pid}"
        );

        let (_, common_dir) = super::super::repository::git_metadata_paths(&main).await;
        let lock_dir = common_dir.unwrap().join("wt-locks");
        let lock = FileLock::try_acquire(&lock_dir, "fetch-origin", "test fetch lock")
            .await
            .unwrap()
            .expect("fetch lock must be released when cancellation returns");
        drop(lock);

        tokio::task::spawn_blocking(move || {
            closed_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("git fetch must close its loopback connection before returning")
        })
        .await
        .unwrap();
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rift_trunk_ref_fetches_missing_objects_and_never_rewinds_ahead_ref() {
        let (_root, runner, seed, main, worktrees) = fixture().await;
        let initial = git(&runner, &main, &["rev-parse", "origin/main"]).await;
        let rift = worktrees.join("rift-one");
        git(
            &runner,
            root_cwd(&main),
            &["clone", main.to_str().unwrap(), rift.to_str().unwrap()],
        )
        .await;
        std::fs::write(rift.join(".rift"), "fixture\n").unwrap();

        commit_file(&runner, &seed, "new.txt", "new\n", "remote update").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        let repository = repository(&main, &worktrees);
        let first = repository
            .fetch_origin(FetchOriginOptions::default(), &CancellationToken::new())
            .await
            .unwrap();
        let fetched = git(&runner, &rift, &["rev-parse", "origin/main"]).await;
        assert_ne!(fetched, initial);
        assert_eq!(first.rift_refs_updated, 1);

        // The tracking ref can be absent even when the target object is
        // already present. An empty old value must create it without force.
        git(
            &runner,
            &rift,
            &["update-ref", "-d", "refs/remotes/origin/main"],
        )
        .await;
        repository
            .fetch_origin(FetchOriginOptions::default(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            git(&runner, &rift, &["rev-parse", "origin/main"]).await,
            fetched
        );

        // Put the Rift tracking ref on a local-only descendant. A later fetch
        // whose main-clone target is behind it must keep the ahead value.
        let ahead = commit_file(&runner, &rift, "rift.txt", "rift\n", "rift ahead").await;
        git(
            &runner,
            &rift,
            &["update-ref", "refs/remotes/origin/main", &ahead],
        )
        .await;
        let behind_main_target = git(&runner, &main, &["rev-parse", "origin/main"]).await;
        git(&runner, &seed, &["reset", "--hard", &initial]).await;
        git(&runner, &seed, &["push", "--force", "origin", "main"]).await;
        repository
            .fetch_origin(FetchOriginOptions::default(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            git(&runner, &rift, &["rev-parse", "origin/main"]).await,
            ahead
        );
        assert_ne!(behind_main_target, ahead);
    }

    #[tokio::test]
    async fn origin_sync_skips_a_clone_owned_by_removal_and_a_vanished_clone() {
        let (_root, runner, seed, main, worktrees) = fixture().await;
        let initial = git(&runner, &main, &["rev-parse", "origin/main"]).await;
        let rift = worktrees.join("rift-busy");
        git(
            &runner,
            root_cwd(&main),
            &["clone", main.to_str().unwrap(), rift.to_str().unwrap()],
        )
        .await;
        std::fs::write(rift.join(".rift"), "fixture\n").unwrap();
        commit_file(&runner, &seed, "new.txt", "new\n", "remote update").await;
        git(&runner, &seed, &["push", "origin", "main"]).await;
        let repository = repository(&main, &worktrees);

        let held = repository
            .lock_clone_maintenance("rift-busy", &CancellationToken::new())
            .await
            .unwrap();
        let skipped = repository
            .fetch_origin(FetchOriginOptions::default(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(skipped.rift_refs_updated, 0);
        assert!(skipped.warnings.is_empty(), "{:?}", skipped.warnings);
        assert_eq!(
            git(&runner, &rift, &["rev-parse", "origin/main"]).await,
            initial
        );

        // A quarantined clone is not inventory, so it is never maintained.
        drop(held);
        let quarantine = worktrees.join(format!(
            "{}rift-busy-1",
            GitRepository::REMOVAL_QUARANTINE_PREFIX
        ));
        std::fs::rename(&rift, &quarantine).unwrap();
        let moved = repository
            .fetch_origin(FetchOriginOptions::default(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(moved.rift_refs_updated, 0);
        assert!(!rift.exists(), "maintenance must not recreate the old path");
        assert_eq!(
            git(&runner, &quarantine, &["rev-parse", "origin/main"]).await,
            initial
        );

        std::fs::rename(&quarantine, &rift).unwrap();
        let resumed = repository
            .fetch_origin(FetchOriginOptions::default(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(resumed.rift_refs_updated, 1);
        assert_ne!(
            git(&runner, &rift, &["rev-parse", "origin/main"]).await,
            initial
        );
    }

    fn root_cwd(path: &Path) -> &Path {
        path.parent().expect("fixture main clone has parent")
    }
}
