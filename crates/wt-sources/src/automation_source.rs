//! Host-local automation engine adapter over the shared source snapshots.

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
};
use wt_automations::AutomationLedger;
use wt_runtime::{Confirmed, LeafHandle, SourceHandle, SourceState, TaskScope, derived_channel};
use wt_tui::Board;

use crate::{
    automation_facts::{self, AutomationInputs, PreparedAutomationFacts},
    local_source::Metadata,
    session_activity::SessionActivitySnapshot,
    session_source::{SessionDiscoveries, SessionInventory},
};
use wt_app_core::{
    automation_engine::{
        AutomationEngine, AutomationOutcome, AutomationPassInput, ClaimedFire, QueuedFire,
    },
    context::AppContext,
};

/// Facts that drive fires are read through the leaves' `confirmed()` views:
/// live data only, never restored cache data or optimistic projections. The
/// board is display data and only supplies presentation hints.
#[derive(Clone)]
pub struct AutomationSources {
    pub board: SourceHandle<Board>,
    pub git: LeafHandle<Vec<wt_vcs::WorktreeSnapshot>>,
    pub metadata: LeafHandle<Metadata>,
    /// Raw confirmed PR data, with no optimistic overlay.
    pub github: LeafHandle<wt_github::GithubData>,
    pub sessions: LeafHandle<SessionDiscoveries>,
    pub inventory: LeafHandle<SessionInventory>,
    pub activity: SourceHandle<SessionActivitySnapshot>,
    pub edits: SourceHandle<std::collections::BTreeMap<String, i64>>,
}

#[derive(Clone)]
pub struct AutomationCommands {
    cancel: mpsc::Sender<oneshot::Sender<Result<usize, String>>>,
}

impl AutomationCommands {
    /// Persist cancellation of queued fires and wait for the bounded result.
    pub async fn cancel_pending(&self) -> Result<usize> {
        let (reply, result) = oneshot::channel();
        self.cancel
            .try_send(reply)
            .context("automation command queue is full")?;
        result
            .await
            .context("automation source stopped")?
            .map_err(anyhow::Error::msg)
    }
}

pub struct AutomationHandle {
    pub board: SourceHandle<Board>,
    pub commands: AutomationCommands,
    pub drain: Arc<AutomationDrain>,
}

