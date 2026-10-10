//! Reactive bridge from prepared Git/state sources to bounded naming work.
//! Diff collection is keyed by stable worktree inputs; board-only updates do
//! not restart Git, and naming results never write a derived title to state.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Mutex as AsyncMutex, Semaphore, mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use wt_core::worktree_target_key;
use wt_naming::{AiSummary, HarnessPrograms, NamingCacheKey, NamingService, NamingServiceConfig};
use wt_platform::{lock::FileLock, process::CommandSpec};
use wt_runtime::{SourceHandle, SourcePublisher, SourceSnapshot, TaskScope, source_channel};
use wt_tui::Board;
use wt_vcs::{WorktreeRecord, WorktreeSnapshot};

use crate::{
    context::AppContext,
    local_source::Metadata,
    naming::{NamingCache, manual_title, title_revision},
};

const MAX_PENDING_REQUESTS: usize = 32;
const MAX_DIFF_CONCURRENCY: usize = 2;
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub struct NamingSources {
    pub board: SourceHandle<Board>,
    pub commands: NamingCommands,
}

#[derive(Clone)]
pub struct NamingCommands {
    sender: mpsc::Sender<ExplicitRequest>,
    invalidate: mpsc::Sender<oneshot::Sender<Result<(), String>>>,
    active: Arc<Mutex<HashMap<String, (u64, CancellationToken)>>>,
    configured: bool,
}

struct ExplicitRequest {
    key: String,
    id: u64,
    cancellation: CancellationToken,
}

impl NamingCommands {
    /// Accept explicit title generation without waiting for Git or a harness.
    /// A newer request for the same worktree supersedes the previous request.
    pub fn request(&self, key: impl Into<String>) -> Result<(), String> {
        if !self.configured {
            return Err("naming is not configured ([naming] is missing)".into());
        }
        let key = key.into();
        if key.is_empty() {
            return Err("worktree key is empty".into());
        }
        let id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let cancellation = CancellationToken::new();
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.sender
            .try_send(ExplicitRequest {
                key: key.clone(),
                id,
                cancellation: cancellation.clone(),
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => "naming request queue is full".to_owned(),
                mpsc::error::TrySendError::Closed(_) => {
                    "naming service is shutting down".to_owned()
                }
            })?;
        if let Some((_, previous)) = active.insert(key, (id, cancellation)) {
            previous.cancel();
        }
        Ok(())
    }

    /// Clear derived title results and restart automatic naming from current inputs.
    /// Explicit title requests remain accepted and are not canceled.
    pub async fn invalidate_derived(&self) -> Result<(), String> {
        let (reply, response) = oneshot::channel();
        self.invalidate
            .send(reply)
            .await
            .map_err(|_| "naming service is shutting down".to_owned())?;
        response
            .await
            .map_err(|_| "naming service stopped before clearing its cache".to_owned())?
    }
}

#[derive(Clone)]
struct Candidate {
    key: String,
    signature: String,
    worktree: WorktreeRecord,
}

#[derive(Clone, Copy)]
enum RequestMode {
    Automatic,
    Explicit { id: u64 },
}

struct NamingOutcome {
    key: String,
    signature: String,
    generation: u64,
    mode: RequestMode,
    result: Result<Option<(NamingCacheKey, AiSummary)>, String>,
}

#[derive(Clone)]
struct NamingEngine {
    context: AppContext,
    service: Option<NamingService>,
    cache: NamingCache,
    diff_permits: Arc<Semaphore>,
    summary_gate: Arc<AsyncMutex<()>>,
}

struct TitleExpectation {
    revision: u64,
    base: Option<String>,
    branch: String,
    trunk: String,
}

struct ExplicitCommitGuard {
    active: Arc<Mutex<HashMap<String, (u64, CancellationToken)>>>,
    request_id: u64,
    cancellation: CancellationToken,
}

