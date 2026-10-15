//! Bounded, cached Git facts used by the live worktree presentation.
//!
//! Git inventory already supplies status counts. This lane adds facts that
//! need extra Git or filesystem reads, and only recomputes a row when its
//! identity, HEAD/status, fork-base record, PR, index, or rebase state changes.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{fs, io::AsyncReadExt};
use tokio_util::sync::CancellationToken;
use wt_core::Domain;
use wt_github::{GithubData, PullRequest};
use wt_platform::process::CommandSpec;
use wt_runtime::{
    DiskCache, Persistence, RefreshPolicy, SourceHandle, SourceSnapshot, SourceSpec, SourceState,
    Stamp, TaskScope, derived_channel, leaf_channel,
};
use wt_tui::{Board, GitPresentation, LandingKind};
use wt_vcs::WorktreeSnapshot;

use crate::{context::AppContext, local_source::Metadata};

const MAX_CONCURRENT_ROWS: usize = 4;
const GIT_TIMEOUT: Duration = Duration::from_secs(8);
const GIT_OUTPUT_LIMIT: usize = 1024 * 1024;
/// Dirty rows re-read their diff stat at most this often: editing an already
/// modified file changes neither the index nor the status counts. Only the
/// working-tree probes rerun on this tick; ref-derived facts follow the row
/// signature.
const ACTIVITY_REFRESH: Duration = Duration::from_secs(60);
/// Untracked files counted line by line for the diff stat; beyond either
/// per-file bound a file still counts as changed but adds no lines.
const MAX_UNTRACKED_FILES: usize = 500;
const MAX_UNTRACKED_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Total untracked bytes streamed per row and pass. Counting stops there, so
/// the added-line count is a lower bound for very large untracked trees.
const MAX_UNTRACKED_TOTAL_BYTES: u64 = 8 * 1024 * 1024;

pub type PresentationMap = BTreeMap<String, GitFact>;

/// `merge-tree` results keyed on the exact `(HEAD sha, base sha)` pair, so the
/// probe (which can write tree objects) runs once per pair. `None` values are
/// a known-unknown result, such as a Git without `merge-tree --write-tree`.
type ConflictCache = std::sync::Mutex<HashMap<(String, String), Option<Vec<String>>>>;

/// Inputs recorded by a full enrichment that later cheap passes reuse.
#[derive(Clone, Debug, Default)]
struct ActivityAnchors {
    /// Fork point the diff stat is measured from.
    merge_base: Option<String>,
    /// The `(HEAD sha, base sha)` pair whose conflict result this row uses.
    conflict_pair: Option<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFact {
    identity: FactIdentity,
    presentation: GitPresentation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FactIdentity {
    path: String,
    branch: String,
    head: Option<String>,
    base_branch: String,
    base_sha: Option<String>,
    pr: Option<PrIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PrIdentity {
    number: u64,
    title: String,
    head_oid: Option<String>,
    state: String,
    base: String,
    merge_oid: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct RowSignature<'a> {
    key: String,
    path: &'a str,
    branch: &'a str,
    head: Option<&'a str>,
    status: Option<StatusSignature<'a>>,
    base_branch: Option<&'a str>,
    base_sha: Option<&'a str>,
    pr: Option<PrSignature<'a>>,
    index: FileStamp,
    rebase_merge: FileStamp,
    rebase_apply: FileStamp,
    base_ref: Vec<FileStamp>,
    production_ref: Vec<FileStamp>,
    packed_refs: FileStamp,
}

#[derive(Clone, Debug, Serialize)]
struct StatusSignature<'a> {
    branch: Option<&'a str>,
    head: Option<&'a str>,
    upstream: Option<&'a str>,
    ahead: Option<u32>,
    behind: Option<u32>,
    tracked: u32,
    untracked: u32,
}

#[derive(Clone, Debug, Serialize)]
struct PrSignature<'a> {
    title: &'a str,
    state: &'a str,
    base: &'a str,
    merge_oid: Option<&'a str>,
    head_oid: Option<&'a str>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct FileStamp {
    exists: bool,
    len: u64,
    modified_ns: Option<u128>,
}

#[derive(Clone)]
struct CachedRow {
    signature: String,
    identity: FactIdentity,
    presentation: GitPresentation,
    retry_after: Option<tokio::time::Instant>,
    /// `ACTIVITY_REFRESH` bucket the diff stat was read in; `None` when
    /// the row was clean.
    activity_epoch: Option<u64>,
    anchors: ActivityAnchors,
}

/// The facts survive restarts as restored display data. A restored fact only
/// applies to a row whose current identity (path, branch, HEAD, base, PR)
/// still matches, so stale facts never attach to a changed worktree.
fn facts_spec(context: &AppContext) -> SourceSpec<PresentationMap> {
    SourceSpec {
        name: "git-presentation",
        domain: Domain::Presentation,
        persistence: Persistence::Disk(DiskCache::new(&context.caches)),
        policy: RefreshPolicy::default(),
    }
}

/// Start the host-local facts lane. It consumes prepared inputs and performs
/// bounded Git work off the input/render path. It reads only the worktrees a
/// live Git scan names; restored facts stay shown until then.
pub fn start(
    scope: &TaskScope,
    context: &AppContext,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<Metadata>,
    github: SourceHandle<GithubData>,
) -> SourceHandle<PresentationMap> {
    let (source, mut publisher) = leaf_channel(scope, facts_spec(context));
    let cancellation = scope.token();
    let context = context.clone();
    scope.spawn(async move {
        let mut git_updates = git.subscribe();
        let mut metadata_updates = metadata.subscribe();
        let mut github_updates = github.subscribe();
        git_updates.mark_changed();
        metadata_updates.mark_changed();
        github_updates.mark_changed();
        let mut cache = HashMap::<String, CachedRow>::new();
        let conflicts = Arc::new(ConflictCache::default());
        let mut last_published: Option<PresentationMap> = None;
        let mut last_state: Option<SourceState> = None;
        let mut activity_tick = tokio::time::interval(ACTIVITY_REFRESH);
        activity_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let retry_at = cache.values().filter_map(|row| row.retry_after).min();
            let retry_timer = async move {
                if let Some(retry_at) = retry_at {
                    tokio::time::sleep_until(retry_at).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    git.refresh(); metadata.refresh(); github.refresh();
                    continue;
                }
                changed = git_updates.changed() => if changed.is_err() { break; },
                changed = metadata_updates.changed() => if changed.is_err() { break; },
                changed = github_updates.changed() => if changed.is_err() { break; },
                _ = retry_timer => {},
                _ = activity_tick.tick() => {},
            }

            let observed = Stamp::now();
            let git_snapshot = git_updates.borrow_and_update().clone();
            let metadata_snapshot = metadata_updates.borrow_and_update().clone();
            let github_snapshot = github_updates.borrow_and_update().clone();
            // Without inputs the last facts (live or restored) stay shown. The
            // row set comes from Git: a restored inventory may name removed
            // worktrees, so it never keys probes published as live facts.
            let Some(rows) = git_snapshot
                .data()
                .map(Arc::as_ref)
                .filter(|_| scanned_live(&git_snapshot))
            else {
                publisher.set_state(git_snapshot.state().clone());
                continue;
            };
            let Some((state, _)) = metadata_snapshot.data().map(Arc::as_ref) else {
                publisher.set_state(metadata_snapshot.state().clone());
                continue;
            };
            let prs = github_snapshot.data().map(Arc::as_ref);
            let mut keyed = Vec::with_capacity(rows.len());
            let mut ticked = Vec::new();
            for row in rows.iter().filter(|row| !row.worktree.is_main) {
                let target = &row.worktree.target;
                let key = wt_core::worktree_target_key(target);
                let pr = prs.and_then(|data| data.prs.get(&target.branch));
                let signature = row_signature(row, state, pr, &context).await;
                let serialized = serde_json::to_string(&signature).unwrap_or_default();
                let identity = fact_identity(row, state, pr, &context.config.branch.base);
                let epoch = activity_epoch(row, SystemTime::now());
                if let Some(cached) = cache.get(&key).filter(|cached| {
                    cached.signature == serialized
                        && cached
                            .retry_after
                            .is_none_or(|retry_after| retry_after > tokio::time::Instant::now())
                }) {
                    // Ref-derived facts are covered by the signature; only
                    // the working-tree diff can drift without it changing.
                    if cached.activity_epoch != epoch {
                        ticked.push((
                            key,
                            PathBuf::from(&target.path),
                            cached.anchors.merge_base.clone(),
                            epoch,
                        ));
                    }
                    continue;
                }
                let previous = cache
                    .get(&key)
                    .filter(|cached| cached.signature == serialized)
                    .map(|cached| cached.presentation.clone());
                if previous.is_none() {
                    // A changed ref, index, or rebase marker can invalidate
                    // facts without changing HEAD or the prepared row. Drop
                    // that proof before awaiting its replacement probes.
                    cache.remove(&key);
                }
                keyed.push((
                    key,
                    serialized,
                    identity,
                    row.clone(),
                    pr.cloned(),
                    previous,
                    epoch,
                ));
            }

            let current_keys = rows
                .iter()
                .filter(|row| !row.worktree.is_main)
                .map(|row| wt_core::worktree_target_key(&row.worktree.target))
                .collect::<std::collections::HashSet<_>>();
            cache.retain(|key, _| current_keys.contains(key));

            // Invalidation controls which probes must run, not which last
            // observation the display may show. Keep that observation and its
            // stamp until the replacement is ready. The overlay still rejects
            // facts whose checkout/HEAD/base/PR identity changed. This source
            // exposes only a display handle, never confirmed safety evidence.
            let retained = presentation_map(&cache);
            if last_published
                .as_ref()
                .is_some_and(|published| *published != retained)
            {
                publisher.set_state(SourceState::Refreshing);
                last_state = Some(SourceState::Refreshing);
            }

            let context_for_rows = context.clone();
            let state_for_rows = state.clone();
            let updated = stream::iter(keyed)
                .map(|(key, signature, identity, row, pr, previous, epoch)| {
                    let context = context_for_rows.clone();
                    let state = state_for_rows.clone();
                    let conflicts = conflicts.clone();
                    let cancellation = cancellation.child_token();
                    async move {
                        let (mut presentation, needs_retry, anchors) = enrich(
                            &context,
                            &row,
                            &state,
                            pr.as_ref(),
                            &conflicts,
                            &cancellation,
                        )
                        .await;
                        if needs_retry && let Some(previous) = previous {
                            let error = presentation.error.clone();
                            presentation = previous;
                            presentation.error = error;
                        }
                        (
                            key,
                            signature,
                            identity,
                            presentation,
                            needs_retry,
                            epoch,
                            anchors,
                        )
                    }
                })
                .buffer_unordered(MAX_CONCURRENT_ROWS)
                .collect::<Vec<_>>()
                .await;
            let refreshed = stream::iter(ticked)
                .map(|(key, path, merge_base, epoch)| {
                    let context = context_for_rows.clone();
                    let cancellation = cancellation.child_token();
                    async move {
                        let diff = match &merge_base {
                            Some(merge_base) => {
                                diff_stat(&context, &path, merge_base, &cancellation).await
                            }
                            None => None,
                        };
                        (key, diff, epoch)
                    }
                })
                .buffer_unordered(MAX_CONCURRENT_ROWS)
                .collect::<Vec<_>>()
                .await;
            if cancellation.is_cancelled() {
                break;
            }
            for (key, signature, identity, presentation, needs_retry, activity_epoch, anchors) in
                updated
            {
                cache.insert(
                    key,
                    CachedRow {
                        signature,
                        identity,
                        presentation,
                        retry_after: needs_retry
                            .then(|| tokio::time::Instant::now() + Duration::from_secs(3)),
                        activity_epoch,
                        anchors,
                    },
                );
            }
            for (key, diff, epoch) in refreshed {
                if let Some(cached) = cache.get_mut(&key) {
                    cached.activity_epoch = epoch;
                    if diff.is_some() {
                        cached.presentation.diff = diff;
                    }
                }
            }
            if let Ok(mut conflicts) = conflicts.lock() {
                let live = cache
                    .values()
                    .filter_map(|row| row.anchors.conflict_pair.as_ref())
                    .collect::<std::collections::HashSet<_>>();
                conflicts.retain(|pair, _| live.contains(pair));
            }

            let presentation = presentation_map(&cache);
            let mut state = SourceState::combine([
                ("Git", git_snapshot.state()),
                ("State", metadata_snapshot.state()),
                ("GitHub", github_snapshot.state()),
            ]);
            let failed_rows = presentation
                .values()
                .filter(|row| row.presentation.error.is_some())
                .count();
            if failed_rows > 0 && !matches!(state, SourceState::Failed(_)) {
                state = SourceState::Failed(
                    format!(
                        "Git presentation could not refresh {failed_rows} worktree(s); retrying"
                    )
                    .into(),
                );
            }
            if last_published.as_ref() != Some(&presentation) || last_state.as_ref() != Some(&state)
            {
                publisher.publish_live_with_state(presentation.clone(), observed, state.clone());
                last_published = Some(presentation);
                last_state = Some(state);
            }
        }
    });
    source.view()
}