/// Evaluation stops with the view; already claimed mutations must still join
/// before the host cancels command ownership or closes its database.
pub struct AutomationDrain {
    cancel: tokio_util::sync::CancellationToken,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl AutomationDrain {
    pub fn new(
        cancel: tokio_util::sync::CancellationToken,
        task: tokio::task::JoinHandle<()>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cancel,
            task: tokio::sync::Mutex::new(Some(task)),
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.cancel.cancel();
        let mut task = self.task.lock().await;
        if let Some(handle) = task.as_mut() {
            let result = handle.await.context("automation owner exited");
            task.take();
            result?;
        }
        Ok(())
    }
}

pub fn start(scope: &TaskScope, ctx: &AppContext, sources: AutomationSources) -> AutomationHandle {
    if ctx.config.automations.is_empty() || std::env::var("WT_AUTOMATIONS").as_deref() == Ok("off")
    {
        return passthrough(scope, sources.board);
    }
    let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
    let commands = AutomationCommands { cancel: cancel_tx };
    #[expect(
        clippy::disallowed_methods,
        reason = "engine status signal folded into the derived automations board, not board data itself"
    )]
    let (status_tx, status_rx) = watch::channel(EngineStatus::default());
    let board = overlay(scope, sources.board.clone(), status_rx);
    let token = scope.token();
    let accepted_context = ctx.clone();
    let mut context = ctx.clone();
    // Evaluation is source work, even when our caller supplies the host's
    // longer-lived command context. Only claimed dispatches own that drain.
    context.cancellation = token.clone();
    let drain_token = token.clone();
    let task = scope.spawn(async move {
        let ledger = AutomationLedger::new(context.config.paths.cache_root.join("automations.json"));
        let mut engine = AutomationEngine::new(ledger, token.clone());
        let delivered = match wt_app_core::actions::service(&context) {
            Ok(service) => match service.list_runs(500).await {
                Ok(runs) => runs.into_iter().flat_map(|run| run.meta.auto_fire_keys).collect::<BTreeSet<_>>(),
                Err(error) => { tracing::warn!(%error, "cannot inspect prior action runs during automation startup"); BTreeSet::new() }
            },
            Err(error) => { tracing::warn!(%error, "cannot open action service during automation startup"); BTreeSet::new() }
        };
        if let Err(error) = engine.reconcile_boot(&delivered, now_ms()).await {
            tracing::warn!(%error, "cannot reconcile automation ledger at startup");
        }

        let mut git_rx = sources.git.subscribe();
        let mut metadata_rx = sources.metadata.subscribe();
        let mut github_rx = sources.github.subscribe();
        let mut sessions_rx = sources.sessions.subscribe();
        let mut inventory_rx = sources.inventory.subscribe();
        let mut activity_rx = sources.activity.subscribe();
        let mut edits_rx = sources.edits.subscribe();
        let mut board_rx = sources.board.subscribe();
        let mut last_edit = std::collections::BTreeMap::new();
        let mut previous_status = std::collections::BTreeMap::new();
        let mut previous_git_revision = None;
        let mut persisted_tips = std::collections::BTreeMap::<String, String>::new();
        let mut facts: Option<PreparedAutomationFacts> = None;
        let mut cache_key = None;
        let mut lifecycle_cache_key = None;
        let mut lifecycle_cache = std::collections::BTreeMap::new();
        let mut lifecycle_errors = std::collections::BTreeMap::new();
        let mut lifecycle_retry = LifecycleRetry::default();
        let mut requirements_board_revision = None;
        let mut latest_error: Option<String> = None;
        // Whether `facts` reflects the current inputs. A failed preparation
        // keeps the last facts for cancelling pending fires, never for ticks.
        let mut facts_current = false;
        let mut dispatches: JoinSet<(ClaimedFire, AutomationOutcome, Option<String>)> = JoinSet::new();
        let mut active_dispatches = HashMap::new();
        let mut reported = Vec::new();
        let mut queue_report = QueueReport::default();
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = token.cancelled() => break,
                command = cancel_rx.recv() => {
                    let Some(reply) = command else { break; };
                    let result = if let Some(facts) = facts.as_ref() {
                        let pass = pass_input(&context, facts, now_ms());
                        engine.cancel_pending(pass).await.map_err(|error| format!("{error:#}"))
                    } else { Ok(0) };
                    let _ = reply.send(result);
                    continue;
                }
                changed = git_rx.changed() => if changed.is_err() { break; },
                changed = metadata_rx.changed() => if changed.is_err() { break; },
                changed = github_rx.changed() => if changed.is_err() { break; },
                changed = sessions_rx.changed() => if changed.is_err() { break; },
                changed = inventory_rx.changed() => if changed.is_err() { break; },
                changed = activity_rx.changed() => if changed.is_err() { break; },
                changed = edits_rx.changed() => if changed.is_err() { break; },
                changed = board_rx.changed() => if changed.is_err() { break; },
                completed = dispatches.join_next_with_id(), if !dispatches.is_empty() => {
                    match completed {
                        Some(Ok((id, (claim, outcome, error))) ) => {
                            active_dispatches.remove(&id);
                            if let Some(error) = error { latest_error = Some(error); }
                            let advanced_tip = claim.fire().branch_range.as_ref().map(|range| (range.branch.clone(), range.to.clone()));
                            let started = !matches!(outcome, AutomationOutcome::NotStarted);
                            if let Err(error) = engine.finish_with_cancellation(
                                &claim, outcome, now_ms(), &accepted_context.cancellation,
                            ).await {
                                latest_error = Some(format!("automation ledger finish: {error:#}"));
                            }
                            if started && let Some((branch, tip)) = advanced_tip {
                                let update = [(branch.clone(), tip.clone())].into_iter().collect();
                                match persist_tips(&context, &update).await {
                                    Ok(()) => { persisted_tips.insert(branch, tip); }
                                    Err(error) => latest_error = Some(format!("cannot persist branch tip after dispatch: {error:#}")),
                                }
                            }
                            let _ = status_tx.send(EngineStatus::new(engine.queued().len(), latest_error.clone()));
                        }
                        Some(Err(error)) => {
                            latest_error = Some(format!("automation dispatch task failed: {error}"));
                            // The task may have delivered before it failed;
                            // record that and free its engine slot.
                            if let Some((claim, _)) = active_dispatches.remove(&error.id())
                                && let Err(finish) = engine.finish_with_cancellation(
                                    &claim, failed_dispatch(&error), now_ms(), &accepted_context.cancellation,
                                ).await
                            {
                                latest_error = Some(format!("automation ledger finish: {finish:#}"));
                            }
                            let _ = status_tx.send(EngineStatus::new(engine.queued().len(), latest_error.clone()));
                        }
                        None => {}
                    }
                    continue;
                }
                _ = ticker.tick() => {},
            }
            let git = git_rx.borrow_and_update().clone();
            let metadata = metadata_rx.borrow_and_update().clone();
            let github = github_rx.borrow_and_update().clone();
            let sessions = sessions_rx.borrow_and_update().clone();
            let inventory = inventory_rx.borrow_and_update().clone();
            let activity = activity_rx.borrow_and_update().clone();
            let edits = edits_rx.borrow_and_update().clone();
            let base_board = board_rx.borrow_and_update().clone();
            let confirmed_git = sources.git.confirm(&git);
            let confirmed_metadata = sources.metadata.confirm(&metadata);
            let confirmed_sessions = sources.sessions.confirm(&sessions);
            let confirmed_inventory = sources.inventory.confirm(&inventory);
            let fresh_github = automation_facts::fresh_github(sources.github.confirm(&github));
            let lanes_ready = *git.state() == SourceState::Ready
                && *metadata.state() == SourceState::Ready;
            let metadata_key = metadata_facts_key(confirmed_metadata.as_deref());
            let key = (
                confirmed_git.as_deref().cloned(),
                lanes_ready,
                metadata_key,
                fresh_github.as_deref().cloned(),
            );
            let retry_lifecycle = lifecycle_retry.due();
            if cache_key.as_ref() != Some(&key) || retry_lifecycle {
                if previous_git_revision != Some(git.revision()) {
                    if let Some(snapshots) = confirmed_git.as_deref() {
                        let now = now_ms();
                        for snapshot in snapshots {
                            let slug = snapshot.worktree.target.slug().to_owned();
                            let status = snapshot.status.clone();
                            if previous_status.get(&slug) != Some(&status) {
                                last_edit.insert(slug.clone(), now);
                                previous_status.insert(slug, status);
                            }
                        }
                    }
                    previous_git_revision = Some(git.revision());
                }
                let pass_edits = merged_edits(edits.data().map(|edits| &**edits), &last_edit);
                let lifecycle_key = (
                    confirmed_git.as_deref().cloned(),
                    lifecycle_metadata_key(confirmed_metadata.as_deref()),
                    fresh_github.as_deref().cloned(),
                );
                if lifecycle_cache_key.as_ref() != Some(&lifecycle_key) || retry_lifecycle {
                    match automation_facts::lifecycle_facts(
                        &context,
                        confirmed_git.as_ref(),
                        confirmed_metadata.as_ref(),
                        fresh_github.as_ref(),
                        &context.config.automations,
                    ).await {
                        Ok(next) => {
                            lifecycle_retry.completed(!next.errors.is_empty() || next.deferred);
                            for (slug, error) in &next.errors {
                                if lifecycle_errors.get(slug) != Some(error) {
                                    tracing::warn!(
                                        target: "wt_attention",
                                        event_channel = "attention",
                                        event_source = %slug,
                                        event_text = %format!("Automation cleanup/landing evidence unavailable for {slug}: {error}"),
                                        "automation local evidence unavailable"
                                    );
                                }
                            }
                            lifecycle_errors = next.errors;
                            lifecycle_cache = next.rows;
                            lifecycle_cache_key = Some(lifecycle_key);
                        }
                        Err(error) => {
                            // Back off until the retry or an input change;
                            // the lifecycle rows rerun on any change.
                            lifecycle_retry.completed(true);
                            lifecycle_cache_key = None;
                            cache_key = Some(key);
                            facts_current = false;
                            latest_error = Some(format!("automation facts unavailable: lifecycle evidence: {error:#}"));
                            let _ = status_tx.send(EngineStatus::new(engine.queued().len(), latest_error.clone()));
                            continue;
                        }
                    }
                }
                let prepared = automation_facts::prepare(&context, AutomationInputs {
                    git: confirmed_git.as_ref(),
                    metadata: confirmed_metadata.as_ref(),
                    lanes_ready,
                    github: fresh_github.as_ref(),
                    discoveries: confirmed_sessions.as_ref(),
                    inventory: confirmed_inventory.as_ref(),
                    activity: activity.data().map(|activity| &**activity),
                    board: base_board.data().map(|board| &**board),
                    last_edit_at_ms: &pass_edits,
                    rules: &context.config.automations,
                    lifecycle: &lifecycle_cache,
                }).await;
                match prepared {
                    Ok(mut next) => {
                        if !next.initial_branch_tips.is_empty() {
                            if let Err(error) = persist_tips(&context, &next.initial_branch_tips).await {
                                // Back off like any other facts failure
                                // rather than retrying on every tick.
                                lifecycle_retry.completed(true);
                                cache_key = Some(key);
                                facts_current = false;
                                latest_error = Some(format!("automation facts unavailable: cannot persist branch tip baseline: {error:#}"));
                                let _ = status_tx.send(EngineStatus::new(engine.queued().len(), latest_error.clone()));
                                continue;
                            }
                            persisted_tips.extend(next.initial_branch_tips.clone());
                        }
                        for (branch, tip) in &mut next.context.branch_tips {
                            if let Some(seen) = persisted_tips.get(branch) { tip.seen = Some(seen.clone()); }
                        }
                        facts = Some(next); cache_key = Some(key); latest_error = None;
                        facts_current = true;
                    }
                    Err(error) => {
                        // Retry on the lifecycle backoff even if no input changes.
                        lifecycle_retry.completed(true);
                        cache_key = Some(key);
                        facts_current = false;
                        latest_error = Some(format!("automation facts unavailable: {error:#}"));
                    }
                }
            }
            // A dispatch error may since have replaced the message; the
            // flag, not the text, keeps stale facts from ticking.
            if !facts_current {
                let _ = status_tx.send(EngineStatus::new(engine.queued().len(), latest_error.clone()));
                queue_report.write(&context, &engine.queued(), paused(&facts), latest_error.as_deref());
                continue;
            }
            let Some(facts) = facts.as_mut() else {
                queue_report.write(&context, &engine.queued(), queue_report.paused(), latest_error.as_deref());
                continue;
            };
            let current_edits = merged_edits(edits.data().map(|edits| &**edits), &last_edit);
            automation_facts::refresh_quiescence_edits(
                facts,
                &current_edits,
                activity.data().map(|activity| &**activity),
            );
            for (branch, tip) in &mut facts.context.branch_tips {
                if let Some(seen) = persisted_tips.get(branch) { tip.seen = Some(seen.clone()); }
            }
            automation_facts::refresh_sessions(facts, confirmed_sessions.as_ref(), confirmed_inventory.as_ref());
            if requirements_board_revision != Some(base_board.revision()) {
                automation_facts::refresh_requirements(&context, facts, base_board.data().map(|board| &**board));
                requirements_board_revision = Some(base_board.revision());
            }
            facts.context.now_ms = now_ms();
            facts.context.github_fresh = fresh_github.is_some();
            let pass = pass_input(&context, facts, facts.context.now_ms);
            let tick = match engine.tick(pass).await {
                Ok(tick) => tick,
                Err(error) => {
                    latest_error = Some(format!("automation engine: {error:#}"));
                    queue_report.write(&context, &engine.queued(), facts.globally_paused, latest_error.as_deref());
                    continue;
                }
            };
            for claim in tick.claims {
                let context = accepted_context.clone();
                let github = fresh_github.clone();
                let board = base_board.data().map(|board| (**board).clone());
                let tracked = claim.clone();
                let task = dispatches.spawn(async move { dispatch_claim(context, claim, github, board).await });
                active_dispatches.insert(task.id(), (tracked, Instant::now()));
            }
            let queued = engine.queued();
            let _ = status_tx.send(EngineStatus::new(queued.len(), latest_error.clone()));
            report_queue_changes(&reported, &queued);
            reported = queued;
            queue_report.write(&context, &reported, facts.globally_paused, latest_error.as_deref());
        }
        let mut progress = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(3),
            Duration::from_secs(10),
        );
        progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while !dispatches.is_empty() {
            tokio::select! {
                completed = dispatches.join_next_with_id() => match completed {
                    Some(Ok((id, (claim, outcome, _)))) => {
                        active_dispatches.remove(&id);
                        if let Err(error) = engine.finish_with_cancellation(
                            &claim, outcome, now_ms(), &accepted_context.cancellation,
                        ).await {
                            tracing::warn!(%error, "cannot finish automation dispatch during shutdown");
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "automation dispatch task failed during shutdown");
                        if let Some((claim, _)) = active_dispatches.remove(&error.id())
                            && let Err(error) = engine.finish_with_cancellation(
                                &claim, failed_dispatch(&error), now_ms(), &accepted_context.cancellation,
                            ).await
                        {
                            tracing::warn!(%error, "cannot finish automation dispatch during shutdown");
                        }
                    }
                    None => break,
                },
                _ = progress.tick() => {
                    for (claim, started) in active_dispatches.values() {
                        eprintln!(
                            "wt: shutdown waiting: automation for {} on this machine is running ({} elapsed)",
                            claim.fire().slug,
                            shutdown_elapsed(started.elapsed()),
                        );
                    }
                }
            }
        }
    });
    AutomationHandle {
        board,
        commands,
        drain: AutomationDrain::new(drain_token, task),
    }
}

