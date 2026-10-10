//! Fetch `origin` and safely advance local base references.

use std::{
    collections::BTreeSet,
    path::{Component, Path},
    sync::Arc,
    time::Duration,
};

use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use wt_platform::install::InstallPolicy;
use wt_platform::lock::{FileLock, LockError};
use wt_platform::process::{CommandSpec, ProcessError};

use crate::{
    repository::{
        GitRepository, RepositoryError, RepositoryKind, WorktreeRecord, git_metadata_paths,
    },
    status::common_git_env,
};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct FetchOriginOptions {
    /// Local branches to keep in sync with their `origin/<branch>` refs.
    pub keep_fresh: Vec<String>,
    /// Explicitly configured generated paths; staged edits are never discarded.
    pub auto_regen_paths: Vec<String>,
    /// Frozen dependency maintenance runs under the fetch lock after a pull.
    pub sync_install: Option<InstallPolicy>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchOriginReport {
    pub before_main_head: Option<String>,
    pub after_main_head: Option<String>,
    pub warnings: Vec<String>,
    pub rift_refs_updated: usize,
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

    async fn fetch_origin_locked(
        &self,
        options: FetchOriginOptions,
        cancellation: &CancellationToken,
    ) -> Result<FetchOriginReport, RepositoryError> {
        let (git_dir, common_dir) = git_metadata_paths(&self.origin_config().main_clone).await;
        let lock_dir = common_dir
            .or(git_dir)
            .unwrap_or_else(|| self.origin_config().main_clone.join(".git"));
        let lock_dir = lock_dir.join("wt-locks");
        let _lock = FileLock::acquire(&lock_dir, "fetch-origin", "fetch origin", cancellation)
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
            })?;

        let before_main_head = self
            .rev_parse(&self.origin_config().main_clone, "HEAD", cancellation)
            .await?;
        self.run_git_args(
            &self.origin_config().main_clone,
            ["fetch", "origin", "--prune"].map(Into::into).to_vec(),
            cancellation,
            "fetch origin",
        )
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
                                        .freshen_rift_trunk(&worktree, &tip, cancellation)
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
            self.sync_main_dependencies(policy, before, after, cancellation, &mut warnings)
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

    async fn sync_main_dependencies(
        &self,
        policy: &InstallPolicy,
        before: &str,
        after: &str,
        cancellation: &CancellationToken,
        warnings: &mut Vec<String>,
    ) -> Result<(), RepositoryError> {
        let root = &self.origin_config().main_clone;
        let plan = match policy.resolve(root, true).await {
            Ok(Some(plan)) => plan,
            Ok(None) => return Ok(()),
            Err(error) => {
                warnings.push(format!(
                    "could not detect main clone dependency manager: {error}"
                ));
                return Ok(());
            }
        };
        let mut args = vec![
            "diff".into(),
            "--name-only".into(),
            before.into(),
            after.into(),
            "--".into(),
        ];
        args.extend(plan.gate_lockfiles.iter().map(|file| (*file).into()));
        if self
            .run_git_args(
                root,
                args,
                cancellation,
                "check pulled dependency lockfiles",
            )
            .await?
            .is_empty()
        {
            return Ok(());
        }
        let result = self
            .origin_runner()
            // Package managers can be verbose. Keep a bounded diagnostic
            // capture while draining the rest, never terminate an install just
            // because it printed more than the capture budget.
            .run_streaming(plan.spec, cancellation, |_, _| {})
            .await
            .and_then(|output| output.checked("package manager").map(|_| ()));
        match result {
            Ok(()) => {}
            Err(ProcessError::Cancelled { .. }) => return Err(RepositoryError::Cancelled),
            Err(error) => warnings.push(format!(
                "main clone dependency sync failed ({}): {error}",
                plan.label
            )),
        }
        Ok(())
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
                return Ok(());
            }
            let result = self
                .run_git_status(
                    &self.origin_config().main_clone,
                    [
                        "merge",
                        "--ff-only",
                        "--quiet",
                        "--",
                        &format!("origin/{branch}"),
                    ],
                    cancellation,
                    "fast-forward checked out branch",
                )
                .await?;
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
                .run_git_args(
                    path,
                    [
                        "fetch".into(),
                        "--no-tags".into(),
                        "--quiet".into(),
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
        assert_eq!(report.warnings.len(), 3);
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

    fn root_cwd(path: &Path) -> &Path {
        path.parent().expect("fixture main clone has parent")
    }
}