pub fn start(
    scope: &TaskScope,
    context: &AppContext,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<Metadata>,
    upstream: SourceHandle<Board>,
) -> NamingSources {
    let configured = context.config.naming.is_some();
    let (board, mut publisher) = source_channel();
    let (sender, mut requests) = mpsc::channel(MAX_PENDING_REQUESTS);
    let (invalidate, mut invalidations) = mpsc::channel(1);
    let active = Arc::new(Mutex::new(HashMap::new()));
    let commands = NamingCommands {
        sender,
        invalidate,
        active: active.clone(),
        configured,
    };
    let context = context.clone();
    let cancel = scope.token();
    scope.spawn(async move {
        let cache = NamingCache::new(&context.config.paths.cache_root);
        cache.load().await;
        let service_result = context.config.naming.clone().map(|naming| {
            NamingService::new(NamingServiceConfig {
                naming,
                // The per-user harness selection is persisted separately from
                // the config fallback and can be changed with CyclePrimary.
                primary_harness: crate::harness::AppHarness::new(&context).primary(),
                main_clone: context.config.paths.main_clone.clone(),
                trunk_branch: context.config.branch.base.clone(),
                programs: HarnessPrograms::default(),
            }, context.processes.clone())
        });
        let service_error = service_result.as_ref().and_then(|result| result.as_ref().err()).map(ToString::to_string);
        let service = service_result.and_then(Result::ok);
        let engine = NamingEngine {
            context: context.clone(),
            service,
            cache,
            diff_permits: Arc::new(Semaphore::new(MAX_DIFF_CONCURRENCY)),
            summary_gate: Arc::new(AsyncMutex::new(())),
        };
        let mut git_updates = git.subscribe();
        let mut metadata_updates = metadata.subscribe();
        let mut upstream_updates = upstream.subscribe();
        git_updates.mark_changed();
        metadata_updates.mark_changed();
        upstream_updates.mark_changed();
        let mut last_git = git_updates.borrow().clone();
        let mut last_metadata = metadata_updates.borrow().clone();
        let mut latest_board = upstream_updates.borrow().clone();
        let mut last_input = HashMap::<String, String>::new();
        let mut auto_tokens = HashMap::<String, (String, CancellationToken)>::new();
        let mut attempted = HashSet::<(String, String)>::new();
        let mut visible = HashMap::<String, (NamingCacheKey, AiSummary)>::new();
        let mut errors = HashMap::<String, String>::new();
        let mut tasks = JoinSet::<NamingOutcome>::new();
        let mut generation = 0_u64;
        let mut output_fingerprint = String::new();
        publish_board(&mut publisher, &latest_board, &last_metadata, &visible, &errors, &mut output_fingerprint);

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                changed = git_updates.changed() => {
                    if changed.is_err() { break; }
                    last_git = git_updates.borrow_and_update().clone();
                    let candidates = candidates(&context, &last_git, &last_metadata);
                    if let Some(error) = &service_error {
                        for candidate in &candidates { errors.insert(candidate.key.clone(), error.clone()); }
                    }
                    update_automatic(
                        candidates, engine.clone(), generation,
                        &mut tasks, &mut auto_tokens, &mut attempted, &mut last_input,
                    );
                }
                changed = metadata_updates.changed() => {
                    if changed.is_err() { break; }
                    last_metadata = metadata_updates.borrow_and_update().clone();
                    let candidates = candidates(&context, &last_git, &last_metadata);
                    if let Some(error) = &service_error {
                        for candidate in &candidates { errors.insert(candidate.key.clone(), error.clone()); }
                    }
                    update_automatic(
                        candidates, engine.clone(), generation,
                        &mut tasks, &mut auto_tokens, &mut attempted, &mut last_input,
                    );
                    publish_board(&mut publisher, &latest_board, &last_metadata, &visible, &errors, &mut output_fingerprint);
                }
                changed = upstream_updates.changed() => {
                    if changed.is_err() { break; }
                    latest_board = upstream_updates.borrow_and_update().clone();
                    publish_board(&mut publisher, &latest_board, &last_metadata, &visible, &errors, &mut output_fingerprint);
                }
                request = requests.recv() => {
                    let Some(request) = request else { break; };
                    if let Some((_, token)) = auto_tokens.remove(&request.key) { token.cancel(); }
                    errors.remove(&request.key);
                    if engine.service.is_none() {
                        errors.insert(request.key.clone(), service_error.clone().unwrap_or_else(|| "naming is not configured".into()));
                        publish_board(&mut publisher, &latest_board, &last_metadata, &visible, &errors, &mut output_fingerprint);
                        continue;
                    }
                    if let Some(snapshot) = last_git.data.as_deref().and_then(|rows| rows.iter().find(|row| worktree_target_key(&row.worktree.target) == request.key && !row.worktree.is_main)).cloned() {
                        let token = request.cancellation.clone();
                        let engine = engine.clone();
                        let signature = snapshot_signature(&context, &snapshot.worktree, &last_metadata);
                        let current_requests = active.clone();
                        tasks.spawn(async move {
                            explicit_job(engine, snapshot.worktree, request.key, request.id, signature, current_requests, token).await
                        });
                    } else {
                        errors.insert(request.key.clone(), "worktree is not in the current Git inventory".into());
                        clear_active(&active, &request.key, request.id);
                        publish_board(&mut publisher, &latest_board, &last_metadata, &visible, &errors, &mut output_fingerprint);
                    }
                }
                request = invalidations.recv() => {
                    let Some(reply) = request else { break; };
                    generation = generation.wrapping_add(1);
                    for (_, token) in auto_tokens.values() { token.cancel(); }
                    auto_tokens.clear();
                    attempted.clear();
                    last_input.clear();
                    visible.clear();
                    errors.clear();
                    let result = engine.cache.clear(&context.config.paths.lock_dir, &context.cancellation).await
                        .map_err(|error| format!("clear derived naming cache: {error:#}"));
                    if result.is_ok() {
                        update_automatic(
                            candidates(&context, &last_git, &last_metadata), engine.clone(), generation,
                            &mut tasks, &mut auto_tokens, &mut attempted, &mut last_input,
                        );
                    }
                    publish_board(&mut publisher, &latest_board, &last_metadata, &visible, &errors, &mut output_fingerprint);
                    let _ = reply.send(result);
                }
                completed = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Ok(outcome)) = completed {
                        let explicit_current = match outcome.mode {
                            RequestMode::Automatic => true,
                            RequestMode::Explicit { id } => is_current(&active, &outcome.key, id),
                        };
                        let latest_signature = last_input.get(&outcome.key).cloned();
                        let current = explicit_current && match outcome.mode {
                            RequestMode::Automatic => outcome.generation == generation
                                && latest_signature.as_deref() == Some(&outcome.signature),
                            RequestMode::Explicit { .. } => last_git.data.as_deref().is_some_and(|rows| {
                                rows.iter().any(|row| !row.worktree.is_main
                                    && row.error.is_none()
                                    && worktree_target_key(&row.worktree.target) == outcome.key
                                    && snapshot_signature(&context, &row.worktree, &last_metadata) == outcome.signature)
                            }),
                        };
                        if current {
                            match outcome.result {
                                Ok(Some((cache_key, summary))) => {
                                    // The worker has already persisted the computed content key. Keep result
                                    // keyed by slug here so a new hash can retain the previous display value.
                                    visible.insert(outcome.key.clone(), (cache_key, summary));
                                    errors.remove(&outcome.key);
                                }
                                Ok(None) => { errors.remove(&outcome.key); }
                                Err(error) => { errors.insert(outcome.key.clone(), error); }
                            }
                            publish_board(&mut publisher, &latest_board, &last_metadata, &visible, &errors, &mut output_fingerprint);
                        }
                        if let RequestMode::Explicit { id } = outcome.mode { clear_active(&active, &outcome.key, id); }
                    }
                }
                _ = publisher.requested() => {
                    git.refresh();
                    metadata.refresh();
                    upstream.refresh();
                }
            }
        }
        for (_, (_, token)) in auto_tokens { token.cancel(); }
        for (_, (_, token)) in active.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).drain() { token.cancel(); }
        while tasks.join_next().await.is_some() {}
    });
    NamingSources { board, commands }
}