/// A dispatch task that panicked or was aborted may have delivered first.
fn failed_dispatch(error: &tokio::task::JoinError) -> AutomationOutcome {
    AutomationOutcome::Ambiguous {
        reason: format!("dispatch task failed: {error}"),
        slot: wt_app_core::automation_engine::DeliverySlot::None,
    }
}

fn shutdown_elapsed(elapsed: Duration) -> String {
    if elapsed.as_secs() >= 60 {
        format!("{}m {}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else {
        format!("{}s", elapsed.as_secs())
    }
}

fn passthrough(scope: &TaskScope, board: SourceHandle<Board>) -> AutomationHandle {
    let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
    let commands = AutomationCommands { cancel: cancel_tx };
    let token = scope.token();
    let drain_token = token.clone();
    let task = scope.spawn(async move {
        loop {
            tokio::select! {
                _ = token.cancelled() => break,
                reply = cancel_rx.recv() => {
                    let Some(reply) = reply else { break; };
                    let _ = reply.send(Ok(0));
                }
            }
        }
    });
    AutomationHandle {
        board,
        commands,
        drain: AutomationDrain::new(drain_token, task),
    }
}

async fn dispatch_claim(
    context: AppContext,
    claim: ClaimedFire,
    github: Option<Confirmed<wt_github::GithubData>>,
    board: Option<Board>,
) -> (ClaimedFire, AutomationOutcome, Option<String>) {
    let fire = claim.fire().clone();
    let (outcome, error) = if fire.rule.run.starts_with("builtin:") {
        match wt_ops::automation_builtins::execute(&context, &fire).await {
            wt_ops::automation_builtins::BuiltinExecution::Retry { reason } => {
                tracing::info!(rule = %fire.rule.id, %reason, "automation will retry");
                (AutomationOutcome::NotStarted, None)
            }
            wt_ops::automation_builtins::BuiltinExecution::Completed { message } => {
                tracing::info!(rule = %fire.rule.id, %message, "automation completed");
                (
                    AutomationOutcome::Delivered {
                        slot: wt_app_core::automation_engine::DeliverySlot::None,
                    },
                    None,
                )
            }
            wt_ops::automation_builtins::BuiltinExecution::Failed { message } => {
                tracing::warn!(rule = %fire.rule.id, %message, "automation failed");
                (
                    AutomationOutcome::Delivered {
                        slot: wt_app_core::automation_engine::DeliverySlot::None,
                    },
                    Some(format!("{}: {message}", fire.rule.id)),
                )
            }
        }
    } else {
        (
            wt_ops::action_dispatch::dispatch_automation(
                &context,
                &fire,
                github.as_ref(),
                board.as_ref(),
            )
            .await,
            None,
        )
    };
    (claim, outcome, error)
}

fn overlay(
    scope: &TaskScope,
    input: SourceHandle<Board>,
    mut status_rx: watch::Receiver<EngineStatus>,
) -> SourceHandle<Board> {
    let (source, mut publisher) = derived_channel(scope, "automations");
    let cancel = scope.token();
    let mut updates = input.subscribe();
    scope.spawn(async move {
        updates.mark_changed();
        let mut failures = wt_app_core::feeds::FailureClock::default();
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    input.refresh();
                    continue;
                }
                changed = updates.changed() => if changed.is_err() { break; },
                changed = status_rx.changed() => if changed.is_err() { break; },
            }
            let snapshot = updates.borrow_and_update().clone();
            let EngineStatus { pending, error } = status_rx.borrow_and_update().clone();
            let Some(mut board) = snapshot.data().map(|board| (**board).clone()) else {
                continue;
            };
            // The paused chip stays the inventory's: it reads durable state
            // and is the witness the pause toggle's Pending settles against.
            board.automations_pending = pending;
            board.activity.retain(|line| {
                !line.text.starts_with("Automations:")
                    && !line.text.starts_with("Automation error:")
            });
            board.attention.retain(|line| {
                !line.text.starts_with("Automations:")
                    && !line.text.starts_with("Automation error:")
            });
            if pending > 0 {
                failures.activity(
                    &mut board,
                    "INFO",
                    "Automations",
                    &format!("Automations: {pending} pending"),
                );
            }
            if let Some(error) = error {
                failures.attention(
                    &mut board,
                    "Automations",
                    &format!("Automation error: {error}"),
                );
            }
            failures.sweep();
            // The status lines are derived from the board's own inputs, so
            // the board keeps its provenance.
            publisher.publish_if_changed(snapshot.derive(Some(Arc::new(board))));
        }
    });
    source
}

