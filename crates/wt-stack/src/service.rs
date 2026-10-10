use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_github::{GithubClient, GithubError};
use wt_platform::{
    lock::{FileLock, LockError},
    process::{ProcessError, ProcessRunner},
};
use wt_store::{RepositoryIdentity, Store, StoreError};
use wt_vcs::{GitRepository, RepositoryError};

use crate::{
    backup::{PruneBackupsResult, prune_backups},
    chain::{RestackChain, resolve_chain},
    git::git_sha,
    replay::{ReplayContext, replay_chain},
};

#[derive(Clone, Debug)]
pub struct StateConfig {
    pub path: PathBuf,
    pub identity: RepositoryIdentity,
}

#[derive(Clone, Debug)]
pub struct StackConfig {
    pub main_clone: PathBuf,
    pub lock_dir: PathBuf,
    pub trunk_branch: String,
    pub fetch_options: wt_vcs::FetchOriginOptions,
    pub state: StateConfig,
}

#[derive(Clone, Debug, Default)]
pub struct RestackOptions {
    pub onto: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestackOutcome {
    Complete {
        replayed: usize,
        total: usize,
    },
    Conflict {
        branch: String,
        backup_ref: String,
        error: String,
    },
    Refused {
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StackEvent {
    Log(String),
    Attention(String),
}

#[derive(Debug, Error)]
pub enum StackError {
    #[error("invalid stack operation: {0}")]
    Invalid(String),
    #[error("Git operation failed: {0}")]
    Process(#[from] ProcessError),
    #[error("worktree inventory failed: {0}")]
    Repository(#[from] RepositoryError),
    #[error("wt state operation failed: {0}")]
    Store(#[from] StoreError),
    #[error("GitHub operation failed: {0}")]
    Github(#[from] GithubError),
    #[error("operation lock failed: {0}")]
    Lock(#[from] LockError),
    #[error("blocking store operation failed: {0}")]
    Join(String),
    #[error("stack operation cancelled")]
    Cancelled,
    #[error("another wt operation is already running on this stack's worktrees")]
    Busy,
}

#[derive(Clone)]
pub struct StackService {
    config: StackConfig,
    repository: GitRepository,
    processes: ProcessRunner,
    github: GithubClient,
}

impl StackService {
    pub fn new(
        config: StackConfig,
        repository: GitRepository,
        processes: ProcessRunner,
        github: GithubClient,
    ) -> Self {
        Self {
            config,
            repository,
            processes,
            github,
        }
    }

    pub async fn restack(
        &self,
        branch: &str,
        options: RestackOptions,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(StackEvent),
    ) -> Result<RestackOutcome, StackError> {
        let result = self
            .restack_inner(branch, options, cancellation, on_event)
            .await;
        if cancellation.is_cancelled() {
            Err(StackError::Cancelled)
        } else {
            result
        }
    }

    async fn restack_inner(
        &self,
        branch: &str,
        options: RestackOptions,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(StackEvent),
    ) -> Result<RestackOutcome, StackError> {
        let stack_trunk = self.config.trunk_branch.clone();
        let trunk = options.onto.unwrap_or_else(|| stack_trunk.clone());
        let Some(_) = self
            .resolve_chain(branch, &stack_trunk, cancellation)
            .await?
        else {
            return Ok(RestackOutcome::Refused {
                error: format!("{branch} has no live worktree to restack"),
            });
        };
        let Some((chain, _locks)) = self.lock_chain(branch, &stack_trunk, cancellation).await?
        else {
            return Ok(RestackOutcome::Refused {
                error: format!("{branch} has no live worktree to restack"),
            });
        };
        let locked_slugs = chain
            .steps
            .iter()
            .map(|step| step.slug.as_str())
            .collect::<BTreeSet<_>>();
        match self
            .repository
            .fetch_origin(self.config.fetch_options.clone(), cancellation)
            .await
        {
            Ok(report) => {
                for warning in report.warnings {
                    on_event(StackEvent::Log(warning));
                }
            }
            Err(error) => {
                return Ok(RestackOutcome::Refused {
                    error: format!("{error}; refusing to restack onto possibly stale refs"),
                });
            }
        }
        let landed = self
            .reconcile_locked(&chain, &trunk, cancellation, on_event)
            .await?;
        let mut candidates = chain
            .steps
            .iter()
            .filter(|step| !landed.contains(&step.branch))
            .map(|step| step.branch.clone())
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(RestackOutcome::Refused {
                error: format!("every member of {branch} has landed; clean them first"),
            });
        }

        let mut replayed_roots = BTreeSet::new();
        let mut moved_count = 0;
        let mut total = 0;
        for candidate in candidates.drain(..) {
            let Some(candidate_chain) = self
                .resolve_chain(&candidate, &stack_trunk, cancellation)
                .await?
            else {
                continue;
            };
            if !replayed_roots.insert(candidate_chain.root.clone()) {
                continue;
            }
            if candidate_chain
                .steps
                .iter()
                .any(|step| !locked_slugs.contains(step.slug.as_str()))
            {
                return Ok(RestackOutcome::Refused {
                    error: "stack membership changed during restack; retry after refreshing".into(),
                });
            }
            let result = replay_chain(
                &ReplayContext {
                    service: self,
                    config: &self.config,
                    runner: &self.processes,
                    github: &self.github,
                },
                &candidate_chain,
                &trunk,
                cancellation,
                on_event,
            )
            .await?;
            total += result.total;
            moved_count += result.replayed;
            if let Some(conflict) = result.conflict {
                return Ok(RestackOutcome::Conflict {
                    branch: conflict.branch,
                    backup_ref: conflict.backup_ref,
                    error: conflict.error,
                });
            }
            if let Some(error) = result.error {
                return Ok(RestackOutcome::Refused { error });
            }
        }
        if replayed_roots.is_empty() {
            return Ok(RestackOutcome::Refused {
                error: format!("{branch} has no live worktree to restack"),
            });
        }
        Ok(RestackOutcome::Complete {
            replayed: moved_count,
            total,
        })
    }

    pub async fn prune_backups(
        &self,
        older_than_days: u64,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(StackEvent),
    ) -> Result<PruneBackupsResult, StackError> {
        prune_backups(
            &self.config,
            &self.repository,
            &self.processes,
            older_than_days,
            cancellation,
            on_event,
        )
        .await
    }

    async fn resolve_chain(
        &self,
        branch: &str,
        trunk: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<RestackChain>, StackError> {
        let rows = self.repository.inventory(cancellation).await?;
        let state = self.read_state().await?;
        Ok(resolve_chain(branch, trunk, &rows, &state))
    }

    async fn lock_chain(
        &self,
        branch: &str,
        trunk: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<(RestackChain, Vec<FileLock>)>, StackError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut attempt: u32 = 0;
        loop {
            if cancellation.is_cancelled() {
                return Err(StackError::Cancelled);
            }
            let Some(probe) = self.resolve_chain(branch, trunk, cancellation).await? else {
                return Ok(None);
            };
            let mut slugs = probe
                .steps
                .iter()
                .map(|step| step.slug.clone())
                .collect::<Vec<_>>();
            slugs.sort();
            slugs.dedup();
            let mut locks = Vec::with_capacity(slugs.len());
            let mut contended = false;
            for slug in &slugs {
                match FileLock::try_acquire(&self.config.lock_dir, slug, "restack").await? {
                    Some(lock) => locks.push(lock),
                    None => {
                        contended = true;
                        break;
                    }
                }
            }
            if !contended {
                let Some(current) = self.resolve_chain(branch, trunk, cancellation).await? else {
                    return Ok(None);
                };
                let locked = slugs.iter().map(String::as_str).collect::<BTreeSet<_>>();
                if current
                    .steps
                    .iter()
                    .all(|step| locked.contains(step.slug.as_str()))
                {
                    return Ok(Some((current, locks)));
                }
            }
            drop(locks);
            if tokio::time::Instant::now() >= deadline {
                return Err(StackError::Busy);
            }
            attempt += 1;
            let delay = Duration::from_millis(250 * u64::from(attempt) + jitter_ms(attempt));
            tokio::select! {
                _ = cancellation.cancelled() => return Err(StackError::Cancelled),
                _ = tokio::time::sleep(delay.min(deadline - tokio::time::Instant::now())) => {}
            }
        }
    }

    async fn read_state(&self) -> Result<Value, StackError> {
        let path = self.config.state.path.clone();
        let identity = self.config.state.identity.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = Store::open(path, identity)?;
            store.read_wt_state()
        })
        .await
        .map_err(|error| StackError::Join(error.to_string()))?
        .map_err(Into::into)
    }

    pub(crate) async fn advance_anchor(
        &self,
        slug: &str,
        expected_parent: &str,
        sha: &str,
    ) -> Result<bool, StackError> {
        let path = self.config.state.path.clone();
        let identity = self.config.state.identity.clone();
        let slug = slug.to_owned();
        let expected_parent = expected_parent.to_owned();
        let sha = sha.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut store = Store::open(path, identity)?;
            store.advance_base_anchor(&slug, &expected_parent, &sha)
        })
        .await
        .map_err(|error| StackError::Join(error.to_string()))?
        .map_err(Into::into)
    }

    async fn reconcile_locked(
        &self,
        chain: &RestackChain,
        trunk: &str,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(StackEvent),
    ) -> Result<BTreeSet<String>, StackError> {
        let in_chain = chain
            .steps
            .iter()
            .map(|step| (step.branch.clone(), step.clone()))
            .collect::<BTreeMap<_, _>>();
        let parents = chain
            .steps
            .iter()
            .filter_map(|step| step.parent_branch.clone())
            .collect::<BTreeSet<_>>();
        let mut landed = BTreeSet::new();
        for parent in parents {
            let live = self.github.view_pr(&parent, cancellation).await?;
            if live.as_ref().is_some_and(|pr| pr.state == "MERGED") {
                let number = live.as_ref().map(|pr| pr.number).unwrap_or_default();
                on_event(StackEvent::Log(format!(
                    "parent {parent} merged (#{number})"
                )));
                landed.insert(parent);
                continue;
            }
            if live.is_some() || in_chain.contains_key(&parent) {
                continue;
            }
            if !self.branch_exists(&parent, cancellation).await? {
                on_event(StackEvent::Log(format!("parent {parent} is gone")));
                landed.insert(parent);
            }
        }
        if landed.is_empty() {
            return Ok(landed);
        }
        for step in &chain.steps {
            let Some(parent) = step.parent_branch.as_deref() else {
                continue;
            };
            if !landed.contains(parent) || landed.contains(&step.branch) {
                continue;
            }
            let mut candidate = Some(parent.to_owned());
            while candidate
                .as_ref()
                .is_some_and(|value| landed.contains(value))
            {
                candidate = candidate
                    .as_deref()
                    .and_then(|value| in_chain.get(value))
                    .and_then(|ancestor| ancestor.parent_branch.clone());
            }
            let new_parent = candidate.unwrap_or_else(|| trunk.to_owned());
            self.set_base(&step.slug, &new_parent, step.base_sha.as_deref())
                .await?;
            on_event(StackEvent::Log(format!(
                "reparented {} onto {new_parent}",
                step.branch
            )));
        }
        Ok(landed)
    }

    async fn set_base(
        &self,
        slug: &str,
        parent: &str,
        anchor: Option<&str>,
    ) -> Result<(), StackError> {
        let path = self.config.state.path.clone();
        let identity = self.config.state.identity.clone();
        let slug = slug.to_owned();
        let parent = parent.to_owned();
        let anchor = anchor.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            let mut store = Store::open(path, identity)?;
            store.set_slug_base(&slug, Some((&parent, anchor.as_deref())))
        })
        .await
        .map_err(|error| StackError::Join(error.to_string()))??;
        Ok(())
    }

    async fn branch_exists(
        &self,
        branch: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, StackError> {
        for reference in [
            format!("refs/heads/{branch}"),
            format!("refs/remotes/origin/{branch}"),
        ] {
            if git_sha(
                &self.processes,
                &self.config.main_clone,
                &reference,
                cancellation,
            )
            .await?
            .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn jitter_ms(attempt: u32) -> u64 {
    u64::from((attempt.wrapping_mul(73)) % 251)
}