fn candidates(
    context: &AppContext,
    git: &SourceSnapshot<Vec<WorktreeSnapshot>>,
    metadata: &SourceSnapshot<Metadata>,
) -> Vec<Candidate> {
    if !context
        .config
        .naming
        .as_ref()
        .is_some_and(|naming| naming.auto_rename)
    {
        return Vec::new();
    }
    let (Some(rows), Some((state, _))) = (git.data.as_deref(), metadata.data.as_deref()) else {
        return Vec::new();
    };
    rows.iter()
        .filter(|snapshot| !snapshot.worktree.is_main && snapshot.error.is_none())
        .filter(|snapshot| manual_title(state, snapshot.worktree.target.slug()).is_none())
        .map(|snapshot| {
            let worktree = snapshot.worktree.clone();
            let key = worktree_target_key(&worktree.target);
            let signature = snapshot_signature(context, &worktree, metadata);
            Candidate {
                key,
                signature,
                worktree,
            }
        })
        .collect()
}

fn snapshot_signature(
    context: &AppContext,
    worktree: &WorktreeRecord,
    metadata: &SourceSnapshot<Metadata>,
) -> String {
    let state = metadata.data.as_deref().map(|m| &m.0);
    let slug = worktree.target.slug();
    let recorded = state
        .and_then(|state| state["slugs"][slug]["baseBranch"].as_str())
        .unwrap_or("");
    let base = if recorded.is_empty()
        || recorded == context.config.branch.base
        || recorded == worktree.target.branch
    {
        format!("origin/{}", context.config.branch.base)
    } else {
        recorded.to_owned()
    };
    let text = format!(
        "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{:?}",
        slug,
        worktree.target.path,
        worktree.target.branch,
        worktree.head_sha.as_deref().unwrap_or(""),
        worktree
            .git_dir
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        worktree
            .common_dir
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        base,
        worktree.kind,
    );
    hex(&Sha256::digest(text.as_bytes()))
}