/// Attach Git facts to rows by stable target key. Remote rows without a local
/// producer remain unchanged, including rows with matching local slugs.
pub fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    facts: SourceHandle<PresentationMap>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<Metadata>,
    github: SourceHandle<GithubData>,
    configured_base: String,
) -> SourceHandle<Board> {
    let (source, mut publisher) = derived_channel(scope, "git-presentation-board");
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut board_updates = board.subscribe();
        let mut fact_updates = facts.subscribe();
        let mut git_updates = git.subscribe();
        let mut metadata_updates = metadata.subscribe();
        let mut github_updates = github.subscribe();
        board_updates.mark_changed();
        fact_updates.mark_changed();
        git_updates.mark_changed();
        metadata_updates.mark_changed();
        github_updates.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh(); facts.refresh(); git.refresh(); metadata.refresh(); github.refresh(); continue;
                }
                changed = board_updates.changed() => if changed.is_err() { break; },
                changed = fact_updates.changed() => if changed.is_err() { break; },
                changed = git_updates.changed() => if changed.is_err() { break; },
                changed = metadata_updates.changed() => if changed.is_err() { break; },
                changed = github_updates.changed() => if changed.is_err() { break; },
            }
            let base = board_updates.borrow_and_update().clone();
            let fact_snapshot = fact_updates.borrow_and_update().clone();
            let git_snapshot = git_updates.borrow_and_update().clone();
            let metadata_snapshot = metadata_updates.borrow_and_update().clone();
            let github_snapshot = github_updates.borrow_and_update().clone();
            let current = current_identities(
                &git_snapshot,
                &metadata_snapshot,
                &github_snapshot,
                &configured_base,
            );
            let next = compose(base, &fact_snapshot, &current);
            publisher.publish_if_changed(next);
        }
    });
    source
}

fn compose(
    base: SourceSnapshot<Board>,
    facts: &SourceSnapshot<PresentationMap>,
    current: &HashMap<String, FactIdentity>,
) -> SourceSnapshot<Board> {
    // Unmatched cached facts do not contribute to the board. In particular,
    // another host's rows cannot inherit this host's restored provenance.
    let mut applied = false;
    let has_local_rows = base.data().is_some_and(|board| {
        board
            .rows
            .iter()
            .any(|row| !wt_core::is_remote_worktree_ledger_key(&row.key))
    });
    let prepared = base.data().map(|board| {
        let mut board = board.as_ref().clone();
        if let Some(data) = facts.data() {
            applied = apply_facts(&mut board, data, current);
        }
        Arc::new(board)
    });
    let state = match (base.state(), facts.state()) {
        (SourceState::Failed(error), _) => SourceState::Failed(error.clone()),
        (_, SourceState::Failed(error)) if has_local_rows => {
            applied = true; // The source error contributes visible state.
            SourceState::Failed(format!("Git presentation: {error}").into())
        }
        (state, _) => state.clone(),
    };
    let mut next = base.with_data(prepared).with_state(state);
    if applied {
        next = next.with_input(facts);
    }
    next
}

fn scanned_live(git: &SourceSnapshot<Vec<WorktreeSnapshot>>) -> bool {
    git.domains()
        .get(&Domain::Git)
        .is_some_and(|meta| meta.is_live())
}

fn presentation_map(cache: &HashMap<String, CachedRow>) -> PresentationMap {
    cache
        .iter()
        .map(|(key, cached)| {
            (
                key.clone(),
                GitFact {
                    identity: cached.identity.clone(),
                    presentation: cached.presentation.clone(),
                },
            )
        })
        .collect()
}