/// What the engine reports to the board's title bar and activity pane.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct EngineStatus {
    pending: usize,
    error: Option<String>,
}

impl EngineStatus {
    fn new(pending: usize, error: Option<String>) -> Self {
        Self { pending, error }
    }
}

fn paused(facts: &Option<PreparedAutomationFacts>) -> bool {
    facts.as_ref().is_some_and(|facts| facts.globally_paused)
}

/// Log a fire when it first queues and when it leaves the queue without
/// being claimed, so a fire that waits forever leaves a trace.
fn report_queue_changes(before: &[QueuedFire], after: &[QueuedFire]) {
    let key = |fire: &QueuedFire| (fire.rule.clone(), fire.target.clone());
    let previous = before
        .iter()
        .map(key)
        .collect::<std::collections::BTreeSet<_>>();
    let current = after
        .iter()
        .map(key)
        .collect::<std::collections::BTreeSet<_>>();
    for fire in after.iter().filter(|fire| !previous.contains(&key(fire))) {
        tracing::info!(
            rule = %fire.rule,
            target = %fire.target,
            detail = %fire.detail,
            waiting_for = %fire.waiting_for,
            "automation queued"
        );
    }
    for fire in before.iter().filter(|fire| !current.contains(&key(fire))) {
        tracing::info!(rule = %fire.rule, target = %fire.target, "automation left the queue");
    }
}