fn update_automatic(
    candidates: Vec<Candidate>,
    engine: NamingEngine,
    generation: u64,
    tasks: &mut JoinSet<NamingOutcome>,
    auto_tokens: &mut HashMap<String, (String, CancellationToken)>,
    attempted: &mut HashSet<(String, String)>,
    last_input: &mut HashMap<String, String>,
) {
    let incoming: HashMap<_, _> = candidates.into_iter().map(|c| (c.key.clone(), c)).collect();
    for (key, (signature, token)) in auto_tokens.iter() {
        if incoming
            .get(key)
            .is_none_or(|candidate| candidate.signature != *signature)
        {
            token.cancel();
        }
    }
    auto_tokens.retain(|key, (signature, _)| {
        incoming
            .get(key)
            .is_some_and(|candidate| candidate.signature == *signature)
    });
    last_input.retain(|key, _| incoming.contains_key(key));
    attempted.retain(|(key, signature)| {
        incoming
            .get(key)
            .is_some_and(|candidate| candidate.signature == *signature)
    });
    for (key, candidate) in incoming {
        last_input.insert(key.clone(), candidate.signature.clone());
        if auto_tokens.contains_key(&key)
            || !attempted.insert((key.clone(), candidate.signature.clone()))
        {
            continue;
        }
        if engine.service.is_none() {
            continue;
        }
        let token = engine.context.cancellation.child_token();
        auto_tokens.insert(key.clone(), (candidate.signature.clone(), token.clone()));
        let engine = engine.clone();
        let signature = candidate.signature;
        let task_generation = generation;
        tasks.spawn(async move {
            summary_job(
                engine,
                candidate.worktree,
                key,
                signature,
                task_generation,
                token,
            )
            .await
        });
    }
}

