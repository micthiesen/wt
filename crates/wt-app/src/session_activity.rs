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

use futures_util::{StreamExt, stream};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wt_core::Domain;
use wt_core::HarnessId;
use wt_harness::{
    ActivityKind, ClaudeEventParser, ClaudePaths, CodexActivityTracker, CodexEventLevel,
    CodexHarness, CodexOutputTracker, HarnessOutputKind, HarnessOutputTarget,
    OpenCodeActivityTracker, OpenCodeEventLevel, OpenCodeHarness, OpenCodeOutputTracker,
    OpenCodePaths, PiHarness, PiOutputTracker, PiStatus, session_jsonl_path,
};
use wt_runtime::{
    Persistence, SourceHandle, SourceSnapshot, SourceSpec, SourceState, Stamp, TaskScope,
    leaf_channel,
};
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

type RolloutFingerprint = Option<(u64, std::time::SystemTime)>;

/// Coalesced notifications for the exact active Codex rollout files. Watching
/// their parent directories avoids recursive session-tree watches; the event
/// handler filters back to retained UUID paths. A metadata-only 500ms backstop
/// catches dropped notifications without rereading unchanged transcripts.
struct CodexOutputWatch {
    watcher: Option<RecommendedWatcher>,
    watched_paths: Arc<RwLock<BTreeSet<PathBuf>>>,
    directories: BTreeSet<PathBuf>,
    updates: mpsc::Receiver<()>,
    fingerprints: Arc<RwLock<BTreeMap<PathBuf, RolloutFingerprint>>>,
    fallback: tokio::task::JoinHandle<()>,
}

impl CodexOutputWatch {
    fn new() -> Self {
        let (sender, updates) = mpsc::channel(1);
        let watched_paths = Arc::new(RwLock::new(BTreeSet::new()));
        let fingerprints = Arc::new(RwLock::new(BTreeMap::<PathBuf, RolloutFingerprint>::new()));
        let polled = fingerprints.clone();
        let fallback_sender = sender.clone();
        let fallback = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(Duration::from_millis(500));
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticks.tick().await;
                if polled.read().is_ok_and(|paths| paths.is_empty()) {
                    continue;
                }
                let polled = polled.clone();
                let changed = tokio::task::spawn_blocking(move || {
                    let Ok(mut paths) = polled.write() else {
                        return false;
                    };
                    let mut changed = false;
                    for (path, previous) in paths.iter_mut() {
                        let current = rollout_fingerprint(path);
                        if current != *previous {
                            *previous = current;
                            changed = true;
                        }
                    }
                    changed
                })
                .await
                .unwrap_or(false);
                if changed {
                    let _ = fallback_sender.try_send(());
                }
            }
        });
        let watcher =
            notify::recommended_watcher(Self::event_handler(Arc::clone(&watched_paths), sender));
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
            fingerprints,
            fallback,
        }
    }

    fn event_handler(
        callback_paths: Arc<RwLock<BTreeSet<PathBuf>>>,
        sender: mpsc::Sender<()>,
    ) -> impl FnMut(notify::Result<Event>) + Send + 'static {
        move |result| {
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
        }
    }

    async fn sync(&mut self, paths: impl Iterator<Item = PathBuf>) {
        let paths = paths.collect::<Vec<_>>();
        let seeded = tokio::task::spawn_blocking(move || {
            paths
                .into_iter()
                .map(|path| {
                    let path = std::fs::canonicalize(&path).unwrap_or(path);
                    let stamp = rollout_fingerprint(&path);
                    (path, stamp)
                })
                .collect::<BTreeMap<_, _>>()
        })
        .await
        .unwrap_or_default();
        let paths = seeded.keys().cloned().collect::<BTreeSet<_>>();
        // Seed before returning so an immediate append cannot become the first
        // polling baseline. Never read transcript contents for this backstop.
        if let Ok(mut fingerprints) = self.fingerprints.write() {
            fingerprints.retain(|path, _| paths.contains(path));
            for (path, stamp) in seeded {
                fingerprints.entry(path).or_insert(stamp);
            }
        }
        let next_directories = paths
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect::<BTreeSet<_>>();
        if self.directories != next_directories
            && let Some(mut watcher) = self.watcher.take()
        {
            let remove = self
                .directories
                .difference(&next_directories)
                .cloned()
                .collect::<Vec<_>>();
            let add = next_directories
                .difference(&self.directories)
                .cloned()
                .collect::<Vec<_>>();
            // FSEvents restarts its stream when the watch set changes. Batch
            // those edits once, off the executor, and keep ownership until done.
            self.watcher = match tokio::task::spawn_blocking(move || {
                let mut changes = watcher.paths_mut();
                for directory in remove {
                    let _ = changes.remove(&directory);
                }
                for directory in add {
                    if let Err(error) = changes.add(&directory, RecursiveMode::NonRecursive) {
                        tracing::warn!(path = %directory.display(), %error, "could not watch Codex rollout directory; polling fallback remains active");
                    }
                }
                if let Err(error) = changes.commit() {
                    tracing::warn!(%error, "could not update Codex rollout watches; polling fallback remains active");
                }
                watcher
            }).await {
                Ok(watcher) => Some(watcher),
                Err(error) => {
                    tracing::warn!(%error, "Codex rollout watcher stopped; polling fallback remains active");
                    None
                }
            };
        }
        self.directories = next_directories;
        if let Ok(mut watched) = self.watched_paths.write() {
            *watched = paths;
        }
    }
}