/// The queue snapshot `wt automations` prints. Rewritten when it changes and
/// at least once a minute, so a reader can tell a live engine from a dead one.
#[derive(Default)]
struct QueueReport {
    last: Option<(String, Instant)>,
    paused: bool,
}

/// Per repository: a cache root can be shared by repositories identified by
/// their main clone.
pub fn queue_report_path(config: &wt_config::Config) -> std::path::PathBuf {
    let id = config
        .repo_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    config
        .paths
        .cache_root
        .join(format!("automations-queue.{id}.json"))
}
const QUEUE_REPORT_HEARTBEAT: Duration = Duration::from_secs(60);

impl QueueReport {
    fn write(
        &mut self,
        ctx: &AppContext,
        queued: &[QueuedFire],
        paused: bool,
        error: Option<&str>,
    ) {
        let body = serde_json::json!({
            "paused": paused,
            "error": error,
            "queued": queued,
        })
        .to_string();
        if self
            .last
            .as_ref()
            .is_some_and(|(last, at)| *last == body && at.elapsed() < QUEUE_REPORT_HEARTBEAT)
        {
            return;
        }
        let report = serde_json::json!({
            "pid": std::process::id(),
            "updated_at_ms": now_ms(),
            "paused": paused,
            "error": error,
            "queued": queued,
        });
        let path = queue_report_path(&ctx.config);
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let written =
            std::fs::write(&temp, report.to_string()).and_then(|()| std::fs::rename(&temp, &path));
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temp);
            tracing::warn!(%error, path = %path.display(), "cannot write automation queue report");
        }
        self.last = Some((body, Instant::now()));
        self.paused = paused;
    }

    /// The last reported pause state, for passes that have no facts yet.
    fn paused(&self) -> bool {
        self.paused
    }
}