async fn summary_job(
    engine: NamingEngine,
    worktree: WorktreeRecord,
    key: String,
    signature: String,
    generation: u64,
    cancellation: CancellationToken,
) -> NamingOutcome {
    let context = &engine.context;
    let service = engine
        .service
        .as_ref()
        .expect("automatic naming has a service");
    let slug = worktree.target.slug().to_owned();
    let result = async {
        let _diff = tokio::select! { biased; _ = cancellation.cancelled() => anyhow::bail!("naming request cancelled"), permit = engine.diff_permits.acquire() => permit.context("diff worker unavailable")? };
        let state = context.database.call(|store| Ok(store.read_wt_state()?)).await?;
        let revision = title_revision(&state, &slug);
        if manual_title(&state, &slug).is_some() { anyhow::bail!("automatic naming is disabled by a manual title"); }
        let base = effective_base(&state, &slug, &worktree.target.branch, &context.config.branch.base);
        let diff = service.diff_context(&worktree.target.path, base.as_deref(), &cancellation).await?;
        let Some(diff) = diff else { return Ok::<_, anyhow::Error>(None); };
        drop(_diff);
        let cache_key = summary_cache_key_for_diff(context, &slug, &diff.hash);
        let _serial = tokio::select! { biased; _ = cancellation.cancelled() => anyhow::bail!("naming request cancelled"), lock = engine.summary_gate.lock() => lock };
        let summary = engine.cache.get(&cache_key).await;
        let summary = if let Some(summary) = summary {
            summary
        } else {
            let summary = service.summarize_diff(&diff.prompt, &cancellation).await?;
            validate_revision(context, &slug, revision, true, base.as_deref(), &worktree.target.branch, &context.config.branch.base).await?;
            engine.cache.put(&cache_key, summary.clone(), &context.config.paths.lock_dir, &cancellation).await?;
            summary
        };
        validate_revision(context, &slug, revision, true, base.as_deref(), &worktree.target.branch, &context.config.branch.base).await?;
        Ok(Some((cache_key, summary)))
    }.await;
    NamingOutcome {
        key,
        signature,
        generation,
        mode: RequestMode::Automatic,
        result: result.map_err(|e| wt_core::sanitize_terminal_text(&e.to_string())),
    }
}