fn current_identities(
    git: &SourceSnapshot<Vec<WorktreeSnapshot>>,
    metadata: &SourceSnapshot<Metadata>,
    github: &SourceSnapshot<GithubData>,
    configured_base: &str,
) -> HashMap<String, FactIdentity> {
    let (Some(rows), Some((state, _))) = (git.data(), metadata.data().map(Arc::as_ref)) else {
        return HashMap::new();
    };
    // Without GitHub data the facts were computed with no PR, as here.
    let prs = github.data();
    rows.iter()
        .filter(|row| !row.worktree.is_main)
        .map(|row| {
            let target = &row.worktree.target;
            let key = wt_core::worktree_target_key(target);
            let pr = prs.and_then(|data| data.prs.get(&target.branch));
            (key, fact_identity(row, state, pr, configured_base))
        })
        .collect()
}

fn apply_facts(
    board: &mut Board,
    facts: &PresentationMap,
    current: &HashMap<String, FactIdentity>,
) -> bool {
    let mut applied = false;
    for row in &mut board.rows {
        if let Some(fact) = facts.get(&row.key)
            && current.get(&row.key) == Some(&fact.identity)
            && fact.identity.head.as_deref() == row.git.head_sha.as_deref()
        {
            applied = true;
            row.git.landed_on = fact.presentation.landed_on;
            row.git.rebasing = fact.presentation.rebasing;
            row.git
                .conflict_files
                .clone_from(&fact.presentation.conflict_files);
            row.git.pr_title.clone_from(&fact.presentation.pr_title);
            row.git
                .first_commit_title
                .clone_from(&fact.presentation.first_commit_title);
            row.git
                .base_conflicts
                .clone_from(&fact.presentation.base_conflicts);
            row.git.diff = fact.presentation.diff;
            row.git.last_commit_ms = fact.presentation.last_commit_ms;
            row.git.created_ms = fact.presentation.created_ms;
            row.git.base_ahead = fact.presentation.base_ahead;
            row.git.base_behind = fact.presentation.base_behind;
            if let Some(error) = &fact.presentation.error {
                row.git.error = Some(match row.git.error.take() {
                    Some(existing) if !existing.is_empty() => format!("{existing}; {error}"),
                    _ => error.clone(),
                });
            }
        }
    }
    applied
}

async fn row_signature<'a>(
    row: &'a WorktreeSnapshot,
    state: &'a Value,
    pr: Option<&'a PullRequest>,
    context: &AppContext,
) -> RowSignature<'a> {
    let worktree = &row.worktree;
    let (index, rebase_merge, rebase_apply, base_ref, production_ref, packed_refs) =
        if let Some(git_dir) = &worktree.git_dir {
            let common = worktree.common_dir.as_deref().unwrap_or(git_dir);
            let stored_base = state
                .get("slugs")
                .and_then(|slugs| slugs.get(worktree.target.slug()))
                .and_then(|entry| entry.get("baseBranch"))
                .and_then(Value::as_str)
                .filter(|base| !base.is_empty())
                .unwrap_or(&context.config.branch.base);
            let base_paths = ref_paths(common, stored_base);
            let production_paths = context
                .config
                .branch
                .production
                .as_deref()
                .map(|r| ref_paths(common, r))
                .unwrap_or_default();
            let index_path = git_dir.join("index");
            let rebase_merge_path = git_dir.join("rebase-merge");
            let rebase_apply_path = git_dir.join("rebase-apply");
            let mut future = Vec::new();
            future.push(file_stamp(&index_path));
            future.push(file_stamp(&rebase_merge_path));
            future.push(file_stamp(&rebase_apply_path));
            for path in &base_paths {
                future.push(file_stamp(path));
            }
            for path in &production_paths {
                future.push(file_stamp(path));
            }
            let mut stamps = futures_util::future::join_all(future).await.into_iter();
            let index = stamps.next().unwrap_or_default();
            let rebase_merge = stamps.next().unwrap_or_default();
            let rebase_apply = stamps.next().unwrap_or_default();
            let base_ref = base_paths
                .iter()
                .map(|_| stamps.next().unwrap_or_default())
                .collect();
            let production_ref = production_paths
                .iter()
                .map(|_| stamps.next().unwrap_or_default())
                .collect();
            let packed_refs = file_stamp(&common.join("packed-refs")).await;
            (
                index,
                rebase_merge,
                rebase_apply,
                base_ref,
                production_ref,
                packed_refs,
            )
        } else {
            (
                FileStamp::default(),
                FileStamp::default(),
                FileStamp::default(),
                Vec::new(),
                Vec::new(),
                FileStamp::default(),
            )
        };
    let status = row.status.as_ref().map(|status| StatusSignature {
        branch: status.branch.as_deref(),
        head: status.head_sha.as_deref(),
        upstream: status.upstream.as_deref(),
        ahead: status.ahead,
        behind: status.behind,
        tracked: status.tracked_changes,
        untracked: status.untracked_files,
    });
    RowSignature {
        key: wt_core::worktree_target_key(&worktree.target),
        path: &worktree.target.path,
        branch: &worktree.target.branch,
        head: row
            .status
            .as_ref()
            .and_then(|status| status.head_sha.as_deref())
            .or(worktree.head_sha.as_deref()),
        status,
        base_branch: state
            .get("slugs")
            .and_then(|slugs| slugs.get(worktree.target.slug()))
            .and_then(|entry| entry.get("baseBranch"))
            .and_then(Value::as_str),
        base_sha: state
            .get("slugs")
            .and_then(|slugs| slugs.get(worktree.target.slug()))
            .and_then(|entry| entry.get("baseSha"))
            .and_then(Value::as_str),
        pr: pr.map(|pr| PrSignature {
            title: &pr.title,
            state: &pr.state,
            base: &pr.base_ref_name,
            merge_oid: pr.merge_commit_oid.as_deref(),
            head_oid: pr.head_ref_oid.as_deref(),
        }),
        index,
        rebase_merge,
        rebase_apply,
        base_ref,
        production_ref,
        packed_refs,
    }
}