fn pass_input<'a>(
    ctx: &'a AppContext,
    facts: &'a PreparedAutomationFacts,
    now_ms: i64,
) -> AutomationPassInput<'a> {
    AutomationPassInput {
        rules: &ctx.config.automations,
        rows: &facts.rows,
        context: &facts.context,
        globally_paused: facts.globally_paused,
        state_ready: facts.state_ready,
        enabled: std::env::var("WT_AUTOMATIONS").as_deref() != Ok("off"),
        now_ms,
        requirements: &facts.requirements,
        quiescence: &facts.quiescence,
        activity: &facts.activity,
    }
}

fn now_ms() -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .unwrap_or(i64::MAX)
}

/// A partial local failure cannot become a permanent cached negative. Keep
/// healthy evidence usable while a bounded backstop retries the same inputs.
#[derive(Default)]
struct LifecycleRetry {
    at: Option<tokio::time::Instant>,
}

impl LifecycleRetry {
    const DELAY: Duration = Duration::from_secs(30);

    fn due(&self) -> bool {
        self.at.is_some_and(|at| tokio::time::Instant::now() >= at)
    }

    fn completed(&mut self, partial_failure: bool) {
        self.at = partial_failure.then(|| tokio::time::Instant::now() + Self::DELAY);
    }
}