async fn explicit_job(
    engine: NamingEngine,
    worktree: WorktreeRecord,
    key: String,
    id: u64,
    signature: String,
    active: Arc<Mutex<HashMap<String, (u64, CancellationToken)>>>,
    cancellation: CancellationToken,
) -> NamingOutcome {
    let context = &engine.context;
    let service = engine
        .service
        .as_ref()
        .expect("explicit naming has a service");
    let slug = worktree.target.slug().to_owned();
    let result = async {
        if !is_current(&active, &key, id) || cancellation.is_cancelled() { anyhow::bail!("title request superseded"); }
        let state = context.database.call(|store| Ok(store.read_wt_state()?)).await?;
        let revision = title_revision(&state, &slug);
        let base = effective_base(&state, &slug, &worktree.target.branch, &context.config.branch.base);
        let _diff = tokio::select! { biased; _ = cancellation.cancelled() => anyhow::bail!("naming request cancelled"), permit = engine.diff_permits.acquire() => permit.context("diff worker unavailable")? };
        let diff = service.diff_context(&worktree.target.path, base.as_deref(), &cancellation).await?;
        let Some(diff) = diff else { anyhow::bail!("no committed changes to name"); };
        drop(_diff);
        let cache_key = summary_cache_key_for_diff(context, &slug, &diff.hash);
        let _serial = tokio::select! { biased; _ = cancellation.cancelled() => anyhow::bail!("naming request cancelled"), lock = engine.summary_gate.lock() => lock };
        let summary = service.summarize_diff(&diff.prompt, &cancellation).await?;
        if summary.title.as_deref().is_none_or(|title| title.trim().is_empty()) { anyhow::bail!("naming harness returned no title"); }
        if !is_current(&active, &key, id) || cancellation.is_cancelled() { anyhow::bail!("title request superseded"); }
        // Serialize the final identity check and CAS against remove/recreate.
        // Lifecycle uses the same per-slug lock, so a deleted worktree cannot
        // be replaced between this check and the durable title write.
        let _worktree_lock = FileLock::acquire(
            &context.config.paths.lock_dir,
            &slug,
            "save generated title",
            &cancellation,
        ).await.context("lock worktree title")?;
        verify_head(context, &worktree, &cancellation).await?;
        commit_explicit_title(
            context,
            &slug,
            &summary,
            TitleExpectation {
                revision,
                base: base.clone(),
                branch: worktree.target.branch.clone(),
                trunk: context.config.branch.base.clone(),
            },
            ExplicitCommitGuard {
                active: active.clone(),
                request_id: id,
                cancellation: cancellation.clone(),
            },
        )
        .await?;
        drop(_worktree_lock);
        engine.cache.put(&cache_key, summary.clone(), &context.config.paths.lock_dir, &cancellation).await?;
        Ok(Some((cache_key, summary)))
    }.await;
    NamingOutcome {
        key,
        signature,
        generation: 0,
        mode: RequestMode::Explicit { id },
        result: result.map_err(|e| wt_core::sanitize_terminal_text(&e.to_string())),
    }
}

async fn commit_explicit_title(
    context: &AppContext,
    slug: &str,
    summary: &AiSummary,
    expected: TitleExpectation,
    guard: ExplicitCommitGuard,
) -> Result<()> {
    let Some(title) = summary
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
    else {
        anyhow::bail!("naming harness returned no title");
    };
    let slug = slug.to_owned();
    let title = title.to_owned();
    if guard.cancellation.is_cancelled() {
        anyhow::bail!("title request superseded");
    }
    let TitleExpectation {
        revision,
        base: expected_base,
        branch,
        trunk,
    } = expected;
    let ExplicitCommitGuard {
        active,
        request_id,
        cancellation: _,
    } = guard;
    context
        .database
        .call(move |store| {
            // Hold the request-generation guard across the synchronous state
            // CAS so a newer T request cannot slip between check and write.
            let active = active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !active
                .get(&slug)
                .is_some_and(|(id, token)| *id == request_id && !token.is_cancelled())
            {
                return Ok(false);
            }
            let state = store.read_wt_state()?;
            let current_revision = title_revision(&state, &slug);
            if current_revision != revision
                || effective_base(&state, &slug, &branch, &trunk) != expected_base
            {
                return Ok(false);
            }
            if manual_title(&state, &slug).is_some() {
                Ok(store.set_slug_manual_title(&slug, &title, Some(revision))?)
            } else {
                // The generated title remains derived. This read-side CAS check
                // prevents a delayed result from replacing a newer manual edit.
                Ok(true)
            }
        })
        .await
        .context("save generated title")?
        .then_some(())
        .ok_or_else(|| {
            anyhow::anyhow!("title changed while generation was running; kept the newer title")
        })?;
    Ok(())
}

async fn validate_revision(
    context: &AppContext,
    slug: &str,
    revision: u64,
    automatic: bool,
    expected_base: Option<&str>,
    branch: &str,
    trunk: &str,
) -> Result<()> {
    let slug = slug.to_owned();
    let expected_base = expected_base.map(str::to_owned);
    let branch = branch.to_owned();
    let trunk = trunk.to_owned();
    let (current, has_manual, current_base) = context
        .database
        .call(move |store| {
            let state = store.read_wt_state()?;
            Ok((
                title_revision(&state, &slug),
                manual_title(&state, &slug).is_some(),
                effective_base(&state, &slug, &branch, &trunk),
            ))
        })
        .await?;
    if current != revision || (automatic && has_manual) || current_base != expected_base {
        anyhow::bail!(
            "title or fork base changed while generation was running; kept the newer state"
        );
    }
    Ok(())
}