fn fact_identity(
    row: &WorktreeSnapshot,
    state: &Value,
    pr: Option<&PullRequest>,
    configured_base: &str,
) -> FactIdentity {
    let target = &row.worktree.target;
    let slug_state = state
        .get("slugs")
        .and_then(|slugs| slugs.get(target.slug()));
    FactIdentity {
        path: target.path.clone(),
        branch: target.branch.clone(),
        head: row
            .status
            .as_ref()
            .and_then(|status| status.head_sha.clone())
            .or_else(|| row.worktree.head_sha.clone()),
        base_branch: slug_state
            .and_then(|entry| entry.get("baseBranch"))
            .and_then(Value::as_str)
            .filter(|branch| !branch.is_empty())
            .unwrap_or(configured_base)
            .to_owned(),
        base_sha: slug_state
            .and_then(|entry| entry.get("baseSha"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        pr: pr.map(|pr| PrIdentity {
            number: pr.number,
            title: pr.title.clone(),
            head_oid: pr.head_ref_oid.clone(),
            state: pr.state.clone(),
            base: pr.base_ref_name.clone(),
            merge_oid: pr.merge_commit_oid.clone(),
        }),
    }
}

fn ref_paths(common_dir: &Path, reference: &str) -> Vec<PathBuf> {
    if reference.is_empty()
        || reference
            .split('/')
            .any(|part| part == ".." || part.is_empty())
    {
        return Vec::new();
    }
    let mut paths = Vec::new();
    if reference.starts_with("refs/") {
        paths.push(common_dir.join(reference));
    } else {
        paths.push(common_dir.join("refs/heads").join(reference));
        paths.push(common_dir.join("refs/remotes").join(reference));
        // The activity probes prefer the remote-tracking base, so its
        // movement must invalidate them too.
        paths.push(common_dir.join("refs/remotes/origin").join(reference));
    }
    paths
}

async fn file_stamp(path: &Path) -> FileStamp {
    match fs::metadata(path).await {
        Ok(metadata) => FileStamp {
            exists: true,
            len: metadata.len(),
            modified_ns: metadata.modified().ok().and_then(system_time_ns),
        },
        Err(_) => FileStamp::default(),
    }
}

fn system_time_ns(time: SystemTime) -> Option<u128> {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

async fn enrich(
    context: &AppContext,
    row: &WorktreeSnapshot,
    state: &Value,
    pr: Option<&PullRequest>,
    conflicts: &ConflictCache,
    cancellation: &CancellationToken,
) -> (GitPresentation, bool, ActivityAnchors) {
    let mut failures = Vec::new();
    let mut anchors = ActivityAnchors::default();
    let status = row.status.as_ref();
    let target = &row.worktree.target;
    let mut result = GitPresentation {
        head_sha: status
            .and_then(|status| status.head_sha.clone())
            .or_else(|| row.worktree.head_sha.clone()),
        tracked_changes: status.map(|status| status.tracked_changes),
        untracked_files: status.map(|status| status.untracked_files),
        upstream: status.and_then(|status| status.upstream.clone()),
        ahead: status.and_then(|status| status.ahead),
        behind: status.and_then(|status| status.behind),
        pr_title: pr
            .map(|pr| pr.title.clone())
            .filter(|title| !title.trim().is_empty()),
        ..GitPresentation::default()
    };
    let path = PathBuf::from(&target.path);
    if let Some(git_dir) = &row.worktree.git_dir {
        result.rebasing = fs::try_exists(git_dir.join("rebase-merge"))
            .await
            .unwrap_or(false)
            || fs::try_exists(git_dir.join("rebase-apply"))
                .await
                .unwrap_or(false);
    }

    if status.is_some() && !cancellation.is_cancelled() {
        match run_git(
            context,
            &path,
            ["status", "--porcelain=v2", "-z", "--untracked-files=no"],
            cancellation,
        )
        .await
        {
            Some(output) if output.status.success() && !output.stdout_truncated => {
                result.conflict_files = parse_unmerged_paths(&output.stdout);
            }
            _ => failures.push("working-tree conflict status"),
        }
    }

    let stored = state
        .get("slugs")
        .and_then(|slugs| slugs.get(target.slug()));
    let configured_base = stored
        .and_then(|entry| entry.get("baseBranch"))
        .and_then(Value::as_str)
        .filter(|base| !base.is_empty())
        .unwrap_or(&context.config.branch.base);
    let base_sha = stored
        .and_then(|entry| entry.get("baseSha"))
        .and_then(Value::as_str);
    let head = result.head_sha.clone();
    if let Some(head) = head.as_deref().filter(|head| !head.is_empty())
        && !cancellation.is_cancelled()
    {
        // A base that exists only as `origin/<base>` (or no longer exists)
        // must not fail every probe and retry forever: resolve it once.
        let base_ref = match resolve_branch_ref(context, &path, configured_base, cancellation).await
        {
            Ok(base_ref) => base_ref,
            Err(()) => {
                failures.push("base ref");
                None
            }
        };
        let anchor = base_sha
            .filter(|sha| !sha.is_empty())
            .or(base_ref.as_deref());
        if let Some(anchor) = anchor {
            let (first_commit_title, failed) =
                first_commit_title(context, &path, anchor, cancellation).await;
            result.first_commit_title = first_commit_title;
            if failed {
                failures.push("first commit title");
            }
        }
        let own_commits = if let Some(anchor) = anchor {
            let (count, failed) =
                async_count(context, &path, &format!("{anchor}..{head}"), cancellation).await;
            if failed {
                failures.push("own commit count");
            }
            count.unwrap_or(0)
        } else {
            0
        };
        if own_commits > 0 {
            let pr_is_for_current_head =
                pr.is_some_and(|pr| pr.head_ref_oid.as_deref() == Some(head));
            let pr_landed_on_base = pr_is_for_current_head
                && pr.is_some_and(|pr| {
                    pr.state.eq_ignore_ascii_case("MERGED") && pr.base_ref_name == configured_base
                });
            let local_landed_on_base = match base_ref.as_deref() {
                Some(base_ref) => {
                    let (landed, failed) =
                        is_ancestor(context, &path, head, base_ref, cancellation).await;
                    if failed {
                        failures.push("base ancestry");
                    }
                    landed
                }
                None => false,
            };
            if pr_landed_on_base || local_landed_on_base {
                result.landed_on = Some(LandingKind::Base);
            }
            if let Some(production) = context.config.branch.production.as_deref() {
                if production == configured_base && (pr_landed_on_base || local_landed_on_base) {
                    result.landed_on = Some(LandingKind::Production);
                } else {
                    let production_ref =
                        match resolve_branch_ref(context, &path, production, cancellation).await {
                            Ok(production_ref) => production_ref,
                            Err(()) => {
                                failures.push("production ref");
                                None
                            }
                        };
                    let mut head_on_production = false;
                    let mut merge_on_production = false;
                    if let Some(production_ref) = production_ref.as_deref() {
                        let (landed, head_failed) =
                            is_ancestor(context, &path, head, production_ref, cancellation).await;
                        if head_failed {
                            failures.push("production ancestry");
                        }
                        head_on_production = landed;
                        let merge_oid = pr.and_then(|pr| {
                            exact_merged_pr_oid(
                                pr.head_ref_oid.as_deref(),
                                head,
                                &pr.state,
                                pr.merge_commit_oid.as_deref(),
                            )
                        });
                        if let Some(merge_oid) = merge_oid {
                            let (landed, failed) = is_ancestor(
                                context,
                                &path,
                                merge_oid,
                                production_ref,
                                cancellation,
                            )
                            .await;
                            if failed {
                                failures.push("PR merge ancestry");
                            }
                            merge_on_production = landed;
                        }
                    }
                    if head_on_production || merge_on_production {
                        result.landed_on = Some(LandingKind::Production);
                    }
                }
            }
        }
        // Presentation-only activity facts. These are best effort: a
        // missing base ref or an old Git leaves them unknown rather than
        // failing the row, since nothing decides safety from them.
        if !cancellation.is_cancelled() {
            anchors = read_activity(
                context,
                &path,
                configured_base,
                &context.config.branch.base,
                head,
                &mut result,
                conflicts,
                cancellation,
            )
            .await;
        }
    }
    result.created_ms = created_ms(&path).await;
    let needs_retry = !failures.is_empty();
    if needs_retry {
        failures.sort_unstable();
        failures.dedup();
        result.error = Some(format!("Could not read {}; retrying", failures.join(", ")));
    }
    (result, needs_retry, anchors)
}

/// The refresh bucket for a dirty row's diff stat, `None` for a clean one.
fn activity_epoch(row: &WorktreeSnapshot, now: SystemTime) -> Option<u64> {
    let status = row.status.as_ref()?;
    (status.tracked_changes + status.untracked_files > 0).then(|| {
        now.duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs() / ACTIVITY_REFRESH.as_secs())
    })
}

/// Diff stat since the fork point (working tree included), newest commit
/// time, ahead/behind against the base, and a `merge-tree` conflict
/// pre-flight against the base. Returns the anchors a later cheap diff
/// refresh and the conflict cache reuse.
#[allow(clippy::too_many_arguments)]
async fn read_activity(
    context: &AppContext,
    path: &Path,
    base: &str,
    trunk: &str,
    head: &str,
    result: &mut GitPresentation,
    conflicts: &ConflictCache,
    cancel: &CancellationToken,
) -> ActivityAnchors {
    let mut anchors = ActivityAnchors::default();
    if let Some(output) =
        run_git(context, path, ["log", "-1", "--format=%ct", "HEAD"], cancel).await
        && output.status.success()
    {
        result.last_commit_ms = output
            .stdout_text()
            .trim()
            .parse::<u64>()
            .ok()
            .map(|seconds| seconds.saturating_mul(1000));
    }
    let Some((base, base_sha)) = resolve_probe_base(context, path, base, trunk, cancel).await
    else {
        return anchors;
    };
    if let Some(output) = run_git(
        context,
        path,
        [
            "rev-list",
            "--left-right",
            "--count",
            &format!("{base}...HEAD"),
        ],
        cancel,
    )
    .await
        && output.status.success()
        && let Some((behind, ahead)) = parse_left_right(&output.stdout_text())
    {
        result.base_behind = Some(behind);
        result.base_ahead = Some(ahead);
    }
    if let Some(output) = run_git(context, path, ["merge-base", &base, "HEAD"], cancel).await
        && output.status.success()
    {
        let merge_base = output.stdout_text().trim().to_owned();
        if !merge_base.is_empty() {
            result.diff = diff_stat(context, path, &merge_base, cancel).await;
            anchors.merge_base = Some(merge_base);
        }
    }
    // Mid-rebase HEAD is transient and the rebase itself is the fact.
    if !result.rebasing {
        let pair = (head.to_owned(), base_sha);
        let cached = conflicts
            .lock()
            .ok()
            .and_then(|cache| cache.get(&pair).cloned());
        result.base_conflicts = match cached {
            Some(known) => known,
            // A base already in HEAD's history rebases trivially.
            None if result.base_behind == Some(0) => Some(Vec::new()),
            None => match base_conflicts(context, path, &pair.1, &pair.0, cancel).await {
                ConflictProbe::Settled(value) => {
                    if let Ok(mut cache) = conflicts.lock() {
                        cache.insert(pair.clone(), value.clone());
                    }
                    value
                }
                ConflictProbe::Unknown => None,
            },
        };
        anchors.conflict_pair = Some(pair);
    }
    anchors
}

/// Working tree against the fork point plus untracked additions. These are
/// the only activity inputs that move without a ref or index change.
async fn diff_stat(
    context: &AppContext,
    path: &Path,
    merge_base: &str,
    cancel: &CancellationToken,
) -> Option<wt_tui::DiffStat> {
    let diff = run_git(context, path, ["diff", "--shortstat", merge_base], cancel)
        .await
        .filter(|output| output.status.success() && !output.stdout_truncated)?;
    let mut stat = parse_shortstat(&diff.stdout_text());
    let (files, lines) = untracked_lines(context, path, cancel).await?;
    stat.files = stat.files.saturating_add(files);
    stat.added = stat.added.saturating_add(lines);
    Some(stat)
}

enum ConflictProbe {
    /// A result that holds for this exact commit pair, including a
    /// known-unknown one from a Git that cannot run the probe.
    Settled(Option<Vec<String>>),
    /// A transient failure; probe again next time.
    Unknown,
}

/// `merge-tree --write-tree` (Git 2.38+) between two commits. A `--quiet`
/// pass first answers clean merges without writing tree objects; only a
/// conflicting pair, or a Git without `--quiet`, runs the listing pass that
/// writes them. Exit 129 from the listing means `--write-tree` itself is
/// unsupported.
async fn base_conflicts(
    context: &AppContext,
    path: &Path,
    base_sha: &str,
    head_sha: &str,
    cancel: &CancellationToken,
) -> ConflictProbe {
    let Some(quiet) = run_git(
        context,
        path,
        ["merge-tree", "--write-tree", "--quiet", base_sha, head_sha],
        cancel,
    )
    .await
    else {
        return ConflictProbe::Unknown;
    };
    match quiet.status.code() {
        Some(0) => return ConflictProbe::Settled(Some(Vec::new())),
        Some(1 | 129) => {}
        _ => return ConflictProbe::Unknown,
    }
    let Some(output) = run_git(
        context,
        path,
        [
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            base_sha,
            head_sha,
        ],
        cancel,
    )
    .await
    .filter(|output| !output.stdout_truncated) else {
        return ConflictProbe::Unknown;
    };
    if output.status.code() == Some(129) {
        return ConflictProbe::Settled(None);
    }
    parse_merge_tree(output.status.code(), &output.stdout).map_or(ConflictProbe::Unknown, |files| {
        ConflictProbe::Settled(Some(files))
    })
}

/// The ref a branch name resolves to in this checkout: the local branch,
/// else its remote-tracking copy. `Ok(None)` when neither exists, a settled
/// unknown rather than a read failure worth retrying; `Err` when Git failed.
async fn resolve_branch_ref(
    context: &AppContext,
    path: &Path,
    name: &str,
    cancel: &CancellationToken,
) -> Result<Option<String>, ()> {
    for candidate in [name.to_owned(), format!("origin/{name}")] {
        let output = run_git(
            context,
            path,
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{candidate}^{{commit}}"),
            ],
            cancel,
        )
        .await
        .ok_or(())?;
        match output.status.code() {
            Some(0) => return Ok(Some(candidate)),
            Some(1) => {}
            _ => return Err(()),
        }
    }
    Ok(None)
}

/// The ref the activity probes compare against. Trunk resolves to its
/// remote-tracking ref first, because a checkout's local trunk may never
/// move; a stacked parent prefers its local branch (which carries unpushed
/// commits), then its remote-tracking ref, then trunk.
async fn resolve_probe_base(
    context: &AppContext,
    path: &Path,
    base: &str,
    trunk: &str,
    cancel: &CancellationToken,
) -> Option<(String, String)> {
    let remote_trunk = format!("origin/{trunk}");
    let candidates = if base == trunk || base == remote_trunk {
        vec![remote_trunk, trunk.to_owned()]
    } else {
        vec![
            base.to_owned(),
            format!("origin/{base}"),
            remote_trunk,
            trunk.to_owned(),
        ]
    };
    for candidate in candidates {
        let output = run_git(
            context,
            path,
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{candidate}^{{commit}}"),
            ],
            cancel,
        )
        .await?;
        let sha = output.stdout_text().trim().to_owned();
        if output.status.success() && !sha.is_empty() {
            return Some((candidate, sha));
        }
    }
    None
}