impl Drop for CodexOutputWatch {
    fn drop(&mut self) {
        self.fallback.abort();
    }
}

fn rollout_fingerprint(path: &Path) -> RolloutFingerprint {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
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
    /// The persisted primary harness name, read with each activity pass.
    #[serde(default)]
    pub primary: Option<String>,
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
    pi_output: PiOutputTracker,
    pi_status: BTreeMap<SessionKey, Option<PiStatus>>,
    next_event_id: u64,
}

pub fn start(
    scope: &TaskScope,
    context: &AppContext,
    sessions: &SessionSources,
) -> SessionActivitySources {
    let (activity, mut publisher) = leaf_channel(
        scope,
        SourceSpec {
            name: "session-activity",
            domain: Domain::Sessions,
            persistence: Persistence::Never("live harness transcripts"),
            policy: Default::default(),
        },
    );
    let app = AppHarness::new(context);
    let paths = ClaudePaths::new(&context.home, &context.config.paths.cache_root);
    let tmux = TmuxClient::new(
        context.processes.clone(),
        TmuxServer::named(context.config.tmux.socket.clone()).with_cwd(context.home.clone()),
    );
    let codex = CodexHarness::new(
        crate::harness::codex_paths(&context.home, &context.config.paths.cache_root),
        context.processes.clone(),
        tmux.clone(),
    );
    let opencode = OpenCodeHarness::new(
        OpenCodePaths::new(&context.home, &context.config.paths.cache_root),
        context.processes.clone(),
        tmux.clone(),
    );
    let pi = PiHarness::new(
        crate::harness::pi_paths(&context.home, &context.config.paths.cache_root),
        context.processes.clone(),
        tmux,
    );
    let primary_path = context.config.paths.cache_root.join("harness.json");
    let primary_fallback = context.config.harness.primary;
    let cancellation = scope.token();
    let scope_cancel = scope.token();
    let inventory = sessions.inventory.view();
    let discoveries = sessions.discoveries.view();
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
                // An explicit refresh (e.g. after the primary harness changed)
                // runs a pass even when no session input changed.
                request = publisher.requested() => if request.is_none() { break; } else { ActivityWake::Source },
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
            // The pass observes transcripts from here on.
            let observed = Stamp::now();
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
                (changed, std::collections::BTreeSet::new(), errors)
            } else {
                let (mut changed, mut discovery_changed, mut errors) = poll_active(
                    &paths,
                    &codex,
                    &opencode,
                    &mut cursors,
                    &mut snapshot,
                    &active,
                    &cancellation,
                )
                .await;
                let (pi_changed, pi_discovery, pi_errors) = poll_pi_activity(
                    &pi, &mut cursors, &mut snapshot, &active, &cancellation,
                ).await;
                changed |= pi_changed;
                discovery_changed.extend(pi_discovery);
                errors.extend(pi_errors);
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
            codex_watch.sync(cursors.codex_output.watched_paths().map(Path::to_path_buf)).await;
            for (slug, harness) in discovery_changed {
                commands.discover(slug, harness);
            }
            snapshot.primary =
                Some(crate::harness::read_primary(&primary_path, primary_fallback).await.as_str().to_owned());
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
                // Partial failures still carry the tails that did update.
                publisher.publish_live_with_state(
                    snapshot.clone(),
                    observed,
                    last_error.as_ref().map_or(SourceState::Ready, |error| {
                        SourceState::Failed(error.clone().into())
                    }),
                );
            }
        }
        crate::watcher_cleanup::drop_watcher(codex_watch).await;
    });
    SessionActivitySources {
        activity: activity.view(),
    }
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
                && crate::session_source::is_session_live(
                    inventory,
                    entry.key.harness,
                    &entry.session,
                )
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
    if matches!(git.state(), SourceState::Failed(_))
        || matches!(inventory.state(), SourceState::Failed(_))
        || matches!(discoveries.state(), SourceState::Failed(_))
    {
        return Vec::new();
    }
    current_active(
        app,
        git.data().map(Arc::as_ref),
        inventory.data().map(Arc::as_ref),
        discoveries.data().map(Arc::as_ref),
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
) -> (
    bool,
    std::collections::BTreeSet<(String, HarnessId)>,
    Vec<String>,
) {
    let ActivityCursors {
        claude: claude_cursors,
        codex: codex_tracker,
        opencode: opencode_tracker,
        opencode_output: opencode_output_tracker,
        next_event_id,
        ..
    } = cursors;
    let mut changed = false;
    let mut discovery_changed = std::collections::BTreeSet::new();
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
                discovery_changed.extend(
                    batch
                        .changed_slugs
                        .into_iter()
                        .map(|slug| (slug, HarnessId::Codex)),
                );
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
                if batch.changed {
                    discovery_changed.extend(
                        active
                            .iter()
                            .filter(|entry| entry.key.harness == HarnessId::Opencode)
                            .map(|entry| (entry.key.slug.clone(), HarnessId::Opencode)),
                    );
                }
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
                let slug = update.slug.clone();
                let output_changed = apply_output_update(snapshot, HarnessId::Opencode, update);
                changed |= output_changed;
                if output_changed {
                    discovery_changed.insert((slug, HarnessId::Opencode));
                }
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
            if updated {
                discovery_changed.extend(
                    active
                        .iter()
                        .filter(|entry| entry.key.harness == HarnessId::Claude)
                        .map(|entry| (entry.key.slug.clone(), HarnessId::Claude)),
                );
            }
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

async fn poll_pi_activity(
    pi: &PiHarness,
    cursors: &mut ActivityCursors,
    snapshot: &mut SessionActivitySnapshot,
    active: &[ActiveSession],
    cancel: &CancellationToken,
) -> (bool, BTreeSet<(String, HarnessId)>, Vec<String>) {
    let mut changed = false;
    let mut discoveries = BTreeSet::new();
    let mut errors = Vec::new();
    let targets = active
        .iter()
        .filter(|entry| entry.key.harness == HarnessId::Pi && !entry.key.session_id.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    let statuses = stream::iter(targets.into_iter().map(|entry| async move {
        (
            entry.key.clone(),
            pi.status_for(
                &entry.key.slug,
                &entry.target.cwd,
                &entry.key.session_id,
                cancel,
            )
            .await,
        )
    }))
    .buffer_unordered(4)
    .collect::<Vec<_>>()
    .await;
    let live_keys = statuses
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<BTreeSet<_>>();
    cursors.pi_status.retain(|key, _| live_keys.contains(key));
    if live_keys.is_empty() {
        cursors.pi_output = PiOutputTracker::default();
        return (false, discoveries, errors);
    }
    for (key, result) in statuses {
        let status = match result {
            Ok(status) if status.session_id == key.session_id => Some(status),
            Ok(_) => None,
            Err(error) => {
                errors.push(format!("Pi activity for {}: {error}", key.slug));
                None
            }
        };
        // A lost or changed identity invalidates the previous state as well.
        // Never infer idle from a historical transcript or a missing socket.
        if cursors.pi_status.get(&key) != Some(&status) {
            discoveries.insert((key.slug.clone(), HarnessId::Pi));
            cursors.pi_status.insert(key, status);
            changed = true;
        }
    }
    let (tracker, output) = pi
        .poll_output_async(
            cursors.pi_output.clone(),
            output_targets(active, HarnessId::Pi),
            cancel,
        )
        .await;
    cursors.pi_output = tracker;
    match output {
        Ok(updates) => {
            for update in updates {
                let slug = update.slug.clone();
                if apply_output_update(snapshot, HarnessId::Pi, update) {
                    changed = true;
                    discoveries.insert((slug, HarnessId::Pi));
                }
            }
        }
        Err(error) => errors.push(format!("Pi output: {error}")),
    }
    (changed, discoveries, errors)
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
    async fn active_polling_rechecks_exact_identity_even_if_discovery_claims_live() {
        use crate::commands::test_support::CommandFixture;
        use crate::session_source::{DiscoveredSession, SessionDescriptor};
        let fixture = CommandFixture::new().await.unwrap();
        let app = AppHarness::new(&fixture.ctx);
        let worktrees = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let mut inventory = SessionInventory::default();
        inventory.all.insert(
            "one-codex".into(),
            SessionDescriptor {
                name: "one-codex".into(),
                id: "$1".into(),
                created_at: 1,
                attached_clients: 0,
                window_count: 1,
                harness_session_id: None,
            },
        );
        let histories = ["old", "current"]
            .map(|id| DiscoveredSession {
                key: SessionKey {
                    slug: "one".into(),
                    harness: HarnessId::Codex,
                    session_id: id.into(),
                },
                session: wt_harness::HarnessSession {
                    display_name: id.into(),
                    session_id: id.into(),
                    tmux_session_name: "one-codex".into(),
                    last_active_ms: None,
                    is_live: true,
                    extras: Default::default(),
                },
            })
            .to_vec();
        assert!(
            current_active(&app, Some(&worktrees), Some(&inventory), Some(&histories)).is_empty()
        );
        inventory
            .harness_session_ids
            .insert("one-codex".into(), "current".into());
        let active = current_active(&app, Some(&worktrees), Some(&inventory), Some(&histories));
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].key.session_id, "current");
        fixture.close().await.unwrap();
    }

    #[test]
    fn codex_output_watch_coalesces_exact_rollout_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        let watched = temp.path().join("rollout-session.jsonl");
        let neighbor = temp.path().join("rollout-other-session.jsonl");
        std::fs::write(&watched, b"seed\n").unwrap();
        std::fs::write(&neighbor, b"seed\n").unwrap();
        let paths = Arc::new(RwLock::new(BTreeSet::from([std::fs::canonicalize(
            &watched,
        )
        .unwrap()])));
        let (sender, mut updates) = mpsc::channel(1);
        let mut handle = CodexOutputWatch::event_handler(paths, sender);

        // FSEvents may deliver setup writes after watcher registration. Feed
        // exact events here so a delayed seed event cannot masquerade as an
        // unrelated-path failure. The next test checks real OS notifications
        // and observation of appended content.
        handle(Ok(Event::new(notify::EventKind::Any).add_path(neighbor)));
        handle(Ok(Event::new(notify::EventKind::Access(
            notify::event::AccessKind::Read,
        ))
        .add_path(watched.clone())));
        assert!(updates.try_recv().is_err());

        for _ in 0..3 {
            handle(Ok(
                Event::new(notify::EventKind::Any).add_path(watched.clone())
            ));
        }
        assert_eq!(updates.try_recv(), Ok(()));
        assert!(updates.try_recv().is_err());

        handle(Err(notify::Error::generic("watch fixture error")));
        assert_eq!(updates.try_recv(), Ok(()));
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
            wt_harness::CodexPaths::new(&home, temp.path().join("cache")),
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
        // Prove the metadata backstop independently of OS event delivery.
        watch.watcher = None;
        watch
            .sync(cursors.codex_output.watched_paths().map(Path::to_path_buf))
            .await;
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