async fn verify_head(
    context: &AppContext,
    worktree: &WorktreeRecord,
    cancellation: &CancellationToken,
) -> Result<()> {
    let inventory = context.repository.inventory(cancellation).await?;
    let target = inventory
        .iter()
        .find(|current| {
            !current.is_main
                && current.target.path == worktree.target.path
                && current.target.slug() == worktree.target.slug()
        })
        .context("worktree was removed or recreated while title was generated")?;
    if target.target.branch != worktree.target.branch
        || target.git_dir != worktree.git_dir
        || target.common_dir != worktree.common_dir
        || target.kind != worktree.kind
        || target.head_sha != worktree.head_sha
    {
        anyhow::bail!("worktree identity changed while title was generated; kept the newer state");
    }
    let mut command = CommandSpec::new("git").args(["rev-parse", "--verify", "HEAD"]);
    command.cwd = Some(worktree.target.path.clone().into());
    command.timeout = Duration::from_secs(5);
    command.output_limit = 4096;
    let output = context.processes.run(command, cancellation).await?;
    if !output.status.success() {
        anyhow::bail!(
            "cannot verify current worktree head: {}",
            output.stderr_text().trim()
        );
    }
    let head = output.stdout_text().trim().to_owned();
    if worktree
        .head_sha
        .as_deref()
        .is_some_and(|expected| expected != head)
    {
        anyhow::bail!("worktree head changed while title was generated; kept the newer state");
    }
    Ok(())
}

fn effective_base(
    state: &serde_json::Value,
    slug: &str,
    branch: &str,
    trunk: &str,
) -> Option<String> {
    state["slugs"][slug]["baseBranch"]
        .as_str()
        .map(str::to_owned)
        .filter(|base| !base.is_empty() && base != trunk && base != branch)
}

fn summary_cache_key_for_diff(context: &AppContext, slug: &str, hash: &str) -> NamingCacheKey {
    if context
        .config
        .naming
        .as_ref()
        .is_some_and(|n| !n.auto_rename)
    {
        NamingCacheKey::Manual(slug.to_owned())
    } else {
        wt_naming::diff_cache_key(hash)
    }
}

fn publish_board(
    publisher: &mut SourcePublisher<Board>,
    upstream: &SourceSnapshot<Board>,
    metadata: &SourceSnapshot<Metadata>,
    visible: &HashMap<String, (NamingCacheKey, AiSummary)>,
    errors: &HashMap<String, String>,
    previous: &mut String,
) {
    let Some(source) = upstream.data.as_deref() else {
        publisher.publish(SourceSnapshot {
            data: None,
            state: upstream.state.clone(),
            updated_at: upstream.updated_at,
            revision: 0,
        });
        return;
    };
    let state = metadata.data.as_deref().map(|value| &value.0);
    let mut board = source.clone();
    for row in &mut board.rows {
        // Remote rows can share a slug with a local checkout. Their metadata
        // and title are owned by the remote controller, never local naming.
        if row.key.starts_with("@remote/") {
            continue;
        }
        let manual = state.and_then(|state| manual_title(state, &row.slug));
        let summary = visible.get(&row.key).map(|(_, summary)| summary);
        (row.title, row.title_source) = title_fallback(
            &row.slug,
            manual.as_deref(),
            summary.and_then(|value| value.title.as_deref()),
            row.git.pr_title.as_deref(),
            row.git.first_commit_title.as_deref(),
        );
        row.details.retain(|detail| {
            !detail.starts_with("AI summary: ") && !detail.starts_with("Naming: ")
        });
        if let Some(description) = summary
            .map(|s| s.description.trim())
            .filter(|text| !text.is_empty())
        {
            row.details.push(format!(
                "AI summary: {}",
                wt_core::sanitize_terminal_text(description)
            ));
        }
        if let Some(error) = errors.get(&row.key) {
            row.details.push(format!(
                "Naming: {}",
                wt_core::sanitize_terminal_text(error)
            ));
        }
    }
    let fingerprint = board_fingerprint(&board, upstream);
    if fingerprint == *previous {
        return;
    }
    *previous = fingerprint;
    publisher.publish(SourceSnapshot {
        data: Some(Arc::new(board)),
        state: upstream.state.clone(),
        updated_at: upstream.updated_at,
        revision: 0,
    });
}