/// Untracked, non-ignored files and their line counts: what they will add
/// once committed. `None` when the listing fails.
async fn untracked_lines(
    context: &AppContext,
    path: &Path,
    cancel: &CancellationToken,
) -> Option<(u32, u32)> {
    let output = run_git(
        context,
        path,
        ["ls-files", "--others", "--exclude-standard", "-z"],
        cancel,
    )
    .await
    .filter(|output| output.status.success() && !output.stdout_truncated)?;
    let names = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect::<Vec<_>>();
    let mut lines = 0u64;
    let mut budget = MAX_UNTRACKED_TOTAL_BYTES;
    let mut buffer = vec![0u8; 64 * 1024];
    for name in names.iter().take(MAX_UNTRACKED_FILES) {
        if cancel.is_cancelled() || budget == 0 {
            break;
        }
        let file = path.join(name);
        let Ok(metadata) = fs::symlink_metadata(&file).await else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_UNTRACKED_FILE_BYTES {
            continue;
        }
        let Ok(file) = fs::File::open(&file).await else {
            continue;
        };
        // The file may have grown since `symlink_metadata`; never read past
        // either bound.
        let mut reader = file.take(budget.min(MAX_UNTRACKED_FILE_BYTES));
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    budget = budget.saturating_sub(read as u64);
                    lines += buffer[..read].iter().filter(|byte| **byte == b'\n').count() as u64;
                }
            }
        }
    }
    let lines = u32::try_from(lines).unwrap_or(u32::MAX);
    Some((u32::try_from(names.len()).unwrap_or(u32::MAX), lines))
}

/// Checkout directory creation time: birth time where the filesystem
/// records one, otherwise the inode change time.
async fn created_ms(path: &Path) -> Option<u64> {
    let metadata = fs::metadata(path).await.ok()?;
    let created = metadata.created().ok().and_then(system_time_ns);
    #[cfg(unix)]
    let created = created.or_else(|| {
        use std::os::unix::fs::MetadataExt;
        u128::try_from(metadata.ctime())
            .ok()
            .map(|seconds| seconds * 1_000_000_000)
    });
    created.and_then(|ns| u64::try_from(ns / 1_000_000).ok())
}

/// `git rev-list --left-right --count base...HEAD` prints `behind\tahead`.
fn parse_left_right(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.split_whitespace();
    let behind = parts.next()?.parse().ok()?;
    let ahead = parts.next()?.parse().ok()?;
    Some((behind, ahead))
}

/// `git diff --shortstat`: ` 3 files changed, 10 insertions(+), 2 deletions(-)`.
/// Empty output is an empty diff.
fn parse_shortstat(text: &str) -> wt_tui::DiffStat {
    let mut stat = wt_tui::DiffStat::default();
    for part in text.trim().split(',') {
        let mut words = part.split_whitespace();
        let Some(count) = words.next().and_then(|count| count.parse::<u32>().ok()) else {
            continue;
        };
        match words.next() {
            Some(word) if word.starts_with("file") => stat.files = count,
            Some(word) if word.starts_with("insertion") => stat.added = count,
            Some(word) if word.starts_with("deletion") => stat.removed = count,
            _ => {}
        }
    }
    stat
}

/// `git merge-tree --write-tree --name-only -z`: exit 0 is clean; exit 1
/// with a tree OID on stdout is a conflict listing the paths after it.
/// Exit 1 with empty stdout (an unresolvable ref) or any other exit is
/// unknown.
fn parse_merge_tree(code: Option<i32>, stdout: &[u8]) -> Option<Vec<String>> {
    match code {
        Some(0) => Some(Vec::new()),
        Some(1)
            if stdout
                .iter()
                .any(|byte| !byte.is_ascii_whitespace() && *byte != 0) =>
        {
            let mut files = stdout
                .split(|byte| *byte == 0)
                .skip(1)
                .filter(|name| !name.is_empty())
                .map(|name| String::from_utf8_lossy(name).into_owned())
                .collect::<Vec<_>>();
            files.sort();
            files.dedup();
            Some(files)
        }
        _ => None,
    }
}

fn exact_merged_pr_oid<'a>(
    pr_head: Option<&'a str>,
    current_head: &str,
    pr_state: &str,
    merge_oid: Option<&'a str>,
) -> Option<&'a str> {
    (pr_head == Some(current_head) && pr_state.eq_ignore_ascii_case("MERGED"))
        .then_some(merge_oid)
        .flatten()
}