async fn persist_tips(
    ctx: &AppContext,
    tips: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let tips = tips.clone();
    ctx.database
        .call(move |store| {
            for (branch, sha) in tips {
                store.set_branch_tip(&branch, &sha)?;
            }
            Ok(())
        })
        .await
        .context("write automation branch tips")
}

fn merged_edits(
    observed: Option<&std::collections::BTreeMap<String, i64>>,
    fallback: &std::collections::BTreeMap<String, i64>,
) -> std::collections::BTreeMap<String, i64> {
    let mut merged = fallback.clone();
    if let Some(observed) = observed {
        for (slug, timestamp) in observed {
            merged
                .entry(slug.clone())
                .and_modify(|current| *current = (*current).max(*timestamp))
                .or_insert(*timestamp);
        }
    }
    merged
}

fn metadata_facts_key(metadata: Option<&crate::local_source::Metadata>) -> serde_json::Value {
    let Some((state, archived)) = metadata else {
        return serde_json::Value::Null;
    };
    let mut slugs = serde_json::Map::new();
    if let Some(entries) = state["slugs"].as_object() {
        for (slug, record) in entries {
            let mut relevant = serde_json::Map::new();
            for field in [
                "baseBranch",
                "baseSha",
                "createdAt",
                "work",
                "issueId",
                "githubIssue",
                "automationsPaused",
            ] {
                if let Some(value) = record.get(field) {
                    relevant.insert(field.into(), value.clone());
                }
            }
            slugs.insert(slug.clone(), serde_json::Value::Object(relevant));
        }
    }
    let removed_pauses = state["removed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|record| record["automationsPaused"] == true)
        .cloned()
        .collect::<Vec<_>>();
    serde_json::json!({
        "slugs": slugs,
        "archived": archived,
        "pausedStacks": state["pausedStacks"],
        "automationsPaused": state["automationsPaused"],
        "branchTips": state["branchTips"],
        "removedPauses": removed_pauses,
    })
}

fn lifecycle_metadata_key(metadata: Option<&crate::local_source::Metadata>) -> serde_json::Value {
    let Some((state, _)) = metadata else {
        return serde_json::Value::Null;
    };
    let mut slugs = serde_json::Map::new();
    if let Some(entries) = state["slugs"].as_object() {
        for (slug, record) in entries {
            if let Some(base_sha) = record.get("baseSha") {
                slugs.insert(slug.clone(), base_sha.clone());
            }
        }
    }
    // An accepted removal retires its checkout from lifecycle evidence.
    let removals = state["removals"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, removal)| (key.clone(), removal["path"].clone()))
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({"slugs": slugs, "removals": removals})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wt_runtime::SourceSnapshot;

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_cancels_busy_evidence_without_cancelling_accepted_commands() {
        use std::os::unix::fs::PermissionsExt;
        use wt_app_core::test_support::CommandFixture;
        use wt_core::Domain;
        use wt_runtime::{Persistence, RefreshPolicy, SourceSpec, Stamp, leaf_channel};

        let mut fixture = CommandFixture::new().await.unwrap();
        Arc::make_mut(&mut fixture.ctx.config).automations = vec![wt_config::AutomationDef {
            id: "shutdown-evidence".into(),
            on: wt_config::AutomationTrigger::WtMerged,
            run: "builtin:delete-branch".into(),
            ..Default::default()
        }];
        // Only a landed row captures a removal revision (`git status`, which
        // runs the fsmonitor hook below), so land `one` on a local origin.
        let git_in = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(&fixture.ctx.cwd)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        let origin = fixture._root.path().join("origin.git");
        git_in(&["init", "--bare", origin.to_str().unwrap()]);
        git_in(&["remote", "add", "origin", origin.to_str().unwrap()]);
        let base = git_in(&["rev-parse", "HEAD"]);
        fixture
            .ctx
            .database
            .call(move |store| {
                store.set_slug_base("one", Some(("main", Some(&base))))?;
                Ok(())
            })
            .await
            .unwrap();
        std::fs::write(fixture.ctx.cwd.join("tracked.txt"), "landed work\n").unwrap();
        git_in(&["commit", "-am", "landed"]);
        git_in(&["push", "origin", "HEAD:refs/heads/main"]);
        let snapshots = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        // Hold a real Git descendant, not a synthetic never-ready future.
        // The runner must cancel and reap it when evidence loses its scope.
        let marker = fixture._root.path().join("fsmonitor-started");
        let hook = fixture._root.path().join("fsmonitor");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\nexec sleep 30\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let status = std::process::Command::new("git")
            .current_dir(&fixture.ctx.cwd)
            .args(["config", "core.fsmonitor", hook.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success());
        let scope = TaskScope::new();
        fn leaf<T: Send + Sync + 'static>(
            scope: &TaskScope,
            domain: Domain,
            data: T,
        ) -> (LeafHandle<T>, wt_runtime::LeafPublisher<T>) {
            let (leaf, publisher) = leaf_channel(
                scope,
                SourceSpec {
                    name: "shutdown-fixture",
                    domain,
                    persistence: Persistence::Never("isolated test"),
                    policy: RefreshPolicy::default(),
                },
            );
            publisher.publish_live(data, Stamp::now());
            (leaf, publisher)
        }
        let (git, _git) = leaf(&scope, Domain::Git, snapshots);
        let (metadata, _metadata) = leaf(&scope, Domain::Metadata, (state, BTreeSet::new()));
        let (github, _github) = leaf(&scope, Domain::Github, wt_github::GithubData::default());
        let (sessions, _sessions) = leaf(&scope, Domain::Sessions, SessionDiscoveries::default());
        let (inventory, _inventory) = leaf(&scope, Domain::Sessions, SessionInventory::default());
        let (board, _board) = derived_channel(&scope, "board");
        let (activity, _activity) = derived_channel(&scope, "activity");
        let (edits, _edits) = derived_channel(&scope, "edits");
        let _handle = start(
            &scope,
            &fixture.ctx,
            AutomationSources {
                board,
                git,
                metadata,
                github,
                sessions,
                inventory,
                activity,
                edits,
            },
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("automation evidence reached the held Git subprocess");
        let result = scope.shutdown(Duration::from_secs(2)).await;
        assert!(
            !fixture.ctx.cancellation.is_cancelled(),
            "read shutdown cancelled admitted command ownership"
        );
        // Always release the pre-fix runaway if this regression fails.
        fixture.ctx.cancellation.cancel();
        result.expect("read-only evidence must stop and reap its process on quit");
        fixture.close().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn unchanged_lifecycle_inputs_retry_partial_failure_then_stop_after_recovery() {
        let mut retry = LifecycleRetry::default();
        assert!(!retry.due());
        retry.completed(true);
        // Neither source data nor its key changed: the periodic tick must
        // eventually bypass both cache guards, but not on every second.
        for _ in 0..29 {
            tokio::time::advance(Duration::from_secs(1)).await;
            assert!(!retry.due());
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(retry.due());
        retry.completed(true);
        assert!(!retry.due());
        tokio::time::advance(LifecycleRetry::DELAY).await;
        assert!(retry.due());
        retry.completed(false);
        tokio::time::advance(LifecycleRetry::DELAY * 2).await;
        assert!(
            !retry.due(),
            "recovered evidence remains cached for unchanged inputs"
        );
    }

    #[tokio::test]
    async fn passthrough_keeps_board_refresh_and_stops_command_responder() {
        let scope = TaskScope::new();
        let (board, publisher) = derived_channel(&scope, "board");
        publisher.publish(SourceSnapshot::test_live(
            Some(Board::default()),
            SourceState::Ready,
            wt_core::Domain::Git,
        ));
        let handle = passthrough(&scope, board.clone());
        let mut updates = handle.board.subscribe();
        assert_eq!(updates.borrow().revision(), 1);
        assert_eq!(handle.commands.cancel_pending().await.unwrap(), 0);
        publisher.publish(SourceSnapshot::test_live(
            Some(Board::default()),
            SourceState::Ready,
            wt_core::Domain::Git,
        ));
        tokio::time::timeout(
            Duration::from_secs(1),
            updates.wait_for(|snapshot| snapshot.revision() == 2),
        )
        .await
        .unwrap()
        .unwrap();

        scope.cancel();
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
        assert!(handle.commands.cancel_pending().await.is_err());
    }
}