fn title_fallback(
    slug: &str,
    manual: Option<&str>,
    generated: Option<&str>,
    pr: Option<&str>,
    first_commit: Option<&str>,
) -> (String, wt_tui::TitleSource) {
    use wt_tui::TitleSource;
    [
        (manual, TitleSource::Manual),
        (generated, TitleSource::Llm),
        (pr, TitleSource::Pr),
        (first_commit, TitleSource::Commit),
    ]
    .into_iter()
    .find_map(|(title, source)| {
        title
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(|title| (wt_core::sanitize_terminal_text(title), source))
    })
    .unwrap_or_else(|| {
        (
            wt_core::sanitize_terminal_text(&crate::issue_identity::slug_title(slug)),
            TitleSource::Slug,
        )
    })
}

fn board_fingerprint(board: &Board, upstream: &SourceSnapshot<Board>) -> String {
    let mut text = format!("{}:{}", upstream.revision, board.name);
    for row in &board.rows {
        text.push_str(&format!(
            "\0{}\0{}\0{}\0{}\0{}",
            row.key,
            row.title,
            row.title_source.as_str(),
            row.badge,
            row.details.join("\n")
        ));
    }
    hex(&Sha256::digest(text.as_bytes()))
}

fn is_current(
    active: &Arc<Mutex<HashMap<String, (u64, CancellationToken)>>>,
    key: &str,
    id: u64,
) -> bool {
    active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
        .is_some_and(|(active_id, token)| *active_id == id && !token.is_cancelled())
}

fn clear_active(
    active: &Arc<Mutex<HashMap<String, (u64, CancellationToken)>>>,
    key: &str,
    id: u64,
) {
    let mut active = active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if active
        .get(key)
        .is_some_and(|(active_id, _)| *active_id == id)
    {
        active.remove(key);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod title_tests {
    use super::title_fallback;

    #[test]
    fn title_fallback_preserves_manual_and_generated_precedence() {
        assert_eq!(
            title_fallback(
                "feat-42-fix",
                Some("Manual"),
                Some("Generated"),
                Some("PR"),
                Some("Commit")
            ),
            ("Manual".into(), wt_tui::TitleSource::Manual)
        );
        assert_eq!(
            title_fallback(
                "feat-42-fix",
                None,
                Some("Generated"),
                Some("PR"),
                Some("Commit")
            ),
            ("Generated".into(), wt_tui::TitleSource::Llm)
        );
    }

    #[test]
    fn title_fallback_uses_pr_then_first_commit_then_slug() {
        assert_eq!(
            title_fallback(
                "feat-42-fix",
                None,
                Some("  "),
                Some("PR title"),
                Some("Commit title")
            ),
            ("PR title".into(), wt_tui::TitleSource::Pr)
        );
        assert_eq!(
            title_fallback("feat-42-fix", None, None, None, Some("Commit title")),
            ("Commit title".into(), wt_tui::TitleSource::Commit)
        );
        assert_eq!(
            title_fallback("feat-42-fix", None, None, None, None),
            ("Fix".into(), wt_tui::TitleSource::Slug)
        );
    }
}