async fn first_commit_title(
    context: &AppContext,
    path: &Path,
    base: &str,
    cancel: &CancellationToken,
) -> (Option<String>, bool) {
    let Some(output) = run_git(
        context,
        path,
        ["log", "--format=%s", "--reverse", &format!("{base}..HEAD")],
        cancel,
    )
    .await
    else {
        return (None, true);
    };
    if !output.status.success() || output.stdout_truncated {
        return (None, true);
    }
    (
        output
            .stdout_text()
            .lines()
            .next()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned),
        false,
    )
}

async fn async_count(
    context: &AppContext,
    path: &Path,
    range: &str,
    cancel: &CancellationToken,
) -> (Option<u64>, bool) {
    let Some(output) = run_git(context, path, ["rev-list", "--count", range], cancel).await else {
        return (None, true);
    };
    if !output.status.success() || output.stdout_truncated {
        return (None, true);
    }
    let count = output.stdout_text().trim().parse().ok();
    (count, count.is_none())
}

async fn is_ancestor(
    context: &AppContext,
    path: &Path,
    ancestor: &str,
    descendant: &str,
    cancel: &CancellationToken,
) -> (bool, bool) {
    let Some(output) = run_git(
        context,
        path,
        ["merge-base", "--is-ancestor", ancestor, descendant],
        cancel,
    )
    .await
    else {
        return (false, true);
    };
    if output.status.success() {
        (true, false)
    } else if output.status.code() == Some(1) {
        (false, false)
    } else {
        (false, true)
    }
}

async fn run_git<I, S>(
    context: &AppContext,
    path: &Path,
    args: I,
    cancel: &CancellationToken,
) -> Option<wt_platform::process::ProcessOutput>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let mut spec = CommandSpec::new("git").args(args);
    spec.cwd = Some(path.to_path_buf());
    spec.env = vec![
        ("GIT_OPTIONAL_LOCKS".into(), Some("0".into())),
        ("GIT_TERMINAL_PROMPT".into(), Some("0".into())),
        ("LC_ALL".into(), Some("C".into())),
    ];
    spec.timeout = GIT_TIMEOUT;
    spec.output_limit = GIT_OUTPUT_LIMIT;
    context.processes.run(spec, cancel).await.ok()
}

