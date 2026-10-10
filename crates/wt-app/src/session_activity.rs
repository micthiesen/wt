//! Bounded host-local activity polling for live harness sessions.
//!
//! This source never invalidates Git. It publishes only when a tail, event, or
//! usage value changes; an empty poll is intentionally invisible to the TUI.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wt_core::HarnessId;
use wt_harness::{
    ActivityKind, ClaudeEventParser, ClaudePaths, CodexActivityTracker, CodexEventLevel,
    CodexHarness, CodexOutputTracker, CodexPaths, HarnessOutputKind, HarnessOutputTarget,
    OpenCodeActivityTracker, OpenCodeEventLevel, OpenCodeHarness, OpenCodeOutputTracker,
    OpenCodePaths, session_jsonl_path,
};
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel};
use wt_tmux::{TmuxClient, TmuxServer};

use crate::{
    context::AppContext,
    harness::{AgentTarget, AppHarness},
    session_source::{SessionDiscoveries, SessionInventory, SessionKey, SessionSources},
};

const ACTIVITY_INTERVAL: Duration = Duration::from_millis(2_500);
const USAGE_INTERVAL: Duration = Duration::from_secs(60);
const MAX_READ_PER_TICK: usize = 32 * 1024;
const MAX_PENDING_LINE: usize = 256 * 1024;
const MAX_SEED_BYTES: u64 = 64 * 1024;
const MAX_DELTA_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LINES_PER_SESSION: usize = 1_000;
const MAX_EVENT_LINES: usize = 500;

/// Coalesced notifications for the exact active Codex rollout files. Watching
/// their parent directories avoids recursive session-tree watches; the event
/// handler filters back to retained UUID paths. The 2.5s source timer remains
/// a recovery path for dropped or unsupported notifications.
struct CodexOutputWatch {
    watcher: Option<RecommendedWatcher>,
    watched_paths: Arc<RwLock<BTreeSet<PathBuf>>>,
    directories: BTreeSet<PathBuf>,
    updates: mpsc::Receiver<()>,
}

