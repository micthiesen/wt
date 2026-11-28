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
use wt_automations::{AutomationLedger, fire_identity};
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
    /// What each probed stage is running. The proof is read from Git; this
    /// tells the engine when it moved.
    pub deploys: SourceHandle<crate::deploy_source::DeploySnapshot>,
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
    // Watches run under an implicit rule, so the engine runs with no
    // `[[automations]]` at all.
    if std::env::var("WT_AUTOMATIONS").as_deref() == Ok("off") {
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
    crate::mcp_events::start(scope, &context, token.clone());
    let task = scope.spawn(async move {
        let ledger = AutomationLedger::new(context.config.paths.cache_root.join("automations.json"));
        let mut engine = AutomationEngine::new(ledger, token.clone());
        // The queue lives in this process alone: whatever an earlier engine
        // logged as queued and never as left ended when this one started.
        let engine_id = format!("{}-{}", now_ms(), std::process::id());
        tracing::info!(engine = %engine_id, "automation engine started");
        let restored_runs = match wt_app_core::actions::service(&context) {
            Ok(service) => restore_runs(&mut engine, now_ms(), |since_ms| async move {
                Ok(service.automation_runs_since(since_ms).await?)
            })
            .await
            .unwrap_or_else(|error| {
                // Without the records nothing is known, so nothing is ended.
                tracing::warn!(%error, "cannot read back automation runs launched before this start");
                None
            }),
            Err(_) => None,
        };
        let prior_runs = match wt_app_core::actions::service(&context) {
            Ok(service) => match service.list_runs_bounded(500).await {
                Ok(runs) => Some(runs),
                Err(error) => { tracing::warn!(%error, "cannot inspect prior action runs during automation startup"); None }
            },
            Err(error) => { tracing::warn!(%error, "cannot open action service during automation startup"); None }
        };
        let delivered = prior_runs.iter().flatten().flat_map(|run| run.meta.auto_fire_keys.iter().cloned()).collect::<BTreeSet<_>>();
        if let Err(error) = engine.reconcile_boot(&delivered, now_ms()).await {
            tracing::warn!(%error, "cannot reconcile automation ledger at startup");
        }
        match engine.restore_deferred(now_ms()).await {
            Ok(0) => {}
            Ok(restored) => tracing::info!(restored, "requeued deferred automation fires"),
            Err(error) => tracing::warn!(%error, "cannot restore deferred automation fires"),
        }
        if let Some(runs) = restored_runs {
            tracing::info!(runs = runs.len(), "awaiting automation runs launched before this start");
            if let Err(error) = settle(&context, &accepted_context, &mut engine, runs, true).await {
                tracing::warn!(%error, "cannot settle automation runs launched before this start");
            }
        }

        let mut git_rx = sources.git.subscribe();
        let mut metadata_rx = sources.metadata.subscribe();
        let mut github_rx = sources.github.subscribe();
        let mut sessions_rx = sources.sessions.subscribe();
        let mut inventory_rx = sources.inventory.subscribe();
        let mut activity_rx = sources.activity.subscribe();
        let mut edits_rx = sources.edits.subscribe();
        let mut board_rx = sources.board.subscribe();
        let mut deploys_rx = sources.deploys.subscribe();
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
        let mut production_tip = ProductionTip::default();
        let mut requirements_board_revision = None;
        let mut latest_error: Option<String> = None;
        // Whether `facts` reflects the current inputs. A failed preparation
        // keeps the last facts for cancelling pending fires, never for ticks.
        let mut facts_current = false;
        let mut dispatches: JoinSet<(ClaimedFire, AutomationOutcome, Option<String>)> = JoinSet::new();
        let mut active_dispatches = HashMap::new();
        let mut reported = Vec::new();
        let mut queue_report = QueueReport::default();
        let mut externals = crate::watch_facts::Externals::default();
        // What was last written for which metadata revision: the snapshot
        // lags its own writes, and the same finding is not written twice.
        let mut applied_stamps: Option<(u64, Vec<wt_core::watch::WatchStamps>)> = None;
        let mut orphans_settled_at: Option<u64> = None;
        let mut settled_watches = BTreeSet::new();
        // Landing commits outlive the rows they were read from: a watch
        // follows its commit once its worktree is removed.
        let mut known_landings = std::collections::BTreeMap::<String, String>::new();
        // The landing commits of the rows as last read: what a watch about
        // its own landing gets stamped with, so a later process has it too.
        let mut row_landings = std::collections::BTreeMap::<String, String>::new();
        // Armed merges already asked to be forgotten; the snapshot lags.
        let mut ended_arms = BTreeSet::new();
        // Due checks already handed to the store to date; the snapshot lags.
        let mut dated_checks = Vec::new();
        // When removed rows were last looked over for something due, and
        // the metadata revision they were read at.
        let mut removed_due = RemovedDueLooks::default();
        let mut removed_due_told = BTreeSet::new();
        // Records removed before a production landing was kept on them
        // are completed from history, once, before anything reads them.
        if let Err(error) = context
            .database
            .call(|store| Ok(store.heal_removed_landings()?))
            .await
        {
            tracing::warn!(error = %format!("{error:#}"), "cannot complete removed worktrees' landings");
        }
        let mut last_run_check = None;
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
                changed = deploys_rx.changed() => if changed.is_err() { break; },
                completed = dispatches.join_next_with_id(), if !dispatches.is_empty() => {
                    match completed {
                        Some(Ok((id, (claim, outcome, error))) ) => {
                            active_dispatches.remove(&id);
                            if let Some(error) = &error { latest_error = Some(error.clone()); }
                            let advanced_tip = claim.fire().branch_range.as_ref().map(|range| (range.branch.clone(), range.to.clone()));
                            let delivered_event = claim.fire().mcp_event.as_ref().map(|event| (event.rule.clone(), event.event_id.clone()));
                            let started = !matches!(
                                outcome,
                                AutomationOutcome::NotStarted | AutomationOutcome::Deferred { .. }
                            );
                            let finished = fire_event(claim.fire(), &outcome, error.as_deref());
                            match engine.finish_with_cancellation(
                                &claim, outcome, now_ms(), &accepted_context.cancellation,
                            ).await {
                                Ok(Some(notice)) => tracing::warn!(
                                    target: "wt_attention",
                                    event_channel = "attention",
                                    event_source = %claim.fire().slug,
                                    event_text = %notice,
                                    "automation still deferred"
                                ),
                                Ok(None) => {}
                                Err(error) => {
                                    latest_error = Some(format!("automation ledger finish: {error:#}"));
                                }
                            }
                            if let Some(event) = finished {
                                wt_app_core::worktree_history::record(&context, &claim.fire().slug, event).await;
                            }
                            // The ledger already holds the fire's key, so a
                            // failed drop only leaves an event that cannot
                            // fire twice.
                            if started && let Some((rule, event_id)) = delivered_event {
                                let dropped = context.database.call(move |store| Ok(store.drop_mcp_event(&rule, &event_id)?)).await;
                                if let Err(error) = dropped {
                                    latest_error = Some(format!("cannot drop a delivered MCP event: {error:#}"));
                                }
                            }
                            if started && let Some((branch, tip)) = advanced_tip {
                                let update = [(branch.clone(), tip.clone())].into_iter().collect();
                                match persist_tips(&context, &update).await {
                                    Ok(()) => { persisted_tips.insert(branch, tip); }
                                    Err(error) => latest_error = Some(format!("cannot persist branch tip after dispatch: {error:#}")),
                                }
                            }
                            let _ = status_tx.send(engine_status(&engine, latest_error.clone()));
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
                            let _ = status_tx.send(engine_status(&engine, latest_error.clone()));
                        }
                        None => {}
                    }
                    continue;
                }
                arrived = externals.arrived(), if externals.busy() => {
                    if let Some(arrived) = arrived { externals.absorb(arrived); }
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
            let (confirmed_sessions, confirmed_inventory) =
                confirm_session_inputs(&sources.sessions, &sources.inventory, &sessions, &inventory);
            let fresh_github = automation_facts::fresh_github(sources.github.confirm(&github));
            let lanes_ready = *git.state() == SourceState::Ready
                && *metadata.state() == SourceState::Ready;
            let metadata_key = metadata_facts_key(confirmed_metadata.as_deref());
            let watches_await_production = confirmed_metadata.as_deref().is_some_and(|(state, _)| {
                wt_ops::lifecycle_ops::watch_wants(state).values().any(|wants| wants.production)
                    || wt_ops::lifecycle_ops::check_wants(state).values().any(|wants| wants.production)
            });
            let production = production_tip.current(&context, watches_await_production).await;
            let deployed = deploys_rx.borrow_and_update().data().map(|deploys| {
                (deploys.staging.oid.clone(), deploys.production.oid.clone())
            });
            let key = (
                confirmed_git.as_deref().cloned(),
                lanes_ready,
                metadata_key,
                fresh_github.as_deref().cloned(),
                production.clone(),
                deployed.clone(),
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
                    production,
                    deployed,
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
                            row_landings.clone_from(&next.landing_commits);
                            known_landings.extend(next.landing_commits);
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
                            let _ = status_tx.send(engine_status(&engine, latest_error.clone()));
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
                                let _ = status_tx.send(engine_status(&engine, latest_error.clone()));
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
                let _ = status_tx.send(engine_status(&engine, latest_error.clone()));
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
            let queued = engine.retained_fires();
            let undecided = queued.iter().any(|fire| !facts.requirements.contains_key(&fire_identity(fire)));
            if undecided || requirements_board_revision != Some(base_board.revision()) {
                automation_facts::refresh_requirements(&context, facts, base_board.data().map(|board| &**board), &queued);
                requirements_board_revision = Some(base_board.revision());
            }
            facts.context.now_ms = now_ms();
            facts.context.github_fresh = fresh_github.is_some();
            facts.context.github_read_at_ms = fresh_github
                .as_ref()
                .and_then(|github| github.age())
                .map(|age| facts.context.now_ms.saturating_sub(i64::try_from(age.as_millis()).unwrap_or(i64::MAX)));
            facts.context.watches.clear();
            if let Some((state, _)) = confirmed_metadata.as_deref() {
                let watches = wt_store::stored_watches(state);
                let now = facts.context.now_ms;
                let revision = metadata.revision();
                // Reads start here and arrive through the select above;
                // this pass uses what has arrived so far.
                externals.request(&context, &wt_core::watch::needs(&watches), &facts.watch_prs, &facts.branches);
                let known = crate::watch_facts::facts(&watches, &facts.watch_rows, facts.state_ready, &facts.watch_prs, &externals);
                let stamps = crate::watch_facts::stamps(&watches, &known, &row_landings, now);
                if !stamps.is_empty() && applied_stamps.as_ref() != Some(&(revision, stamps.clone())) {
                    let write = stamps.clone();
                    match context.database.call(move |store| Ok(store.apply_evaluation(&write)?)).await {
                        Ok(_) => applied_stamps = Some((revision, stamps)),
                        Err(error) => latest_error = Some(format!("cannot record what a watch saw: {error:#}")),
                    }
                }
                // A build from before displaced checks closes such a check's
                // wake on delivery; the kept record brings it back as owed.
                if wt_store::kept_checks_lost(state) {
                    match context.database.call(move |store| Ok(store.restore_kept_checks(now)?)).await {
                        Ok(restored) => for step in restored {
                            tracing::info!(step = %step.id, slug = %step.slug, "restored a check whose wake an older wt closed");
                        },
                        Err(error) => latest_error = Some(format!("cannot restore a check an older wt closed: {error:#}")),
                    }
                }
                // Only an inventory this process read says a worktree is gone.
                let inventory_read_at = confirmed_git
                    .as_ref()
                    .filter(|_| facts.state_ready)
                    .and_then(|git| git.age())
                    .map(|age| now.saturating_sub(i64::try_from(age.as_millis()).unwrap_or(i64::MAX)));
                let live = crate::watch_facts::live_owners(&facts.watch_rows, &watches, state, inventory_read_at.unwrap_or(i64::MIN));
                if inventory_read_at.is_some()
                    && orphans_settled_at != Some(revision)
                    && crate::watch_facts::has_orphans(&watches, &wt_store::stored_owed(state), &live)
                {
                    orphans_settled_at = Some(revision);
                    let (owners, landings) = (live.clone(), known_landings.clone());
                    match context.database.call(move |store| Ok(store.drop_orphans(&owners, &landings, now)?)).await {
                        Ok(dropped) => for watch in dropped {
                            tracing::info!(watch = %watch.id, slug = %watch.slug, "dropped a watch whose worktree is gone");
                        },
                        Err(error) => latest_error = Some(format!("cannot settle watches of removed worktrees: {error:#}")),
                    }
                }
                known_landings.retain(|slug, _| watches.iter().any(|watch| watch.slug == *slug));
                facts.context.watches = crate::watch_facts::deliverable(&watches, &live, now);
            }
            // A merge that landed, or whose pull request closed, is no
            // longer armed; a later arming is a new record.
            if ended_arms.len() > 512 { ended_arms.clear(); }
            for (slug, armed_at) in wt_automations::finished_merge_arms(&facts.rows, &facts.context) {
                if !ended_arms.insert((slug.clone(), armed_at)) { continue; }
                let ended = slug.clone();
                if let Err(error) = context.database.call(move |store| Ok(store.clear_slug_merge_arm(&ended, Some(armed_at))?)).await {
                    ended_arms.remove(&(slug, armed_at));
                    latest_error = Some(format!("cannot forget an ended merge: {error:#}"));
                }
            }
            // The trigger needs GitHub to have held the merge once.
            for (slug, armed_at) in wt_automations::observed_merge_arms(&facts.rows, &facts.context) {
                let (seen, now) = (slug.clone(), facts.context.now_ms);
                match context.database.call(move |store| Ok(store.note_slug_merge_arm_seen(&seen, armed_at, now)?)).await {
                    Ok(_) => {
                        if let Some(arm) = facts.rows.iter_mut().filter(|row| row.slug == slug).find_map(|row| row.merge_arm.as_mut()).filter(|arm| arm.armed_at_ms == armed_at) {
                            arm.seen_armed_at_ms = Some(now);
                        }
                    }
                    Err(error) => latest_error = Some(format!("cannot record a merge seen armed: {error:#}")),
                }
            }
            if engine.awaits_runs()
                && last_run_check.is_none_or(|at: Instant| at.elapsed() >= RUN_CHECK_INTERVAL)
            {
                last_run_check = Some(Instant::now());
                if let Err(error) = settle_runs(&context, &accepted_context, &mut engine).await {
                    latest_error = Some(format!("automation run results: {error:#}"));
                }
            }
            // A check first seen due is dated: overdue counts from then,
            // whatever is asserted later, and history says when.
            let newly_due = if facts.state_ready { wt_automations::checks_newly_due(&facts.rows, &facts.context) } else { Vec::new() };
            let due_keys = newly_due.iter().map(|due| (due.slug.clone(), due.stage, due.steps.clone(), due.pr)).collect::<Vec<_>>();
            if !newly_due.is_empty() && due_keys != dated_checks {
                let now = facts.context.now_ms;
                match context.database.call(move |store| Ok(store.note_checks_due(&newly_due, now)?)).await {
                    Ok(_) => dated_checks = due_keys,
                    Err(error) => latest_error = Some(format!("cannot record when a check came due: {error:#}")),
                }
            }
            // A removed row has no session to prompt: what it owes and is
            // due goes to the manager, once. Not through the engine's queue,
            // which holds fires for rows that exist.
            let (now, revision) = (facts.context.now_ms, metadata.revision());
            if facts.state_ready
                && !facts.globally_paused
                && std::env::var("WT_AUTOMATIONS").as_deref() != Ok("off")
                && removed_due.due(revision, now)
            {
                let wake = removed_due.may_wake(now);
                let live = facts.rows.iter().map(|row| row.slug.clone()).collect::<BTreeSet<_>>();
                match tell_removed_due(&context, &live, now, &mut removed_due_told, wake).await {
                    Ok(deferred) => {
                        if let Some(reason) = &deferred {
                            tracing::info!(%reason, retry_in_seconds = REMOVED_DUE_RECHECK_MS / 1000, "what removed worktrees owe was not sent to the manager; trying again later");
                        }
                        removed_due.looked(revision, now, wake.then_some(deferred.is_some()));
                    }
                    Err(error) => {
                        removed_due.looked(revision, now, None);
                        latest_error = Some(format!("cannot tell the manager what a removed worktree owes: {error:#}"));
                    }
                }
            }
            let pass = pass_input(&context, facts, facts.context.now_ms);
            let tick = match engine.tick(pass).await {
                Ok(tick) => tick,
                Err(error) => {
                    latest_error = Some(format!("automation engine: {error:#}"));
                    queue_report.write(&context, &engine.queued(), facts.globally_paused, latest_error.as_deref());
                    continue;
                }
            };
            record_skipped(&context, engine.take_skipped()).await;
            if settled_watches.len() > 512 { settled_watches.clear(); }
            for settled in tick.settled_watches {
                let (id, action) = (settled.id.clone(), settled.action);
                if !settled_watches.insert((id.clone(), action)) { continue; }
                // The ledger says the fire ran, not that the gate was
                // written or the wake recorded: a crash between the two
                // leaves the action on its watch. A wake settles the way its
                // delivery would have, by the landing its fire was claimed
                // under.
                let at = facts.context.now_ms;
                let settle = move |store: &mut wt_store::Store| {
                    Ok(wt_ops::watch_actions::settle(
                        store, &settled.id, settled.action, settled.landing, settled.unconfirmed, at,
                    )?)
                };
                if let Err(error) = context.database.call(settle).await {
                    settled_watches.remove(&(id, action));
                    latest_error = Some(format!("cannot record a finished watch action: {error:#}"));
                }
            }
            let dispatched = tick.claims.iter().map(|claim| fire_identity(claim.fire())).collect::<BTreeSet<_>>();
            for claim in tick.claims {
                let context = accepted_context.clone();
                let github = fresh_github.clone();
                let board = base_board.data().map(|board| (**board).clone());
                let tracked = claim.clone();
                let task = dispatches.spawn(async move { dispatch_claim(context, claim, github, board).await });
                active_dispatches.insert(task.id(), (tracked, Instant::now()));
            }
            let queued = engine.queued();
            let _ = status_tx.send(engine_status(&engine, latest_error.clone()));
            report_queue_changes(&engine_id, &reported, &queued, now_ms(), |fire| {
                if dispatched.contains(&fire.id) {
                    "dispatched"
                } else if engine.retains(&fire.id) {
                    "held"
                } else {
                    "dropped"
                }
            });
            reported = queued;
            queue_report.write(&context, &reported, facts.globally_paused, latest_error.as_deref());
        }
        // The queue ends with the engine: say so, and leave no report of
        // fires that nothing will dispatch.
        report_queue_changes(&engine_id, &reported, &[], now_ms(), |_| "engine stopped");
        queue_report.write(&context, &[], queue_report.paused(), latest_error.as_deref());
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

/// How often run records are read while a launched shell run is awaited.
const RUN_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Ended runs read per check; a launched run ends within the newest few.
const RUN_CHECK_LIMIT: usize = 50;

/// Tell the engine how the shell runs it launched ended, and report the
/// failed ones: a retry in the log, a fire that ended failed in its
/// worktree's history. The attention feed gets one line per failed fire: the
/// action's own failure line, or this one for an action that retries, whose
/// own lines the action board keeps off the feed.
async fn settle_runs(
    context: &AppContext,
    accepted: &AppContext,
    engine: &mut AutomationEngine,
) -> Result<()> {
    let runs = wt_app_core::actions::service(context)?
        .list_runs_bounded(RUN_CHECK_LIMIT)
        .await?;
    let runs = runs.into_iter().map(|run| run.meta).collect();
    settle(context, accepted, engine, runs, false).await
}

/// Await again the runs a process that has quit launched, and look up
/// their records: `None` when there are none. `lookup` returns every
/// automation run started at or after the time it is given.
///
/// The ledger is read before the runs. A run's record is written before
/// its launch and its awaited entry after, so every entry read here has its
/// record in a lookup made later, unless the record is gone. Read the other
/// way round, a run another live process launched in between would have an
/// entry and no record, and be ended as unknown under its owner. The lookup
/// is by time, not a count of runs: an awaited run is at most a day old,
/// and maintenance removes only finished runs older than 30 days.
async fn restore_runs<F, Fut>(
    engine: &mut AutomationEngine,
    now_ms: i64,
    lookup: F,
) -> Result<Option<Vec<wt_actions::ActionMeta>>>
where
    F: FnOnce(u64) -> Fut,
    Fut: Future<Output = Result<Vec<wt_actions::ActionMeta>>>,
{
    engine.restore_awaited(now_ms).await?;
    let Some(since_ms) = engine.restored_since_ms() else {
        return Ok(None);
    };
    Ok(Some(lookup(u64::try_from(since_ms).unwrap_or(0)).await?))
}

/// [`settle_runs`] over `runs`. `restored` says they came from
/// [`restore_runs`], so they hold the record of every restored run that
/// still has one: a restored run with no record is then ended with an
/// unknown outcome instead of waited for.
async fn settle(
    context: &AppContext,
    accepted: &AppContext,
    engine: &mut AutomationEngine,
    runs: Vec<wt_actions::ActionMeta>,
    restored: bool,
) -> Result<()> {
    let mut running = Vec::new();
    for meta in &runs {
        if meta.auto_fire_keys.is_empty() {
            continue;
        }
        let started_at_ms = i64::try_from(meta.started_at).unwrap_or(i64::MAX);
        if matches!(
            meta.status,
            wt_actions::ActionRunStatus::Running | wt_actions::ActionRunStatus::Ambiguous
        ) {
            running.push((meta.auto_fire_keys.clone(), started_at_ms));
            continue;
        }
        let failed = engine
            .run_ended(
                &meta.auto_fire_keys,
                started_at_ms,
                run_end(meta),
                now_ms(),
                &accepted.cancellation,
            )
            .await?;
        let Some(failed) = failed else { continue };
        let Some(retry_in_ms) = failed.retry_in_ms else {
            let reason =
                wt_app_core::automation_engine::failed_reason(failed.exit_code, failed.runs);
            if failed.retried {
                tracing::warn!(
                    target: "wt_attention",
                    event_channel = "attention",
                    event_source = %failed.slug,
                    event_text = %format!("auto {}: failed, {reason}", failed.rule),
                    "automation failed"
                );
            } else {
                tracing::warn!(rule = %failed.rule, slug = %failed.slug, %reason, "automation failed");
            }
            if failed.slug != wt_automations::FLEET_SLUG {
                let event = wt_core::WorktreeEventKind::Automation {
                    rule: failed.rule,
                    outcome: wt_core::AutomationOutcomeKind::Failed,
                    reason: Some(wt_core::event_text(&reason)),
                };
                wt_app_core::worktree_history::record(context, &failed.slug, event).await;
            }
            continue;
        };
        tracing::warn!(
            rule = %failed.rule,
            slug = %failed.slug,
            exit_code = failed.exit_code,
            failed_runs = failed.runs,
            retry_in_seconds = retry_in_ms / 1000,
            "automation run failed; it will run again"
        );
    }
    if restored {
        engine
            .end_lost_runs(&running, now_ms(), &accepted.cancellation)
            .await?;
    }
    // Told once, here: the ledger records the outcome for one process only.
    for unknown in engine.take_unknown_runs() {
        let text = format!(
            "auto {}: outcome unknown, not run again: {}",
            unknown.rule, unknown.reason
        );
        tracing::warn!(
            target: "wt_attention",
            event_channel = "attention",
            event_source = %unknown.slug,
            event_text = %text,
            "automation outcome unknown"
        );
        if unknown.slug != wt_automations::FLEET_SLUG {
            let event = wt_core::WorktreeEventKind::Automation {
                rule: unknown.rule,
                outcome: wt_core::AutomationOutcomeKind::Unconfirmed,
                reason: Some(wt_core::event_text(&format!(
                    "outcome unknown: {}",
                    unknown.reason
                ))),
            };
            wt_app_core::worktree_history::record(context, &unknown.slug, event).await;
        }
    }
    Ok(())
}

/// Only a shell command that ran to its end and exited non-zero is a
/// definite failure. One whose worker died before recording an exit code
/// may have succeeded: its outcome is unknown. A killed run and an agent
/// run (which may have acted before it exited) are neither.
fn run_end(meta: &wt_actions::ActionMeta) -> wt_app_core::automation_engine::RunEnd {
    use wt_app_core::automation_engine::RunEnd;
    match (meta.kind, meta.status, meta.exit_code) {
        (wt_actions::ActionRunKind::Shell, wt_actions::ActionRunStatus::Failed, Some(code))
            if code != 0 =>
        {
            RunEnd::ShellFailed { exit_code: code }
        }
        (wt_actions::ActionRunKind::Shell, wt_actions::ActionRunStatus::Failed, None) => {
            RunEnd::ShellUnknown
        }
        _ => RunEnd::Other,
    }
}

/// Record the fires the engine ended without running. A fleet fire has no
/// worktree.
/// How often removed rows are looked over again without a state change: a
/// check put off comes due by the clock alone, and a manager that could not
/// be reached is tried again.
const REMOVED_DUE_RECHECK_MS: i64 = 5 * 60 * 1000;

/// When removed rows are looked over, and when the manager may be woken
/// for them. A look runs on every state change, but each wake is awaited
/// inside the automation loop and a manager waiting on a person holds it
/// for seconds before deferring: after a deferral no wake is tried until
/// the recheck interval has passed, however often the state changes.
#[derive(Default)]
struct RemovedDueLooks {
    /// The metadata revision last looked at, and when to look again anyway.
    seen: Option<(u64, i64)>,
    wake_held_until: Option<i64>,
}

impl RemovedDueLooks {
    fn due(&self, revision: u64, now: i64) -> bool {
        self.seen
            .is_none_or(|(seen, again_at)| seen != revision || now >= again_at)
    }

    fn may_wake(&self, now: i64) -> bool {
        self.wake_held_until.is_none_or(|until| now >= until)
    }

    /// `deferred` is whether a look that could wake the manager was
    /// deferred; `None` for a look that could not, or that failed.
    fn looked(&mut self, revision: u64, now: i64, deferred: Option<bool>) {
        let again_at = now.saturating_add(REMOVED_DUE_RECHECK_MS);
        self.seen = Some((revision, again_at));
        match deferred {
            Some(true) => self.wake_held_until = Some(again_at),
            Some(false) => self.wake_held_until = None,
            None => {}
        }
    }
}

/// Prompt the manager for each removed row with something due that it was
/// not told about, and record what it was told. `told` holds what this
/// process sent, as `(slug, removed_at, key)`: a record that could not be
/// written is written at a later look, and the manager is not asked twice.
/// With `wake` unset nothing is sent, only such records are written.
/// Returns why the manager was not told, when a wake sent nothing.
async fn tell_removed_due(
    context: &AppContext,
    live: &BTreeSet<String>,
    now: i64,
    told: &mut BTreeSet<(String, String, String)>,
    wake: bool,
) -> anyhow::Result<Option<String>> {
    let separate = context.config.branch.separate_production();
    let removed = context
        .database
        .call(|store| Ok(store.read_removed_worktrees()?))
        .await?;
    // Each wake is awaited here, inside the source's loop. Once the manager
    // has deferred one, the rest of this look would only wait on it again.
    let mut manager_reachable = wake;
    let mut deferred = None;
    for entry in removed {
        if live.contains(&entry.slug) || entry.automations_paused == Some(true) {
            continue;
        }
        let sent = |key: &str| {
            told.contains(&(entry.slug.clone(), entry.removed_at.clone(), key.to_owned()))
        };
        let (unrecorded, untold): (Vec<_>, Vec<_>) =
            wt_store::removed_due_untold(&entry, separate, now)
                .into_iter()
                .partition(|(key, _)| sent(key));
        let mut keys = unrecorded
            .into_iter()
            .map(|(key, _)| key)
            .collect::<Vec<_>>();
        let lines = untold
            .iter()
            .filter_map(|(_, item)| {
                let what = match item.stage.filter(|_| item.kind == wt_core::OwedKind::Check) {
                    Some(stage) => format!("check in {}:\n{}", stage.as_str(), item.what),
                    None => item.what.clone(),
                };
                Some((item.done_target()?, what))
            })
            .collect::<Vec<_>>();
        if !lines.is_empty() && manager_reachable {
            let prompt = wt_ops::watch_actions::removed_due_prompt(&entry.slug, &lines);
            match wt_ops::action_dispatch::wake_manager(context, &prompt).await {
                AutomationOutcome::Delivered { .. } | AutomationOutcome::Ambiguous { .. } => {
                    for (key, _) in untold {
                        told.insert((entry.slug.clone(), entry.removed_at.clone(), key.clone()));
                        keys.push(key);
                    }
                }
                // Nothing was sent: a later look tries again.
                AutomationOutcome::Deferred { reason, .. } => {
                    manager_reachable = false;
                    deferred = Some(reason);
                }
                _ => {
                    manager_reachable = false;
                    deferred = Some("nothing was sent to the manager".to_owned());
                }
            }
        }
        if keys.is_empty() {
            continue;
        }
        let (slug, removed_at) = (entry.slug.clone(), entry.removed_at.clone());
        if let Err(error) = context
            .database
            .call(move |store| Ok(store.mark_removed_due_told(&slug, &removed_at, &keys)?))
            .await
        {
            tracing::warn!(
                slug = %entry.slug,
                error = %format!("{error:#}"),
                "cannot record what the manager was told a removed worktree owes"
            );
        }
    }
    Ok(deferred)
}

async fn record_skipped(
    context: &AppContext,
    skipped: Vec<wt_app_core::automation_engine::SkippedFire>,
) {
    for skipped in skipped {
        if skipped.slug == wt_automations::FLEET_SLUG {
            continue;
        }
        let event = wt_core::WorktreeEventKind::Automation {
            rule: skipped.rule,
            outcome: wt_core::AutomationOutcomeKind::Skipped,
            reason: Some(wt_core::event_text(&skipped.reason)),
        };
        wt_app_core::worktree_history::record(context, &skipped.slug, event).await;
    }
}

/// How a finished fire reads in its worktree's history. A fire that will run
/// again (not started, deferred) is still pending and is not an event, and a
/// fleet fire has no worktree.
fn fire_event(
    fire: &wt_automations::AutomationFire,
    outcome: &AutomationOutcome,
    error: Option<&str>,
) -> Option<wt_core::WorktreeEventKind> {
    use wt_core::AutomationOutcomeKind;
    // A watch's history is written with the watch itself.
    if fire.slug == wt_automations::FLEET_SLUG || fire.watch.is_some() {
        return None;
    }
    let (outcome, reason) = match (outcome, error) {
        (AutomationOutcome::NotStarted | AutomationOutcome::Deferred { .. }, _) => return None,
        // A built-in that ran and reported failure is delivered to the
        // ledger, so it is not replayed; here it reads as what it was.
        (AutomationOutcome::Delivered { .. }, Some(error)) => {
            (AutomationOutcomeKind::Failed, Some(error))
        }
        (AutomationOutcome::Delivered { .. }, None) => (AutomationOutcomeKind::Delivered, None),
        (AutomationOutcome::Ambiguous { reason, .. }, _) => {
            (AutomationOutcomeKind::Unconfirmed, Some(reason.as_str()))
        }
    };
    Some(wt_core::WorktreeEventKind::Automation {
        rule: fire.rule.id.clone(),
        outcome,
        reason: reason
            .map(|reason| {
                // A built-in's failure text leads with its own rule id.
                let prefix = format!("{}: ", fire.rule.id);
                wt_core::event_text(reason.strip_prefix(prefix.as_str()).unwrap_or(reason))
            })
            .filter(|reason| !reason.is_empty()),
    })
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
    let (outcome, error) = if fire.watch.is_some() {
        wt_ops::watch_actions::dispatch(&context, &fire).await
    } else if wt_automations::is_check_wake(&fire) {
        wt_ops::watch_actions::dispatch_check(&context, &fire).await
    } else if fire.rule.run.starts_with("builtin:") {
        match wt_ops::automation_builtins::execute(&context, &fire).await {
            wt_ops::automation_builtins::BuiltinExecution::Retry { reason } => {
                tracing::info!(rule = %fire.rule.id, %reason, "automation will retry");
                (AutomationOutcome::NotStarted, None)
            }
            wt_ops::automation_builtins::BuiltinExecution::Defer { reason, frozen } => {
                tracing::info!(rule = %fire.rule.id, %reason, "automation deferred");
                (AutomationOutcome::Deferred { reason, frozen }, None)
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
            let EngineStatus {
                pending,
                error,
                view,
            } = status_rx.borrow_and_update().clone();
            let Some(mut board) = snapshot.data().map(|board| (**board).clone()) else {
                continue;
            };
            // The paused chip stays the inventory's: it reads durable state
            // and is the witness the pause toggle's Pending settles against.
            board.automations_pending = pending;
            board.automations_view = view;
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
    view: wt_tui::AutomationsView,
}

/// The engine's state as the board shows it: the chip's count and the
/// automations overlay's lists.
fn engine_status(engine: &AutomationEngine, error: Option<String>) -> EngineStatus {
    let queued = engine.queued();
    EngineStatus {
        pending: queued.len(),
        error,
        view: wt_tui::AutomationsView {
            enabled: true,
            running: engine
                .running()
                .into_iter()
                .map(|fire| wt_tui::AutomationRunView {
                    rule: fire.rule,
                    target: fire.target,
                    detail: wt_core::sanitize_terminal_text(&fire.detail),
                    since_ms: fire.since_ms,
                })
                .collect(),
            queued: queued
                .into_iter()
                .map(|fire| wt_tui::AutomationQueueView {
                    rule: fire.rule,
                    target: fire.target,
                    detail: wt_core::sanitize_terminal_text(&fire.detail),
                    queued_at_ms: fire.queued_at_ms,
                    settle_at_ms: fire.settle_at_ms,
                    waiting_for: fire.waiting_for,
                })
                .collect(),
            recent: engine
                .recent()
                .iter()
                .map(|record| wt_tui::AutomationRecordView {
                    rule: record.rule_id.clone(),
                    target: record.slug.clone(),
                    state: record.state.clone(),
                    at_ms: record.at_ms,
                    reason: record
                        .reason
                        .as_deref()
                        .map(wt_core::sanitize_terminal_text),
                })
                .collect(),
        },
    }
}

fn paused(facts: &Option<PreparedAutomationFacts>) -> bool {
    facts.as_ref().is_some_and(|facts| facts.globally_paused)
}

/// The fires that joined and that left the reported queue between two
/// passes. One wait is a fire's identity and when it queued: two fires of
/// one rule for one worktree are two waits, and so is a fire that returns.
fn queue_changes<'a>(
    before: &'a [QueuedFire],
    after: &'a [QueuedFire],
) -> (Vec<&'a QueuedFire>, Vec<&'a QueuedFire>) {
    let wait = |fire: &'a QueuedFire| (fire.id.as_str(), fire.queued_at_ms);
    let previous = before.iter().map(wait).collect::<BTreeSet<_>>();
    let current = after.iter().map(wait).collect::<BTreeSet<_>>();
    (
        after
            .iter()
            .filter(|fire| !previous.contains(&wait(fire)))
            .collect(),
        before
            .iter()
            .filter(|fire| !current.contains(&wait(fire)))
            .collect(),
    )
}

/// Log a fire when it queues and when it leaves the queue, so a fire that
/// waits forever leaves a trace. Both records carry the engine, the fire
/// and when it queued, so each queued record has exactly one left record
/// unless its engine died first; the next engine's start record ends those.
fn report_queue_changes(
    engine: &str,
    before: &[QueuedFire],
    after: &[QueuedFire],
    now_ms: i64,
    outcome: impl Fn(&QueuedFire) -> &'static str,
) {
    let (queued, left) = queue_changes(before, after);
    for fire in queued {
        tracing::info!(
            engine,
            fire = %fire.id,
            queued_at_ms = fire.queued_at_ms,
            rule = %fire.rule,
            target = %fire.target,
            detail = %fire.detail,
            waiting_for = %fire.waiting_for,
            "automation queued"
        );
    }
    for fire in left {
        tracing::info!(
            engine,
            fire = %fire.id,
            queued_at_ms = fire.queued_at_ms,
            rule = %fire.rule,
            target = %fire.target,
            waited_ms = now_ms.saturating_sub(fire.queued_at_ms),
            outcome = outcome(fire),
            "automation left the queue"
        );
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

/// A promotion moves only the production branch, which no worktree snapshot
/// reflects, so the landing evidence is also keyed on its tips. Read at most
/// every 30 seconds, and only when a rule waits on a production landing.
#[derive(Default)]
struct ProductionTip {
    checked: Option<Instant>,
    tip: Option<String>,
}

impl ProductionTip {
    const INTERVAL: Duration = Duration::from_secs(30);

    async fn current(&mut self, ctx: &AppContext, watches_await: bool) -> Option<String> {
        let branch = &ctx.config.branch;
        let production = branch
            .production
            .as_deref()
            .filter(|_| branch.separate_production())?;
        let watched = watches_await
            || ctx.config.automations.iter().any(|rule| {
                matches!(
                    rule.on,
                    wt_config::AutomationTrigger::StatusVerificationDue
                        | wt_config::AutomationTrigger::StatusVerificationOverdue
                )
            });
        if !watched || self.checked.is_some_and(|at| at.elapsed() < Self::INTERVAL) {
            return self.tip.clone();
        }
        self.checked = Some(Instant::now());
        let output = wt_ops::commands::resolve::run_git(
            ctx,
            &ctx.config.paths.main_clone,
            [
                "for-each-ref",
                "--format=%(objectname)",
                &format!("refs/heads/{production}"),
                &format!("refs/remotes/origin/{production}"),
            ],
        )
        .await;
        self.tip = output
            .ok()
            .filter(|output| output.status.success())
            .map(|output| output.stdout_text().trim().to_owned());
        self.tip.clone()
    }
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
                "guide",
                "mergeArm",
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
        // Only which events wait matters; the cursor moves on quiet polls.
        "mcpEvents": state["mcpEvents"].as_object().map(|rules| rules.iter().map(|(rule, entry)| {
            let ids = entry["pending"].as_array().into_iter().flatten().map(|event| event["eventId"].clone()).collect::<Vec<_>>();
            (rule.clone(), serde_json::json!({"subscription": entry["subscription"], "pending": ids}))
        }).collect::<serde_json::Map<_, _>>()),
        "removedPauses": removed_pauses,
        "watches": crate::watch_facts::identity(state),
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
    serde_json::json!({"slugs": slugs, "removals": removals, "watches": crate::watch_facts::identity(state)})
}

/// A failed session lane keeps its last data as live, so it confirms nothing
/// here: a session started since then would read as absent and its slug as
/// idle. Discoveries stop while the inventory is failed and go with it.
fn confirm_session_inputs(
    sessions: &LeafHandle<SessionDiscoveries>,
    inventory: &LeafHandle<SessionInventory>,
    sessions_snapshot: &wt_runtime::SourceSnapshot<SessionDiscoveries>,
    inventory_snapshot: &wt_runtime::SourceSnapshot<SessionInventory>,
) -> (
    Option<Confirmed<SessionDiscoveries>>,
    Option<Confirmed<SessionInventory>>,
) {
    let failed = |state: &SourceState| matches!(state, SourceState::Failed(_));
    let inventory_failed = failed(inventory_snapshot.state());
    let discoveries_current = !inventory_failed && !failed(sessions_snapshot.state());
    (
        discoveries_current
            .then(|| sessions.confirm(sessions_snapshot))
            .flatten(),
        (!inventory_failed)
            .then(|| inventory.confirm(inventory_snapshot))
            .flatten(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wt_runtime::SourceSnapshot;

    #[test]
    fn only_a_recorded_non_zero_shell_exit_is_a_definite_failure() {
        use wt_app_core::automation_engine::RunEnd;
        let end = |kind: &str, status: &str, exit: Option<i32>| {
            let meta: wt_actions::ActionMeta = serde_json::from_value(serde_json::json!({
                "version": 1, "slug": "row", "runId": "r", "kind": kind, "actionId": "a",
                "actionName": "A", "prompt": "", "affects": [], "startedAt": 1,
                "status": status, "exitCode": exit,
            }))
            .unwrap();
            run_end(&meta)
        };
        assert_eq!(
            end("shell", "failed", Some(2)),
            RunEnd::ShellFailed { exit_code: 2 }
        );
        // The worker died before recording an exit: it may have succeeded.
        assert_eq!(end("shell", "failed", None), RunEnd::ShellUnknown);
        for (kind, status, exit) in [
            ("shell", "succeeded", Some(0)),
            ("shell", "killed", None),
            ("claude", "failed", Some(1)),
            ("claude", "failed", None),
        ] {
            assert_eq!(end(kind, status, exit), RunEnd::Other, "{kind} {status}");
        }
    }

    /// The queue log pairs a queued record with a left record by fire and
    /// queue time. Keyed by rule and target alone, a second fire of the rule
    /// for the same worktree replaced the first without either record.
    #[test]
    fn each_wait_in_the_queue_is_reported_once_as_queued_and_once_as_left() {
        let fire = |id: &str, queued_at_ms| QueuedFire {
            id: id.into(),
            rule: "fix-ci".into(),
            target: "row".into(),
            detail: String::new(),
            queued_at_ms,
            settle_at_ms: queued_at_ms,
            waiting_for: "its settle delay".into(),
        };
        let waits = |fires: Vec<&QueuedFire>| {
            fires
                .into_iter()
                .map(|fire| (fire.id.clone(), fire.queued_at_ms))
                .collect::<Vec<_>>()
        };
        let before = [fire("fix-ci|row@a", 10), fire("fix-ci|row@b", 20)];
        // The first fire left and a new one for the same rule and worktree
        // queued; the second went and came back (a deferral keeps its age,
        // a new condition does not).
        let after = [fire("fix-ci|row@c", 30), fire("fix-ci|row@b", 40)];
        let (queued, left) = queue_changes(&before, &after);
        assert_eq!(
            waits(queued),
            [
                ("fix-ci|row@c".to_owned(), 30),
                ("fix-ci|row@b".to_owned(), 40)
            ]
        );
        assert_eq!(
            waits(left),
            [
                ("fix-ci|row@a".to_owned(), 10),
                ("fix-ci|row@b".to_owned(), 20)
            ]
        );
        // A fire still waiting is reported again by neither record, however
        // its reason changes.
        let mut same = before.clone();
        same[0].waiting_for = "session busy on row".into();
        let (queued, left) = queue_changes(&before, &same);
        assert!(queued.is_empty() && left.is_empty());
        // Everything queued leaves when the engine stops.
        assert_eq!(queue_changes(&before, &[]).1.len(), 2);
    }

    /// A manager waiting on a person must not hold the automation loop. The
    /// look gives up after one bounded wait, however many removed rows have
    /// something due, and records nothing as told, so a later look asks
    /// again and the manager is still told exactly once.
    #[tokio::test]
    async fn a_manager_waiting_on_a_person_is_asked_once_per_look_and_told_later() {
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        {
            let config = std::sync::Arc::make_mut(&mut fixture.ctx.config);
            config.tmux.socket = format!(
                "wt-removed-due-{}",
                fixture._root.path().file_name().unwrap().to_string_lossy()
            );
            config.harness.primary = wt_core::HarnessId::Claude;
        }
        let context = &fixture.ctx;
        // This test process stands in for the live, waiting manager.
        let cwd = std::fs::canonicalize(&context.config.paths.main_clone).unwrap();
        let sessions = context.home.join(".claude/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join(format!("{}.json", std::process::id())),
            serde_json::json!({"pid": std::process::id(),
                "sessionId": wt_harness::claude_session_id(&cwd, Some("manager")), "cwd": cwd,
                "name": "manager~manager", "status": "waiting", "waitingFor": "question",
                "startedAt": 1, "updatedAt": 2})
            .to_string(),
        )
        .unwrap();
        let now = 1_795_521_600_000_i64;
        context
            .database
            .call(move |store| {
                for slug in ["gone-a", "gone-b"] {
                    store.record_slug_created(slug, "2026-11-01T00:00:00Z")?;
                    let status = wt_store::WorkStatusRecord {
                        state: "ready".into(),
                        at: "2026-11-24T12:00:00Z".into(),
                        note: None,
                        risk: None,
                        sha: None,
                        by: Some(slug.into()),
                        blocked_on: None,
                        verify_after_merge: None,
                        verify_in: None,
                        extra: Default::default(),
                    }
                    .with_check(wt_core::VerifyStage::Staging, "open the page");
                    store.set_slug_work_status(slug, Some(&status))?;
                    store.record_removed_worktrees(
                        &[wt_store::RemovedWorktree {
                            slug: slug.into(),
                            branch: format!("feature/{slug}"),
                            removed_at: "2026-11-24T12:00:01Z".into(),
                            work: None,
                            automations_paused: None,
                            extra: [("landedOnAtRemoval".to_owned(), serde_json::json!("base"))]
                                .into_iter()
                                .collect(),
                        }],
                        now,
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let separate = context.config.branch.separate_production();
        let untold = |context: &AppContext| {
            let database = context.database.clone();
            async move {
                database
                    .call(|store| Ok(store.read_removed_worktrees()?))
                    .await
                    .unwrap()
                    .iter()
                    .filter(|entry| !wt_store::removed_due_untold(entry, separate, now).is_empty())
                    .count()
            }
        };
        assert_eq!(untold(context).await, 2, "the fixture owes nothing");
        let mut told = BTreeSet::new();
        // One wake waits about five seconds on the person; two would not fit.
        let look = tokio::time::timeout(
            Duration::from_secs(9),
            tell_removed_due(context, &BTreeSet::new(), now, &mut told, true),
        )
        .await;
        // The loop holds further wakes after a deferral: such a look sends
        // nothing and waits on nobody.
        let held = tokio::time::timeout(
            Duration::from_secs(2),
            tell_removed_due(context, &BTreeSet::new(), now, &mut told, false),
        )
        .await;
        let still_untold = untold(context).await;
        fixture.close().await.unwrap();
        let deferred = look
            .expect("the look waited on the manager once per removed row")
            .unwrap();
        assert!(deferred.is_some(), "a deferral says why");
        assert_eq!(
            held.expect("a held look waited on the manager").unwrap(),
            None
        );
        assert!(told.is_empty(), "{told:?}");
        assert_eq!(still_untold, 2);
    }

    /// A manager sitting at a question defers every wake after a bounded
    /// wait. State changes many times in a recheck interval; only one of
    /// those looks may pay that wait.
    #[test]
    fn a_deferring_manager_is_woken_once_per_recheck_interval_not_per_snapshot() {
        let mut looks = RemovedDueLooks::default();
        let start = 1_000_000;
        let mut wakes = 0;
        // A new metadata revision every second for two intervals.
        for second in 0..(2 * REMOVED_DUE_RECHECK_MS / 1000) {
            let (revision, now) = (second as u64, start + second * 1000);
            assert!(looks.due(revision, now), "every revision is looked over");
            let wake = looks.may_wake(now);
            wakes += usize::from(wake);
            looks.looked(revision, now, wake.then_some(true));
        }
        assert_eq!(wakes, 2);
        // With nothing changing, the look and the wake come due together.
        let (revision, last) = looks.seen.unwrap();
        assert!(!looks.due(revision, last - 1));
        let until = looks.wake_held_until.unwrap();
        assert!(looks.due(revision, until.max(last)) && looks.may_wake(until));
        // A wake that reaches the manager ends the hold; a failed look
        // neither starts nor ends one.
        looks.looked(revision, until, Some(false));
        assert!(looks.may_wake(until + 1));
        looks.looked(revision, until, Some(true));
        looks.looked(revision, until + 1, None);
        assert!(!looks.may_wake(until + 2));
    }

    /// Another live process may launch a run while this one starts. Its
    /// awaited entry must never be read without its run record, or the run
    /// would be ended as unknown under its owner.
    #[tokio::test]
    async fn awaited_runs_are_read_back_before_their_records_are_looked_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let now = now_ms();
        let write = |keys: &[&str]| {
            let mut root = serde_json::json!({"version": 1, "fired": {}, "awaited": {}});
            for key in keys {
                let mut fire = fire("row");
                fire.fire_keys = vec![(*key).to_owned()];
                root["fired"][key] = serde_json::json!({
                    "state": "delivered", "claimId": format!("claim-{key}"),
                    "ruleId": "nudge", "slug": "row", "at": now - 1_000,
                });
                root["awaited"][format!("id-{key}")] = serde_json::json!({
                    "claim": {
                        "claimId": format!("claim-{key}"), "keys": [key], "ruleId": "nudge",
                        "slug": "row", "kind": "headlessAction", "claimedAtMs": now - 1_000,
                        "breakerReserved": false,
                    },
                    "fire": fire, "queuedAtMs": now - 2_000, "deliveredAtMs": now - 1_000,
                    "failedRuns": 0, "retryOnFailure": true,
                });
            }
            std::fs::write(&path, root.to_string()).unwrap();
        };
        let cancel = tokio_util::sync::CancellationToken::new();

        // Nothing awaited: no run is looked up.
        write(&[]);
        let mut idle = AutomationEngine::new(AutomationLedger::new(&path), cancel.clone());
        let none = restore_runs(&mut idle, now, |_| async { panic!("nothing to look up") })
            .await
            .unwrap();
        assert!(none.is_none());

        write(&["first"]);
        let ledger = AutomationLedger::new(&path);
        let mut engine = AutomationEngine::new(ledger.clone(), cancel.clone());
        let runs = restore_runs(&mut engine, now, |since_ms| {
            // The lookup reaches back to the restored run's claim, however
            // many runs there are since.
            assert_eq!(since_ms, u64::try_from(now - 1_000).unwrap());
            // Meanwhile another process launches a run and records it.
            write(&["first", "second"]);
            async { Ok(Vec::new()) }
        })
        .await
        .unwrap();
        assert_eq!(runs, Some(Vec::new()));
        // No record is left of the first run: it is ended as unknown. The
        // second was never read back, so it stays its owner's to settle.
        engine.end_lost_runs(&[], now, &cancel).await.unwrap();
        let unknown = engine.take_unknown_runs();
        assert_eq!(unknown.len(), 1, "{unknown:?}");
        assert!(!engine.awaits_runs());
        let awaited = ledger.awaited().await.unwrap();
        assert_eq!(awaited.keys().collect::<Vec<_>>(), ["id-second"]);
        assert_eq!(ledger.snapshot().await.unwrap().unknown_outcomes().len(), 1);
    }

    fn fire(slug: &str) -> wt_automations::AutomationFire {
        wt_automations::AutomationFire {
            rule: wt_config::AutomationDef {
                id: "nudge".into(),
                ..Default::default()
            },
            slug: slug.into(),
            quiesce_slugs: Vec::new(),
            fire_keys: vec!["key".into()],
            stack_id: None,
            close_issue: None,
            delete_branch: None,
            delete_branch_pr: None,
            branch_range: None,
            frozen_vars: None,
            frozen_pr: None,
            mcp_event: None,
            watch: None,
            detail: String::new(),
            check: None,
        }
    }

    #[tokio::test]
    async fn finished_and_skipped_fires_are_recorded_and_pending_ones_are_not() {
        use wt_app_core::automation_engine::{DeliverySlot, SkippedFire};
        use wt_core::{AutomationOutcomeKind, WorktreeEventKind};
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let delivered = AutomationOutcome::Delivered {
            slot: DeliverySlot::None,
        };
        let outcomes = [
            (delivered.clone(), None),
            // A built-in that ran and reported failure.
            (delivered.clone(), Some("nudge: push was rejected")),
            (
                AutomationOutcome::Ambiguous {
                    reason: "reply lost".into(),
                    slot: DeliverySlot::Session,
                },
                None,
            ),
            // Both run again later, so neither is an event yet.
            (AutomationOutcome::NotStarted, None),
            (
                AutomationOutcome::Deferred {
                    reason: "waiting".into(),
                    frozen: Default::default(),
                },
                None,
            ),
        ];
        for (outcome, error) in &outcomes {
            if let Some(event) = fire_event(&fire("one"), outcome, *error) {
                wt_app_core::worktree_history::record(&fixture.ctx, "one", event).await;
            }
        }
        assert_eq!(
            fire_event(&fire(wt_automations::FLEET_SLUG), &delivered, None),
            None,
            "a fleet fire belongs to no worktree"
        );
        record_skipped(
            &fixture.ctx,
            vec![
                SkippedFire {
                    slug: "one".into(),
                    rule: "nudge".into(),
                    reason: "session busy on one".into(),
                },
                SkippedFire {
                    slug: wt_automations::FLEET_SLUG.into(),
                    rule: "nudge".into(),
                    reason: "busy".into(),
                },
            ],
        )
        .await;
        let recorded: Vec<_> = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_worktree_events("one", 80)?))
            .await
            .unwrap()
            .into_iter()
            .map(|event| match event.event {
                WorktreeEventKind::Automation {
                    rule,
                    outcome,
                    reason,
                } => {
                    assert_eq!(rule, "nudge");
                    (outcome, reason)
                }
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            recorded,
            [
                (AutomationOutcomeKind::Delivered, None),
                (
                    AutomationOutcomeKind::Failed,
                    Some("push was rejected".to_owned())
                ),
                (
                    AutomationOutcomeKind::Unconfirmed,
                    Some("reply lost".to_owned())
                ),
                (
                    AutomationOutcomeKind::Skipped,
                    Some("session busy on one".to_owned())
                ),
            ]
        );
        let fleet = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_worktree_events(wt_automations::FLEET_SLUG, 80)?))
            .await
            .unwrap();
        assert!(fleet.is_empty());
        fixture.close().await.unwrap();
    }

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
                deploys: crate::deploy_source::start(&scope, &fixture.ctx).view(),
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

    #[tokio::test]
    async fn a_failed_session_lane_confirms_no_session_facts() {
        use wt_core::Domain;
        use wt_runtime::{
            LeafPublisher, Persistence, RefreshPolicy, SourceSpec, Stamp, leaf_channel,
        };

        fn leaf<T: Default + Send + Sync + 'static>(
            scope: &TaskScope,
        ) -> (LeafHandle<T>, LeafPublisher<T>) {
            let (leaf, publisher) = leaf_channel(
                scope,
                SourceSpec {
                    name: "session-fixture",
                    domain: Domain::Sessions,
                    persistence: Persistence::Never("isolated test"),
                    policy: RefreshPolicy::default(),
                },
            );
            publisher.publish_live(T::default(), Stamp::now());
            (leaf, publisher)
        }
        let scope = TaskScope::new();
        let (sessions, sessions_publisher) = leaf::<SessionDiscoveries>(&scope);
        let (inventory, inventory_publisher) = leaf::<SessionInventory>(&scope);
        let confirm = || {
            confirm_session_inputs(
                &sessions,
                &inventory,
                &sessions.view().snapshot(),
                &inventory.view().snapshot(),
            )
        };
        let (discovered, running) = confirm();
        assert!(discovered.is_some() && running.is_some());

        // A slug whose session started after the last good tmux pass is
        // absent from the kept inventory; it must read as busy, not idle.
        inventory_publisher.set_state(SourceState::Failed(
            "tmux session inventory: timeout".into(),
        ));
        let (discovered, running) = confirm();
        assert!(discovered.is_none() && running.is_none());
        let mut facts = PreparedAutomationFacts {
            rows: vec![wt_automations::AutomationRow {
                slug: "one".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        automation_facts::refresh_sessions(&mut facts, discovered.as_ref(), running.as_ref());
        assert!(facts.quiescence["one"].session_busy);
        assert!(facts.quiescence["one"].action_running);

        // A failed discovery pass keeps older per-target entries: the
        // inventory is still current, the discoveries are not.
        inventory_publisher.publish_live(SessionInventory::default(), Stamp::now());
        sessions_publisher.publish_live_with_state(
            SessionDiscoveries::default(),
            Stamp::now(),
            SourceState::Failed("one Codex: timeout".into()),
        );
        let (discovered, running) = confirm();
        assert!(discovered.is_none() && running.is_some());
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