fn parse_unmerged_paths(bytes: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| record.first() == Some(&b'u'))
    {
        let mut spaces = 0;
        let mut path_start = None;
        for (index, byte) in record.iter().enumerate() {
            if *byte == b' ' {
                spaces += 1;
                if spaces == 10 {
                    path_start = Some(index + 1);
                    break;
                }
            }
        }
        if let Some(start) = path_start
            && start < record.len()
        {
            paths.push(String::from_utf8_lossy(&record[start..]).into_owned());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use wt_runtime::{DerivedPublisher, SourceSnapshot};
    use wt_tui::BoardRow;

    #[test]
    fn unused_restored_facts_do_not_mark_the_board_restored() {
        let identity = FactIdentity {
            path: "/one".into(),
            branch: "one".into(),
            head: Some("head".into()),
            base_branch: "main".into(),
            base_sha: None,
            pr: None,
        };
        let facts = SourceSnapshot::test_restored(
            PresentationMap::from([(
                "one".into(),
                GitFact {
                    identity: identity.clone(),
                    presentation: GitPresentation::default(),
                },
            )]),
            Domain::Presentation,
            1,
        );
        let mut board = Board {
            rows: vec![BoardRow {
                key: "one".into(),
                git: GitPresentation {
                    head_sha: Some("head".into()),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let live = |board| SourceSnapshot::test_live(Some(board), SourceState::Ready, Domain::Git);
        // Missing/mismatched identity means the cache supplied no visible fact.
        let unused = compose(live(board.clone()), &facts, &HashMap::new());
        assert!(!unused.is_restored());
        assert!(!unused.domains().contains_key(&Domain::Presentation));
        let identities = HashMap::from([("one".into(), identity)]);
        assert!(compose(live(board.clone()), &facts, &identities).is_restored());
        board.rows[0].key = wt_core::remote_worktree_ledger_key("remote", "one");
        assert!(!compose(live(board.clone()), &facts, &identities).is_restored());
        let remote_failure = compose(
            live(board.clone()),
            &facts
                .clone()
                .with_state(SourceState::Failed("local probe failed".into())),
            &identities,
        );
        assert_eq!(*remote_failure.state(), SourceState::Ready);
        assert!(!remote_failure.is_restored());
        board.rows.clear();
        assert!(!compose(live(board.clone()), &facts, &identities).is_restored());
        let failed = facts.with_state(SourceState::Failed("probe failed".into()));
        let irrelevant = compose(live(board), &failed, &identities);
        assert_eq!(*irrelevant.state(), SourceState::Ready);
        assert!(!irrelevant.is_restored());
        let local = Board {
            rows: vec![BoardRow {
                key: "one".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let result = compose(live(local), &failed, &HashMap::new());
        assert!(
            matches!(result.state(), SourceState::Failed(error) if error.contains("probe failed"))
        );
        assert!(result.domains().contains_key(&Domain::Presentation));
    }

    fn linked(slug: &str) -> WorktreeSnapshot {
        WorktreeSnapshot {
            worktree: wt_vcs::WorktreeRecord {
                target: wt_core::local_worktree_target(slug, slug, format!("/repo/{slug}"), ""),
                is_main: false,
                head_sha: Some("head".into()),
                detached: false,
                kind: wt_vcs::RepositoryKind::LinkedWorktree,
                locked: false,
                prunable: false,
                git_dir: None,
                common_dir: None,
            },
            status: None,
            error: None,
        }
    }

    /// GitHub that never produced data (unauthenticated, offline, no
    /// GitHub origin) must not hide facts that were computed without a PR.
    #[test]
    fn facts_apply_while_github_has_no_data() {
        let git =
            SourceSnapshot::test_live(Some(vec![linked("alpha")]), SourceState::Ready, Domain::Git);
        let metadata = SourceSnapshot::test_live(
            Some((serde_json::json!({}), Default::default())),
            SourceState::Ready,
            Domain::Metadata,
        );
        let github = SourceSnapshot::<GithubData>::test_live(
            None,
            SourceState::Failed("gh auth".into()),
            Domain::Github,
        );
        let identities = current_identities(&git, &metadata, &github, "main");
        let identity = identities
            .get("alpha")
            .expect("identity without GitHub data");
        assert_eq!(identity.pr, None);
        assert_eq!(
            *identity,
            fact_identity(
                &git.data().unwrap()[0],
                &serde_json::json!({}),
                None,
                "main"
            )
        );
    }

    #[test]
    fn only_a_live_git_scan_keys_fact_probes() {
        let rows = vec![linked("alpha")];
        assert!(scanned_live(&SourceSnapshot::test_live(
            Some(rows.clone()),
            SourceState::Ready,
            Domain::Git
        )));
        assert!(!scanned_live(&SourceSnapshot::test_restored(
            rows,
            Domain::Git,
            1
        )));
    }

    #[test]
    fn porcelain_unmerged_paths_preserve_spaces_and_deduplicate() {
        let parsed = parse_unmerged_paths(
            b"u UU N... 100644 100644 100644 100644 a b c file with spaces.rs\0u UU N... 100644 100644 100644 100644 a b c second.rs\0? ignored.txt\0",
        );
        assert_eq!(parsed, ["file with spaces.rs", "second.rs"]);
    }

    #[test]
    fn a_reused_branch_pr_merge_oid_cannot_mark_the_new_head_landed() {
        assert_eq!(
            exact_merged_pr_oid(Some("old-head"), "new-head", "MERGED", Some("merge")),
            None
        );
        assert_eq!(
            exact_merged_pr_oid(Some("new-head"), "new-head", "OPEN", Some("merge")),
            None
        );
        assert_eq!(
            exact_merged_pr_oid(Some("new-head"), "new-head", "MERGED", Some("merge")),
            Some("merge")
        );
    }

    #[test]
    fn matching_facts_preserve_inventory_and_changed_inputs_invalidate_enrichment() {
        let key = "alpha".to_owned();
        let identity = FactIdentity {
            path: "/repo/alpha".into(),
            branch: "alpha".into(),
            head: Some("head".into()),
            base_branch: "main".into(),
            base_sha: Some("base-1".into()),
            pr: Some(PrIdentity {
                number: 7,
                title: "PR".into(),
                head_oid: Some("head".into()),
                state: "OPEN".into(),
                base: "main".into(),
                merge_oid: None,
            }),
        };
        let facts = PresentationMap::from([(
            key.clone(),
            GitFact {
                identity: identity.clone(),
                presentation: GitPresentation {
                    head_sha: Some("head".into()),
                    tracked_changes: Some(1),
                    untracked_files: Some(0),
                    upstream: Some("stale/upstream".into()),
                    ahead: Some(1),
                    behind: Some(2),
                    landed_on: Some(LandingKind::Base),
                    pr_title: Some("enriched title".into()),
                    ..GitPresentation::default()
                },
            },
        )]);
        let mut board = Board {
            rows: vec![BoardRow {
                key: key.clone(),
                git: GitPresentation {
                    head_sha: Some("head".into()),
                    tracked_changes: Some(9),
                    untracked_files: Some(3),
                    upstream: Some("origin/alpha".into()),
                    ahead: Some(5),
                    behind: Some(6),
                    ..GitPresentation::default()
                },
                ..BoardRow::default()
            }],
            ..Board::default()
        };
        apply_facts(
            &mut board,
            &facts,
            &HashMap::from([(key.clone(), identity.clone())]),
        );
        let git = &board.rows[0].git;
        assert_eq!(git.tracked_changes, Some(9));
        assert_eq!(git.untracked_files, Some(3));
        assert_eq!(git.upstream.as_deref(), Some("origin/alpha"));
        assert_eq!(git.ahead, Some(5));
        assert_eq!(git.behind, Some(6));
        assert_eq!(git.landed_on, Some(LandingKind::Base));
        assert_eq!(git.pr_title.as_deref(), Some("enriched title"));

        let mut changed_base = identity.clone();
        changed_base.base_sha = Some("base-2".into());
        let mut board = Board {
            rows: vec![BoardRow {
                key: key.clone(),
                git: GitPresentation {
                    head_sha: Some("head".into()),
                    ..GitPresentation::default()
                },
                ..BoardRow::default()
            }],
            ..Board::default()
        };
        apply_facts(
            &mut board,
            &facts,
            &HashMap::from([(key.clone(), changed_base)]),
        );
        assert_eq!(board.rows[0].git.landed_on, None);

        let mut changed_pr = identity.clone();
        changed_pr.pr.as_mut().unwrap().state = "MERGED".into();
        apply_facts(
            &mut board,
            &facts,
            &HashMap::from([(key.clone(), changed_pr)]),
        );
        assert_eq!(board.rows[0].git.landed_on, None);
    }

    #[tokio::test]
    async fn row_signature_changes_for_index_ref_and_rebase_state() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let rows = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let row = rows.iter().find(|row| !row.worktree.is_main).unwrap();
        let state = serde_json::json!({"slugs": {}});
        let before = row_signature(row, &state, None, &fixture.ctx).await;
        let output = std::process::Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(&row.worktree.target.path)
            .output()
            .unwrap();
        assert!(output.status.success());
        let after = row_signature(row, &state, None, &fixture.ctx).await;
        assert_ne!(
            serde_json::to_string(&before).unwrap(),
            serde_json::to_string(&after).unwrap()
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn reprobe_retains_display_facts_and_original_stamp_until_replacement() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let rows = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let row = rows.iter().find(|row| !row.worktree.is_main).unwrap();
        let key = wt_core::worktree_target_key(&row.worktree.target);
        let scope = TaskScope::new();
        let (git, git_pub) = derived_channel(&scope, "test-git");
        let (metadata, metadata_pub) = derived_channel(&scope, "test-metadata");
        let (github, github_pub) = derived_channel(&scope, "test-github");
        git_pub.publish(SourceSnapshot::test_live(
            Some(rows.clone()),
            SourceState::Ready,
            Domain::Git,
        ));
        metadata_pub.publish(SourceSnapshot::test_live(
            Some((serde_json::json!({"slugs": {}}), Default::default())),
            SourceState::Ready,
            Domain::Metadata,
        ));
        github_pub.publish(SourceSnapshot::test_live(
            Some(GithubData::default()),
            SourceState::Ready,
            Domain::Github,
        ));
        let facts = start(&scope, &fixture.ctx, git, metadata, github);
        let mut updates = facts.subscribe();
        tokio::time::timeout(
            Duration::from_secs(10),
            updates
                .wait_for(|snapshot| snapshot.data().is_some_and(|data| data.contains_key(&key))),
        )
        .await
        .unwrap()
        .unwrap();
        let before = updates.borrow_and_update().clone();
        let stamp = before
            .domains()
            .get(&Domain::Presentation)
            .unwrap()
            .observed();
        // A ref/index/rebase invalidation must not emit an empty intermediate
        // row. Such a row made merged/production glyphs alternate with ready.
        std::fs::create_dir(row.worktree.git_dir.as_ref().unwrap().join("rebase-merge")).unwrap();
        git_pub.publish(SourceSnapshot::test_live(
            Some(rows.clone()),
            SourceState::Ready,
            Domain::Git,
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut saw_refresh = false;
            loop {
                updates.changed().await.unwrap();
                let snapshot = updates.borrow_and_update().clone();
                assert!(
                    snapshot.data().unwrap().contains_key(&key),
                    "display fact disappeared during reprobe"
                );
                if matches!(snapshot.state(), SourceState::Refreshing) {
                    saw_refresh = true;
                    assert_eq!(snapshot.data(), before.data());
                    assert_eq!(
                        snapshot
                            .domains()
                            .get(&Domain::Presentation)
                            .unwrap()
                            .observed(),
                        stamp
                    );
                } else if saw_refresh {
                    break;
                }
            }
        })
        .await
        .unwrap();
        scope.shutdown(Duration::from_secs(2)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[test]
    fn activity_parsers_read_git_output() {
        assert_eq!(
            parse_shortstat(" 3 files changed, 10 insertions(+), 2 deletions(-)\n"),
            wt_tui::DiffStat {
                files: 3,
                added: 10,
                removed: 2
            }
        );
        assert_eq!(
            parse_shortstat(" 1 file changed, 1 deletion(-)"),
            wt_tui::DiffStat {
                files: 1,
                added: 0,
                removed: 1
            }
        );
        assert_eq!(parse_shortstat(""), wt_tui::DiffStat::default());
        assert_eq!(parse_left_right("4\t7\n"), Some((4, 7)));
        assert_eq!(parse_left_right("garbage"), None);
        assert_eq!(parse_merge_tree(Some(0), b"abc\0"), Some(Vec::new()));
        assert_eq!(
            parse_merge_tree(Some(1), b"abc\0b.rs\0a b.rs\0"),
            Some(vec!["a b.rs".to_owned(), "b.rs".to_owned()])
        );
        // An unresolvable base also exits 1, but writes no tree.
        assert_eq!(parse_merge_tree(Some(1), b""), None);
        assert_eq!(parse_merge_tree(Some(128), b"abc\0"), None);
    }

    /// A recorded base that exists only as a remote-tracking ref, or not at
    /// all, settles instead of failing every probe and retrying forever.
    #[tokio::test]
    async fn an_origin_only_or_missing_base_does_not_retry_forever() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}: {output:?}");
        };
        let main = fixture.ctx.config.paths.main_clone.clone();
        git(
            &main,
            &["update-ref", "refs/remotes/origin/coworker", "HEAD"],
        );
        let rows = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let row = rows
            .iter()
            .find(|row| row.worktree.target.branch == "feature/one")
            .unwrap();
        let worktree = PathBuf::from(&row.worktree.target.path);
        std::fs::write(worktree.join("tracked.txt"), "feature\n").unwrap();
        git(&worktree, &["commit", "-qam", "feature change"]);
        let rows = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let row = rows
            .iter()
            .find(|row| row.worktree.target.branch == "feature/one")
            .unwrap();
        let slug = row.worktree.target.slug().to_owned();
        for base in ["coworker", "deleted-by-hand"] {
            let state = serde_json::json!({"slugs": {slug.clone(): {"baseBranch": base}}});
            let (presentation, needs_retry, _) = enrich(
                &fixture.ctx,
                row,
                &state,
                None,
                &ConflictCache::default(),
                &fixture.ctx.cancellation,
            )
            .await;
            assert!(!needs_retry, "{base}: {:?}", presentation.error);
            assert_eq!(presentation.error, None);
            assert_eq!(presentation.landed_on, None);
        }
        let state = serde_json::json!({"slugs": {slug.clone(): {"baseBranch": "coworker"}}});
        let (presentation, _, _) = enrich(
            &fixture.ctx,
            row,
            &state,
            None,
            &ConflictCache::default(),
            &fixture.ctx.cancellation,
        )
        .await;
        assert_eq!(
            presentation.first_commit_title.as_deref(),
            Some("feature change")
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn enrichment_reads_diff_activity_base_counts_and_conflicts() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}: {output:?}");
        };
        let main = fixture.ctx.config.paths.main_clone.clone();
        let rows = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let worktree = PathBuf::from(
            &rows
                .iter()
                .find(|row| row.worktree.target.branch == "feature/one")
                .unwrap()
                .worktree
                .target
                .path,
        );
        std::fs::write(worktree.join("tracked.txt"), "feature\n").unwrap();
        git(&worktree, &["commit", "-qam", "feature change"]);
        std::fs::write(main.join("tracked.txt"), "trunk\n").unwrap();
        git(&main, &["commit", "-qam", "trunk change"]);
        std::fs::write(worktree.join("new.txt"), "one\ntwo\n").unwrap();
        let rows = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let row = rows
            .iter()
            .find(|row| row.worktree.target.branch == "feature/one")
            .unwrap();
        let state = serde_json::json!({"slugs": {}});
        let conflicts = ConflictCache::default();
        let (presentation, _, anchors) = enrich(
            &fixture.ctx,
            row,
            &state,
            None,
            &conflicts,
            &fixture.ctx.cancellation,
        )
        .await;
        assert_eq!(
            presentation.diff,
            Some(wt_tui::DiffStat {
                files: 2,
                added: 3,
                removed: 1
            })
        );
        assert_eq!(presentation.base_ahead, Some(1));
        assert_eq!(presentation.base_behind, Some(1));
        assert_eq!(
            presentation.base_conflicts.as_deref(),
            Some(&["tracked.txt".to_owned()][..])
        );
        assert!(presentation.last_commit_ms.is_some_and(|ms| ms > 0));
        assert!(presentation.created_ms.is_some_and(|ms| ms > 0));

        // The probe result is cached for the exact commit pair, and a cached
        // entry answers later passes without probing again.
        let pair = anchors.conflict_pair.clone().unwrap();
        assert_eq!(pair.0, presentation.head_sha.clone().unwrap());
        assert_eq!(
            conflicts.lock().unwrap().get(&pair).cloned(),
            Some(Some(vec!["tracked.txt".to_owned()]))
        );
        conflicts
            .lock()
            .unwrap()
            .insert(pair.clone(), Some(vec!["cached.rs".into()]));
        let (cached, _, _) = enrich(
            &fixture.ctx,
            row,
            &state,
            None,
            &conflicts,
            &fixture.ctx.cancellation,
        )
        .await;
        assert_eq!(
            cached.base_conflicts.as_deref(),
            Some(&["cached.rs".to_owned()][..])
        );

        // The epoch refresh reruns only the working-tree diff from the
        // recorded fork point.
        std::fs::write(worktree.join("new.txt"), "one\ntwo\nthree\n").unwrap();
        let merge_base = anchors.merge_base.unwrap();
        assert_eq!(
            diff_stat(
                &fixture.ctx,
                &worktree,
                &merge_base,
                &fixture.ctx.cancellation
            )
            .await,
            Some(wt_tui::DiffStat {
                files: 2,
                added: 4,
                removed: 1
            })
        );

        // An unresolvable commit is unknown, never clean, and is not cached.
        assert!(matches!(
            base_conflicts(
                &fixture.ctx,
                &worktree,
                "0000000000000000000000000000000000000001",
                &pair.0,
                &fixture.ctx.cancellation,
            )
            .await,
            ConflictProbe::Unknown
        ));
        // A clean pair settles without listing.
        assert!(matches!(
            base_conflicts(
                &fixture.ctx,
                &worktree,
                &pair.0,
                &pair.0,
                &fixture.ctx.cancellation,
            )
            .await,
            ConflictProbe::Settled(Some(files)) if files.is_empty()
        ));
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn untracked_line_counting_stops_at_the_row_byte_budget() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let rows = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let worktree = PathBuf::from(
            &rows
                .iter()
                .find(|row| row.worktree.target.branch == "feature/one")
                .unwrap()
                .worktree
                .target
                .path,
        );
        // Five files of just under the per-file cap exceed the row budget.
        let per_file = usize::try_from(MAX_UNTRACKED_FILE_BYTES).unwrap() - 1;
        for index in 0..5 {
            std::fs::write(
                worktree.join(format!("big-{index}.txt")),
                vec![b'\n'; per_file],
            )
            .unwrap();
        }
        let (files, lines) = untracked_lines(&fixture.ctx, &worktree, &fixture.ctx.cancellation)
            .await
            .unwrap();
        assert_eq!(files, 5);
        assert_eq!(u64::from(lines), MAX_UNTRACKED_TOTAL_BYTES);
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn overlay_publishes_board_without_waiting_for_git_facts() {
        let scope = TaskScope::new();
        let (board_input, board_publisher) = derived_channel(&scope, "test-board");
        let (facts_input, _facts_publisher) = derived_channel(&scope, "test-facts");
        let (git_input, _git_publisher) = derived_channel(&scope, "test-git");
        let (metadata_input, _metadata_publisher) = derived_channel(&scope, "test-metadata");
        let (github_input, _github_publisher) = derived_channel(&scope, "test-github");
        let output = overlay(
            &scope,
            board_input.clone(),
            facts_input,
            git_input,
            metadata_input,
            github_input,
            "main".into(),
        );
        let mut updates = output.subscribe();

        let publish_board = |publisher: &DerivedPublisher<Board>, title: &str, head: &str| {
            publisher.publish(SourceSnapshot::test_live(
                Some(Board {
                    rows: vec![BoardRow {
                        key: "alpha".into(),
                        title: title.into(),
                        git: GitPresentation {
                            head_sha: Some(head.into()),
                            ..GitPresentation::default()
                        },
                        ..BoardRow::default()
                    }],
                    ..Board::default()
                }),
                SourceState::Ready,
                Domain::Git,
            ));
        };
        publish_board(&board_publisher, "first title", "old-head");
        tokio::time::timeout(
            Duration::from_secs(1),
            updates.wait_for(|snapshot| {
                snapshot.data().is_some_and(|board| {
                    board
                        .rows
                        .first()
                        .is_some_and(|row| row.title == "first title")
                })
            }),
        )
        .await
        .unwrap()
        .unwrap();

        publish_board(&board_publisher, "updated title", "new-head");
        tokio::time::timeout(
            Duration::from_secs(1),
            updates.wait_for(|snapshot| {
                snapshot.data().is_some_and(|board| {
                    board.rows.first().is_some_and(|row| {
                        row.title == "updated title"
                            && row.git.head_sha.as_deref() == Some("new-head")
                    })
                })
            }),
        )
        .await
        .unwrap()
        .unwrap();

        let current = updates.borrow().data().unwrap().rows[0].clone();
        assert_eq!(current.title, "updated title");
        assert_eq!(current.git.head_sha.as_deref(), Some("new-head"));
        assert_eq!(current.git.landed_on, None);
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn git_presentation_facts_meet_the_persisted_source_contract() {
        let sample = PresentationMap::from([(
            "feature-one".to_owned(),
            GitFact {
                identity: FactIdentity {
                    path: "/repo/worktrees/feature-one".into(),
                    branch: "feature/one".into(),
                    head: Some("4b825dc642cb6eb9a060e54bf8d69288fbee4904".into()),
                    base_branch: "main".into(),
                    base_sha: Some("e69de29bb2d1d6434b8b29ae775ad8c2e48c5391".into()),
                    pr: Some(PrIdentity {
                        number: 812,
                        title: "Batch GitHub reads".into(),
                        head_oid: Some("4b825dc642cb6eb9a060e54bf8d69288fbee4904".into()),
                        state: "OPEN".into(),
                        base: "main".into(),
                        merge_oid: None,
                    }),
                },
                presentation: GitPresentation {
                    head_sha: Some("4b825dc642cb6eb9a060e54bf8d69288fbee4904".into()),
                    pr_title: Some("Batch GitHub reads".into()),
                    first_commit_title: Some("Batch reads by chunk".into()),
                    base_conflicts: Some(Vec::new()),
                    diff: Some(wt_tui::DiffStat {
                        files: 4,
                        added: 120,
                        removed: 14,
                    }),
                    last_commit_ms: Some(1_760_000_000_000),
                    created_ms: Some(1_759_900_000_000),
                    base_ahead: Some(3),
                    base_behind: Some(1),
                    ..GitPresentation::default()
                },
            },
        )]);
        wt_runtime::conformance::persisted_source("git-presentation", Domain::Presentation, sample)
            .await;
    }
}