impl CodexOutputWatch {
    fn new() -> Self {
        let (sender, updates) = mpsc::channel(1);
        let watched_paths = Arc::new(RwLock::new(BTreeSet::new()));
        let callback_paths = Arc::clone(&watched_paths);
        let watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            let should_wake = match result {
                Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => false,
                Ok(event) => {
                    let Ok(paths) = callback_paths.read() else {
                        let _ = sender.try_send(());
                        return;
                    };
                    event.paths.iter().any(|changed| {
                        paths.contains(changed)
                            || std::fs::canonicalize(changed)
                                .ok()
                                .is_some_and(|changed| paths.contains(&changed))
                    })
                }
                Err(_) => true,
            };
            if should_wake {
                // A full channel means one wake is already pending. The tailer
                // reads the latest bounded delta, so intermediate writes need
                // no individual queue entry.
                let _ = sender.try_send(());
            }
        });
        let watcher = match watcher {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                tracing::warn!(%error, "could not create Codex rollout watcher; polling fallback remains active");
                None
            }
        };
        Self {
            watcher,
            watched_paths,
            directories: BTreeSet::new(),
            updates,
        }
    }

    fn sync(&mut self, paths: impl Iterator<Item = PathBuf>) {
        let paths = paths
            .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
            .collect::<BTreeSet<_>>();
        let next_directories = paths
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect::<BTreeSet<_>>();
        if let Some(watcher) = self.watcher.as_mut() {
            for directory in self.directories.difference(&next_directories) {
                let _ = watcher.unwatch(directory);
            }
            for directory in next_directories.difference(&self.directories) {
                if let Err(error) = watcher.watch(directory, RecursiveMode::NonRecursive) {
                    tracing::warn!(path = %directory.display(), %error, "could not watch Codex rollout directory; polling fallback remains active");
                }
            }
        }
        self.directories = next_directories;
        if let Ok(mut watched) = self.watched_paths.write() {
            *watched = paths;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivityWake {
    Source,
    Backstop,
    CodexOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityKindDto {
    Info,
    User,
    Assistant,
    Thinking,
    Tool,
    ToolOk,
    ToolError,
    Dim,
    Ok,
    Warn,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityLineDto {
    pub id: u64,
    pub timestamp_ms: i64,
    pub kind: ActivityKindDto,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEvent {
    pub id: u64,
    pub timestamp_ms: i64,
    pub slug: String,
    pub harness: HarnessId,
    pub level: ActivityKindDto,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTailSnapshot {
    pub key: SessionKey,
    pub lines: Vec<ActivityLineDto>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsagePeriodDto {
    pub utilization: f64,
    pub resets_at: Option<String>,
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub claude_five_hour: Option<UsagePeriodDto>,
    pub claude_seven_day: Option<UsagePeriodDto>,
    pub claude_seven_day_scoped: Vec<UsagePeriodDto>,
    pub codex_five_hour: Option<UsagePeriodDto>,
    pub codex_seven_day: Option<UsagePeriodDto>,
    pub codex_plan_type: Option<String>,
    pub opencode_five_hour: Option<f64>,
    pub opencode_seven_day: Option<f64>,
    pub cached_at_ms: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionActivitySnapshot {
    pub tails: Vec<SessionTailSnapshot>,
    pub events: VecDeque<SessionEvent>,
    pub usage: Option<UsageSnapshot>,
}

pub struct SessionActivitySources {
    pub activity: SourceHandle<SessionActivitySnapshot>,
}

#[derive(Default)]
struct ActivityCursors {
    claude: BTreeMap<SessionKey, ClaudeTailCursor>,
    codex: CodexActivityTracker,
    opencode: OpenCodeActivityTracker,
    codex_output: CodexOutputTracker,
    opencode_output: OpenCodeOutputTracker,
    next_event_id: u64,
}

pub fn start(
    scope: &TaskScope,
    context: &AppContext,
    sessions: &SessionSources,
) -> SessionActivitySources {
    let (activity, publisher) = source_channel();
    let app = AppHarness::new(context);
    let paths = ClaudePaths::new(&context.home, &context.config.paths.cache_root);
    let tmux = TmuxClient::new(
        context.processes.clone(),
        TmuxServer::named(context.config.tmux.socket.clone()).with_cwd(context.home.clone()),
    );
    let codex = CodexHarness::new(
        CodexPaths::new(&context.home, &context.config.paths.cache_root),
        context.processes.clone(),
        tmux.clone(),
    );
    let opencode = OpenCodeHarness::new(
        OpenCodePaths::new(&context.home, &context.config.paths.cache_root),
        context.processes.clone(),
        tmux,
    );
    let cancellation = scope.token();
    let scope_cancel = scope.token();
    let inventory = sessions.inventory.clone();
    let discoveries = sessions.discoveries.clone();
    let git = sessions.git.clone();
    let commands = sessions.commands.clone();
    scope.spawn(async move {
        let mut inventory_updates = inventory.subscribe();
        let mut discovery_updates = discoveries.subscribe();
        let mut git_updates = git.subscribe();
        let mut snapshot = SessionActivitySnapshot::default();
        let mut cursors = ActivityCursors {
            next_event_id: 1,
            ..Default::default()
        };
        let mut next_activity = tokio::time::interval(ACTIVITY_INTERVAL);
        next_activity.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut codex_watch = CodexOutputWatch::new();
        let mut next_usage = tokio::time::Instant::now();
        let mut last_published = snapshot.clone();
        let mut usage_error = None;
        let mut last_published_error = None;

        loop {
            let active = active_from_sources(
                &app,
                git_updates.borrow().clone(),
                inventory_updates.borrow().clone(),
                discovery_updates.borrow().clone(),
            );
            let wake = tokio::select! {
                biased;
                _ = scope_cancel.cancelled() => break,
                _ = cancellation.cancelled() => break,
                changed = inventory_updates.changed() => if changed.is_err() { break; } else { ActivityWake::Source },
                changed = discovery_updates.changed() => if changed.is_err() { break; } else { ActivityWake::Source },
                changed = git_updates.changed() => if changed.is_err() { break; } else { ActivityWake::Source },
                _ = next_activity.tick(), if !active.is_empty() => ActivityWake::Backstop,
                Some(()) = codex_watch.updates.recv(), if !active.is_empty() => ActivityWake::CodexOutput,
                _ = tokio::time::sleep_until(next_usage) => {
                    next_usage = tokio::time::Instant::now() + USAGE_INTERVAL;
                    let updated = fetch_usage(&paths, &codex, &opencode, &cancellation).await;
                    match updated {
                        Ok(mut usage) => {
                            // Freshness is not a visible change. Keep an idle
                            // board still when the measured values are equal.
                            if let Some(previous) = &snapshot.usage {
                                usage.cached_at_ms = previous.cached_at_ms;
                            }
                            snapshot.usage = Some(usage);
                            usage_error = None;
                        },
                        Err(error) => usage_error = Some(format!("session usage: {error}")),
                    }
                    ActivityWake::Source
                }
            };
            if scope_cancel.is_cancelled() || cancellation.is_cancelled() {
                break;
            }
            let active = active_from_sources(
                &app,
                git_updates.borrow().clone(),
                inventory_updates.borrow().clone(),
                discovery_updates.borrow().clone(),
            );
            let (_changed, discovery_changed, poll_errors) = if wake == ActivityWake::CodexOutput {
                let (changed, errors) = poll_codex_output(
                    &codex,
                    &mut cursors,
                    &mut snapshot,
                    &active,
                    &cancellation,
                )
                .await;
                (changed, false, errors)
            } else {
                let (mut changed, discovery_changed, mut errors) = poll_active(
                    &paths,
                    &codex,
                    &opencode,
                    &mut cursors,
                    &mut snapshot,
                    &active,
                    &cancellation,
                )
                .await;
                let (output_changed, output_errors) = poll_codex_output(
                    &codex,
                    &mut cursors,
                    &mut snapshot,
                    &active,
                    &cancellation,
                )
                .await;
                changed |= output_changed;
                errors.extend(output_errors);
                (changed, discovery_changed, errors)
            };
            codex_watch.sync(cursors.codex_output.watched_paths().map(Path::to_path_buf));
            if discovery_changed {
                for entry in &active {
                    commands.discover(entry.key.slug.clone(), entry.key.harness);
                }
            }
            let errors = usage_error
                .iter()
                .cloned()
                .chain(poll_errors)
                .collect::<Vec<_>>();
            let last_error = (!errors.is_empty()).then(|| errors.join("; "));
            if scope_cancel.is_cancelled() || cancellation.is_cancelled() {
                continue;
            }
            if snapshot != last_published || last_error != last_published_error {
                last_published_error = last_error.clone();
                last_published = snapshot.clone();
                publisher.publish(SourceSnapshot {
                    data: Some(Arc::new(snapshot.clone())),
                    state: last_error.as_ref().map_or(SourceState::Ready, |error| {
                        SourceState::Failed(error.clone().into())
                    }),
                    updated_at: Some(tokio::time::Instant::now()),
                    revision: 0,
                });
            }
        }
    });
    SessionActivitySources { activity }
}

#[derive(Clone)]
struct ActiveSession {
    key: SessionKey,
    target: AgentTarget,
}

fn current_active(
    app: &AppHarness,
    worktrees: Option<&Vec<wt_vcs::WorktreeSnapshot>>,
    inventory: Option<&SessionInventory>,
    discoveries: Option<&SessionDiscoveries>,
) -> Vec<ActiveSession> {
    let (Some(inventory), Some(discoveries), Some(worktrees)) = (inventory, discoveries, worktrees)
    else {
        return Vec::new();
    };
    let targets = app.targets(worktrees);
    let by_slug = targets
        .into_iter()
        .map(|target| (target.slug.clone(), target))
        .collect::<BTreeMap<_, _>>();
    discoveries
        .iter()
        .filter(|entry| {
            entry.session.is_live
                && inventory.all.contains_key(&entry.session.tmux_session_name)
                && inventory
                    .harness_session_ids
                    .get(&entry.session.tmux_session_name)
                    .is_none_or(|id| id == &entry.session.session_id)
        })
        .filter_map(|entry| {
            by_slug
                .get(&entry.key.slug)
                .filter(|target| !target.remote)
                .cloned()
                .map(|target| ActiveSession {
                    key: entry.key.clone(),
                    target,
                })
        })
        .collect()
}

fn active_from_sources(
    app: &AppHarness,
    git: SourceSnapshot<Vec<wt_vcs::WorktreeSnapshot>>,
    inventory: SourceSnapshot<SessionInventory>,
    discoveries: SourceSnapshot<SessionDiscoveries>,
) -> Vec<ActiveSession> {
    if matches!(git.state, SourceState::Failed(_))
        || matches!(inventory.state, SourceState::Failed(_))
        || matches!(discoveries.state, SourceState::Failed(_))
    {
        return Vec::new();
    }
    current_active(
        app,
        git.data.as_deref(),
        inventory.data.as_deref(),
        discoveries.data.as_deref(),
    )
}

async fn poll_active(
    paths: &ClaudePaths,
    codex: &CodexHarness,
    opencode: &OpenCodeHarness,
    cursors: &mut ActivityCursors,
    snapshot: &mut SessionActivitySnapshot,
    active: &[ActiveSession],
    cancel: &CancellationToken,
) -> (bool, bool, Vec<String>) {
    let ActivityCursors {
        claude: claude_cursors,
        codex: codex_tracker,
        opencode: opencode_tracker,
        opencode_output: opencode_output_tracker,
        next_event_id,
        ..
    } = cursors;
    let mut changed = false;
    let mut discovery_changed = false;
    let mut errors = Vec::new();
    let codex_active = active
        .iter()
        .filter(|entry| entry.key.harness == HarnessId::Codex)
        .map(|entry| {
            (
                entry.key.slug.clone(),
                entry.target.cwd.clone(),
                Some(entry.key.session_id.clone()),
            )
        })
        .collect::<Vec<_>>();
    if !codex_active.is_empty() {
        match codex
            .poll_activity_async(codex_tracker.clone(), codex_active, cancel)
            .await
        {
            Ok((tracker, batch)) => {
                *codex_tracker = tracker;
                for event in batch.events {
                    let slug = event_slug_for_text(active, HarnessId::Codex, &event.text);
                    changed |= push_event(
                        snapshot,
                        next_event_id,
                        slug,
                        HarnessId::Codex,
                        codex_level(event.level),
                        event.text,
                    );
                }
                let activity_changed = !batch.changed_slugs.is_empty();
                changed |= activity_changed;
                discovery_changed |= activity_changed;
            }
            Err(error) => errors.push(format!("Codex activity: {error}")),
        }
    }
    let opencode_active = active
        .iter()
        .filter(|entry| entry.key.harness == HarnessId::Opencode)
        .map(|entry| (entry.key.slug.clone(), entry.target.cwd.clone()))
        .collect::<Vec<_>>();
    if !opencode_active.is_empty() {
        match opencode
            .poll_activity_async(opencode_tracker.clone(), opencode_active, cancel)
            .await
        {
            Ok((tracker, batch)) => {
                *opencode_tracker = tracker;
                for event in batch.events {
                    changed |= push_event(
                        snapshot,
                        next_event_id,
                        event.slug,
                        HarnessId::Opencode,
                        opencode_level(event.level),
                        event.text,
                    );
                }
                changed |= batch.changed;
                discovery_changed |= batch.changed;
            }
            Err(error) => errors.push(format!("OpenCode activity: {error}")),
        }
    }
    let opencode_output_targets = output_targets(active, HarnessId::Opencode);
    let (tracker, result) = opencode
        .poll_output_async(
            opencode_output_tracker.clone(),
            opencode_output_targets,
            cancel,
        )
        .await;
    *opencode_output_tracker = tracker;
    match result {
        Ok(updates) => {
            for update in updates {
                let output_changed = apply_output_update(snapshot, HarnessId::Opencode, update);
                changed |= output_changed;
                discovery_changed |= output_changed;
            }
        }
        Err(error) => errors.push(format!("OpenCode output: {error}")),
    }
    let claude_active = active
        .iter()
        .filter(|entry| entry.key.harness == HarnessId::Claude)
        .map(|entry| {
            (
                entry.key.clone(),
                session_jsonl_path(&paths.home, &entry.target.cwd, &entry.key.session_id),
            )
        })
        .collect::<Vec<_>>();
    let mut cursors = claude_cursors.clone();
    let mut tails = snapshot.tails.clone();
    let fallback_cursors = cursors.clone();
    let fallback_tails = tails.clone();
    let claude_cancel = cancel.clone();
    let read = tokio::task::spawn_blocking(move || {
        let (changed, errors) =
            poll_claude(&claude_active, &mut cursors, &mut tails, &claude_cancel);
        (cursors, tails, changed, errors)
    })
    .await;
    match read {
        Ok((cursors, tails, updated, read_errors)) => {
            *claude_cursors = cursors;
            snapshot.tails = tails;
            changed |= updated;
            discovery_changed |= updated;
            errors.extend(read_errors);
        }
        Err(error) => {
            *claude_cursors = fallback_cursors;
            snapshot.tails = fallback_tails;
            errors.push(format!("Claude output worker: {error}"));
        }
    }
    let active_keys = active
        .iter()
        .map(|entry| entry.key.clone())
        .collect::<std::collections::BTreeSet<_>>();
    claude_cursors.retain(|key, _| active_keys.contains(key));
    // Retire closed sessions so a controller held open for weeks cannot retain
    // every transcript it has ever displayed.
    snapshot
        .tails
        .retain(|tail| active_keys.contains(&tail.key));
    (changed, discovery_changed, errors)
}

async fn poll_codex_output(
    codex: &CodexHarness,
    cursors: &mut ActivityCursors,
    snapshot: &mut SessionActivitySnapshot,
    active: &[ActiveSession],
    cancel: &CancellationToken,
) -> (bool, Vec<String>) {
    let (tracker, result) = codex
        .poll_output_async(
            cursors.codex_output.clone(),
            output_targets(active, HarnessId::Codex),
            cancel,
        )
        .await;
    cursors.codex_output = tracker;
    match result {
        Ok(updates) => {
            let mut changed = false;
            for update in updates {
                changed |= apply_output_update(snapshot, HarnessId::Codex, update);
            }
            (changed, Vec::new())
        }
        Err(error) => (false, vec![format!("Codex output: {error}")]),
    }
}

fn output_targets(active: &[ActiveSession], harness: HarnessId) -> Vec<HarnessOutputTarget> {
    active
        .iter()
        .filter(|entry| entry.key.harness == harness)
        .map(|entry| HarnessOutputTarget {
            slug: entry.key.slug.clone(),
            cwd: entry.target.cwd.clone(),
            session_id: entry.key.session_id.clone(),
        })
        .collect()
}

fn apply_output_update(
    snapshot: &mut SessionActivitySnapshot,
    harness: HarnessId,
    update: wt_harness::HarnessOutputUpdate,
) -> bool {
    let key = SessionKey {
        slug: update.slug,
        harness,
        session_id: update.session_id,
    };
    let index = snapshot.tails.iter().position(|tail| tail.key == key);
    if update.reset {
        if let Some(index) = index {
            snapshot.tails[index].lines.clear();
        } else {
            snapshot.tails.push(SessionTailSnapshot {
                key: key.clone(),
                lines: Vec::new(),
            });
            snapshot
                .tails
                .sort_by(|left, right| left.key.cmp(&right.key));
        }
    }
    if update.append.is_empty() && !update.reset {
        return false;
    }
    let index = snapshot.tails.iter().position(|tail| tail.key == key);
    let Some(index) = index else {
        return false;
    };
    snapshot.tails[index]
        .lines
        .extend(update.append.into_iter().map(|line| ActivityLineDto {
            id: line.id,
            timestamp_ms: line.timestamp_ms,
            kind: match line.kind {
                HarnessOutputKind::Info => ActivityKindDto::Info,
                HarnessOutputKind::User => ActivityKindDto::User,
                HarnessOutputKind::Assistant => ActivityKindDto::Assistant,
                HarnessOutputKind::Thinking => ActivityKindDto::Thinking,
                HarnessOutputKind::Tool => ActivityKindDto::Tool,
                HarnessOutputKind::ToolOk => ActivityKindDto::ToolOk,
                HarnessOutputKind::ToolError => ActivityKindDto::ToolError,
            },
            text: line.text,
        }));
    let lines = &mut snapshot.tails[index].lines;
    if lines.len() > MAX_LINES_PER_SESSION {
        lines.drain(..lines.len() - MAX_LINES_PER_SESSION);
    }
    true
}

fn poll_claude(
    active: &[(SessionKey, PathBuf)],
    claude_cursors: &mut BTreeMap<SessionKey, ClaudeTailCursor>,
    tails: &mut Vec<SessionTailSnapshot>,
    cancel: &CancellationToken,
) -> (bool, Vec<String>) {
    let mut changed = false;
    let mut errors = Vec::new();
    for (key, path) in active {
        if cancel.is_cancelled() {
            break;
        }
        let cursor = claude_cursors
            .entry(key.clone())
            .or_insert_with(|| ClaudeTailCursor::new(path.clone()));
        match cursor.read_delta(path) {
            Ok(read) => {
                if read.reset {
                    if let Some(tail) = tails.iter_mut().find(|tail| tail.key == *key) {
                        tail.lines.clear();
                    } else {
                        tails.push(SessionTailSnapshot {
                            key: key.clone(),
                            lines: Vec::new(),
                        });
                        tails.sort_by(|left, right| left.key.cmp(&right.key));
                    }
                    changed = true;
                }
                let delta = read.delta;
                if !delta.append.is_empty() || !delta.patch.is_empty() {
                    let mut lines = tails
                        .iter()
                        .find(|tail| tail.key == *key)
                        .map(|tail| tail.lines.clone())
                        .unwrap_or_default();
                    for line in delta.append.into_iter().map(activity_line) {
                        lines.push(line);
                    }
                    for patch in delta.patch {
                        let line = activity_line(patch.line);
                        if let Some(target) =
                            lines.iter_mut().find(|candidate| candidate.id == patch.id)
                        {
                            *target = line;
                        }
                    }
                    if lines.len() > MAX_LINES_PER_SESSION {
                        lines.drain(..lines.len() - MAX_LINES_PER_SESSION);
                    }
                    if let Some(tail) = tails.iter_mut().find(|tail| tail.key == *key) {
                        tail.lines = lines;
                    } else {
                        tails.push(SessionTailSnapshot {
                            key: key.clone(),
                            lines,
                        });
                        tails.sort_by(|a, b| a.key.cmp(&b.key));
                    }
                    changed = true;
                }
            }
            Err(error) => {
                errors.push(format!("Claude output {}: {error}", path.display()));
            }
        }
    }
    (changed, errors)
}

fn event_slug_for_text(active: &[ActiveSession], harness: HarnessId, _text: &str) -> String {
    let Some(slug) = _text.rsplit_once("· ").map(|(_, slug)| slug.trim()) else {
        return String::new();
    };
    active
        .iter()
        .find(|entry| entry.key.harness == harness && entry.key.slug == slug)
        .map(|entry| entry.key.slug.clone())
        .unwrap_or_default()
}

fn push_event(
    snapshot: &mut SessionActivitySnapshot,
    next: &mut u64,
    slug: String,
    harness: HarnessId,
    level: ActivityKindDto,
    text: String,
) -> bool {
    if text.is_empty() {
        return false;
    }
    let id = *next;
    *next = next.saturating_add(1);
    snapshot.events.push_back(SessionEvent {
        id,
        timestamp_ms: epoch_ms(),
        slug,
        harness,
        level,
        text,
    });
    while snapshot.events.len() > MAX_EVENT_LINES {
        snapshot.events.pop_front();
    }
    true
}

fn activity_line(line: wt_harness::ActivityLine) -> ActivityLineDto {
    ActivityLineDto {
        id: line.id,
        timestamp_ms: line.timestamp_ms,
        kind: claude_kind(line.kind),
        text: line.text,
    }
}

fn claude_kind(kind: ActivityKind) -> ActivityKindDto {
    match kind {
        ActivityKind::Info => ActivityKindDto::Info,
        ActivityKind::User => ActivityKindDto::User,
        ActivityKind::Assistant => ActivityKindDto::Assistant,
        ActivityKind::Thinking => ActivityKindDto::Thinking,
        ActivityKind::Tool => ActivityKindDto::Tool,
        ActivityKind::ToolOk => ActivityKindDto::ToolOk,
        ActivityKind::ToolError => ActivityKindDto::ToolError,
    }
}

fn codex_level(level: CodexEventLevel) -> ActivityKindDto {
    match level {
        CodexEventLevel::Info => ActivityKindDto::Info,
        CodexEventLevel::Dim => ActivityKindDto::Dim,
        CodexEventLevel::Ok => ActivityKindDto::Ok,
        CodexEventLevel::Warn => ActivityKindDto::Warn,
    }
}

fn opencode_level(level: OpenCodeEventLevel) -> ActivityKindDto {
    match level {
        OpenCodeEventLevel::Info => ActivityKindDto::Info,
        OpenCodeEventLevel::Dim => ActivityKindDto::Dim,
        OpenCodeEventLevel::Ok => ActivityKindDto::Ok,
        OpenCodeEventLevel::Warn => ActivityKindDto::Warn,
    }
}

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

async fn fetch_usage(
    claude_paths: &ClaudePaths,
    codex: &CodexHarness,
    opencode: &OpenCodeHarness,
    cancel: &CancellationToken,
) -> Result<UsageSnapshot, String> {
    let claude = claude_paths.usage_file();
    let (claude, codex_usage, opencode_cost) = tokio::join!(
        tokio::task::spawn_blocking(move || wt_harness::read_claude_usage(&claude)),
        codex.usage_async(cancel),
        opencode.cost_async(epoch_ms(), cancel),
    );
    let claude = claude.map_err(|error| error.to_string())?;
    let codex_usage = codex_usage.map_err(|error| error.to_string())?;
    let opencode_cost = opencode_cost.map_err(|error| error.to_string())?;
    Ok(UsageSnapshot {
        claude_five_hour: claude
            .as_ref()
            .and_then(|usage| usage.five_hour.clone())
            .map(usage_period),
        claude_seven_day: claude
            .as_ref()
            .and_then(|usage| usage.seven_day.clone())
            .map(usage_period),
        claude_seven_day_scoped: claude
            .map(|usage| {
                usage
                    .seven_day_scoped
                    .into_iter()
                    .map(usage_period)
                    .collect()
            })
            .unwrap_or_default(),
        codex_five_hour: codex_usage
            .as_ref()
            .and_then(|usage| usage.five_hour.clone())
            .map(usage_period),
        codex_seven_day: codex_usage
            .as_ref()
            .and_then(|usage| usage.seven_day.clone())
            .map(usage_period),
        codex_plan_type: codex_usage.and_then(|usage| usage.plan_type),
        opencode_five_hour: opencode_cost.as_ref().map(|cost| cost.five_hour),
        opencode_seven_day: opencode_cost.map(|cost| cost.seven_day),
        cached_at_ms: epoch_ms(),
    })
}

fn usage_period(value: wt_harness::UsagePeriod) -> UsagePeriodDto {
    UsagePeriodDto {
        utilization: value.utilization,
        resets_at: value.resets_at,
        label: value.label,
    }
}

#[derive(Clone, Default)]
struct ClaudeTailCursor {
    path: PathBuf,
    offset: u64,
    pending: Vec<u8>,
    drop_first_fragment: bool,
    initialized: bool,
    parser: ClaudeEventParser,
}

impl ClaudeTailCursor {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            ..Self::default()
        }
    }

    fn read_delta(&mut self, path: &Path) -> std::io::Result<ClaudeTailRead> {
        let mut reset = false;
        if self.path != path {
            *self = Self::new(path.to_owned());
            reset = true;
        }
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ClaudeTailRead {
                    delta: Default::default(),
                    reset,
                });
            }
            Err(error) => return Err(error),
        };
        if metadata.len() < self.offset {
            *self = Self::new(path.to_owned());
            reset = true;
        }
        if !self.initialized {
            self.offset = metadata.len().saturating_sub(MAX_SEED_BYTES);
            self.drop_first_fragment = self.offset > 0;
            self.initialized = true;
        }
        if metadata.len().saturating_sub(self.offset) > MAX_DELTA_BYTES {
            self.offset = metadata.len().saturating_sub(MAX_SEED_BYTES);
            self.pending.clear();
            self.drop_first_fragment = self.offset > 0;
            self.parser = ClaudeEventParser::default();
            reset = true;
        }
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes =
            vec![0; (metadata.len().saturating_sub(self.offset) as usize).min(MAX_READ_PER_TICK)];
        let count = file.read(&mut bytes)?;
        bytes.truncate(count);
        self.offset = self.offset.saturating_add(count as u64);
        let mut delta = wt_harness::ActivityDelta::default();
        for byte in bytes {
            if byte == b'\n' {
                if !self.drop_first_fragment
                    && let Ok(line) = std::str::from_utf8(&self.pending)
                {
                    let parsed = self.parser.parse_line(line);
                    delta.append.extend(parsed.append);
                    delta.patch.extend(parsed.patch);
                }
                self.pending.clear();
                self.drop_first_fragment = false;
            } else if self.pending.len() < MAX_PENDING_LINE {
                self.pending.push(byte);
            } else {
                self.pending.clear();
                self.drop_first_fragment = true;
            }
        }
        Ok(ClaudeTailRead { delta, reset })
    }
}

struct ClaudeTailRead {
    delta: wt_harness::ActivityDelta,
    reset: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;
    use wt_platform::process::ProcessRunner;
    use wt_tmux::{TmuxClient, TmuxServer};

    #[tokio::test]
    async fn codex_output_watch_coalesces_exact_rollout_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        let watched = temp.path().join("rollout-session.jsonl");
        let neighbor = temp.path().join("rollout-other-session.jsonl");
        std::fs::write(&watched, b"seed\n").unwrap();
        std::fs::write(&neighbor, b"seed\n").unwrap();
        let mut watch = CodexOutputWatch::new();
        watch.sync([watched.clone()].into_iter());

        std::fs::write(&neighbor, b"unrelated\n").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(75), watch.updates.recv())
                .await
                .is_err()
        );

        std::fs::write(&watched, b"seed\nnew output\n").unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), watch.updates.recv())
                .await
                .unwrap(),
            Some(())
        );
    }

    #[tokio::test]
    async fn codex_rollout_append_wakes_tailer_and_updates_exact_session() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let home = temp.path().join("home");
        let path = home
            .join(".codex/sessions/2026/10/09")
            .join("rollout-2026-10-09T00-00-00-thread-uuid.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let id = "thread-uuid";
        let header = serde_json::json!({
            "type": "session_meta",
            "payload": { "id": id, "cwd": cwd, "originator": "codex-tui", "thread_source": "user" }
        });
        let seed = serde_json::json!({
            "type": "event_msg", "timestamp": "2026-10-09T00:00:01Z",
            "payload": { "type": "agent_message", "message": "seed_output_ready" }
        });
        std::fs::write(&path, format!("{header}\n{seed}\n")).unwrap();

        let runner = ProcessRunner::new(NonZeroUsize::new(1).unwrap());
        let codex = CodexHarness::new(
            CodexPaths::new(&home, temp.path().join("cache")),
            runner.clone(),
            TmuxClient::new(runner, TmuxServer::named("test-output-tail")),
        );
        let active = [ActiveSession {
            key: SessionKey {
                slug: "fixture".into(),
                harness: HarnessId::Codex,
                session_id: id.into(),
            },
            target: AgentTarget {
                slug: "fixture".into(),
                kind: crate::harness::AgentTargetKind::Worktree,
                branch: Some("feature/fixture".into()),
                cwd,
                managed_name: None,
                remote: false,
            },
        }];
        let mut cursors = ActivityCursors::default();
        let mut snapshot = SessionActivitySnapshot::default();
        let cancel = CancellationToken::new();

        let (changed, errors) =
            poll_codex_output(&codex, &mut cursors, &mut snapshot, &active, &cancel).await;
        assert!(changed);
        assert!(errors.is_empty());
        assert_eq!(snapshot.tails[0].key.session_id, id);
        assert!(
            snapshot.tails[0]
                .lines
                .iter()
                .any(|line| line.text == "seed_output_ready")
        );

        let mut watch = CodexOutputWatch::new();
        watch.sync(cursors.codex_output.watched_paths().map(Path::to_path_buf));
        let appended = serde_json::json!({
            "type": "event_msg", "timestamp": "2026-10-09T00:00:02Z",
            "payload": { "type": "agent_message", "message": "notify_append_visible" }
        });
        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            "{appended}"
        )
        .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), watch.updates.recv())
                .await
                .unwrap(),
            Some(())
        );

        let (changed, errors) =
            poll_codex_output(&codex, &mut cursors, &mut snapshot, &active, &cancel).await;
        assert!(changed);
        assert!(errors.is_empty());
        assert!(
            snapshot.tails[0]
                .lines
                .iter()
                .any(|line| line.text == "notify_append_visible")
        );
    }

    #[test]
    fn claude_tail_truncation_clears_stale_visible_lines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00Z","message":{"content":"old prompt"}}"#.to_owned() + "\n",
        )
        .unwrap();
        let key = SessionKey {
            slug: "fixture".into(),
            harness: HarnessId::Claude,
            session_id: "session-id".into(),
        };
        let mut cursors = BTreeMap::new();
        let mut tails = Vec::new();
        let cancel = CancellationToken::new();
        let (changed, errors) = poll_claude(
            &[(key.clone(), path.clone())],
            &mut cursors,
            &mut tails,
            &cancel,
        );
        assert!(changed);
        assert!(errors.is_empty());
        assert_eq!(tails[0].lines.len(), 1);
        assert!(tails[0].lines[0].text.contains("old prompt"));

        std::fs::write(&path, "").unwrap();
        let (changed, errors) = poll_claude(&[(key, path)], &mut cursors, &mut tails, &cancel);
        assert!(changed);
        assert!(errors.is_empty());
        assert!(tails[0].lines.is_empty());
    }
}
