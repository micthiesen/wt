//! Host-local automation engine adapter over the shared source snapshots.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
};
use wt_automations::AutomationLedger;
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel};
use wt_tui::Board;

use crate::{
    automation_engine::{AutomationEngine, AutomationOutcome, AutomationPassInput, ClaimedFire},
    automation_facts::{self, AutomationInputs, PreparedAutomationFacts},
    context::AppContext,
    local_source::Metadata,
    session_activity::SessionActivitySnapshot,
    session_source::{SessionDiscoveries, SessionInventory},
};

#[derive(Clone)]
pub struct AutomationSources {
    pub board: SourceHandle<Board>,
    pub git: SourceHandle<Vec<wt_vcs::WorktreeSnapshot>>,
    pub metadata: SourceHandle<Metadata>,
    pub github: SourceHandle<wt_github::GithubData>,
    pub sessions: SourceHandle<SessionDiscoveries>,
    pub inventory: SourceHandle<SessionInventory>,
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
}

pub fn start(scope: &TaskScope, ctx: &AppContext, sources: AutomationSources) -> AutomationHandle {
    if ctx.config.automations.is_empty() || std::env::var("WT_AUTOMATIONS").as_deref() == Ok("off")
    {
        return passthrough(scope, sources.board);
    }
    let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
    let commands = AutomationCommands { cancel: cancel_tx };
    let (status_tx, status_rx) = watch::channel((0usize, None::<String>));
    let board = overlay(scope, sources.board.clone(), status_rx);
    let token = scope.token();
    let context = ctx.clone();
    scope.spawn(async move {
        let ledger = AutomationLedger::new(context.config.paths.cache_root.join("automations.json"));
        let mut engine = AutomationEngine::new(ledger, token.clone());
        let delivered = match crate::actions::service(&context) {
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
        let mut requirements_board_revision = None;
        let mut latest_error: Option<String> = None;
        let mut dispatches: JoinSet<(ClaimedFire, AutomationOutcome, Option<String>)> = JoinSet::new();
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
                completed = dispatches.join_next(), if !dispatches.is_empty() => {
                    match completed {
                        Some(Ok((claim, outcome, error))) => {
                            if let Some(error) = error { latest_error = Some(error); }
                            let advanced_tip = claim.fire().branch_range.as_ref().map(|range| (range.branch.clone(), range.to.clone()));
                            let started = !matches!(outcome, AutomationOutcome::NotStarted);
                            if let Err(error) = engine.finish(&claim, outcome, now_ms()).await {
                                latest_error = Some(format!("automation ledger finish: {error:#}"));
                            }
                            if started && let Some((branch, tip)) = advanced_tip {
                                let update = [(branch.clone(), tip.clone())].into_iter().collect();
                                match persist_tips(&context, &update).await {
                                    Ok(()) => { persisted_tips.insert(branch, tip); }
                                    Err(error) => latest_error = Some(format!("cannot persist branch tip after dispatch: {error:#}")),
                                }
                            }
                            let _ = status_tx.send((engine.pending_count(), latest_error.clone()));
                        }
                        Some(Err(error)) => latest_error = Some(format!("automation dispatch task failed: {error}")),
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
            let github_fresh = github.state == SourceState::Ready
                && github.updated_at.is_some_and(|at| at.elapsed() <= Duration::from_secs(90));
            let metadata_key = metadata_facts_key(metadata.data.as_deref());
            let key = (
                git.data.as_deref().cloned(),
                git.state.clone(),
                metadata_key,
                metadata.state.clone(),
                github.data.as_deref().cloned(),
                github.state.clone(),
                github_fresh,
            );
            if cache_key.as_ref() != Some(&key) {
                if previous_git_revision != Some(git.revision) {
                    if let Some(snapshots) = git.data.as_deref() {
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
                    previous_git_revision = Some(git.revision);
                }
                let pass_edits = merged_edits(edits.data.as_deref(), &last_edit);
                let lifecycle_key = (
                    git.data.as_deref().cloned(),
                    lifecycle_metadata_key(metadata.data.as_deref()),
                    facts_github(&github),
                    github_fresh,
                );
                if lifecycle_cache_key.as_ref() != Some(&lifecycle_key) {
                    match automation_facts::lifecycle_facts(
                        &context,
                        &git,
                        &metadata,
                        &facts_github(&github),
                        &context.config.automations,
                    ).await {
                        Ok(next) => {
                            lifecycle_cache = next;
                            lifecycle_cache_key = Some(lifecycle_key);
                        }
                        Err(error) => {
                            latest_error = Some(format!("automation facts unavailable: lifecycle evidence: {error:#}"));
                            let _ = status_tx.send((engine.pending_count(), latest_error.clone()));
                            continue;
                        }
                    }
                }
                let prepared = automation_facts::prepare(&context, AutomationInputs {
                    git: &git,
                    metadata: &metadata,
                    github: &github,
                    discoveries: &sessions,
                    inventory: &inventory,
                    activity: &activity,
                    board: &base_board,
                    last_edit_at_ms: &pass_edits,
                    rules: &context.config.automations,
                    lifecycle: &lifecycle_cache,
                }).await;
                match prepared {
                    Ok(mut next) => {
                        if !next.initial_branch_tips.is_empty() {
                            if let Err(error) = persist_tips(&context, &next.initial_branch_tips).await {
                                latest_error = Some(format!("automation facts unavailable: cannot persist branch tip baseline: {error:#}"));
                                let _ = status_tx.send((engine.pending_count(), latest_error.clone()));
                                continue;
                            }
                            persisted_tips.extend(next.initial_branch_tips.clone());
                        }
                        for (branch, tip) in &mut next.context.branch_tips {
                            if let Some(seen) = persisted_tips.get(branch) { tip.seen = Some(seen.clone()); }
                        }
                        facts = Some(next); cache_key = Some(key); latest_error = None;
                    }
                    Err(error) => { cache_key = Some(key); latest_error = Some(format!("automation facts unavailable: {error:#}")); }
                }
            }
            if latest_error.as_deref().is_some_and(|error| error.starts_with("automation facts unavailable:")) {
                let _ = status_tx.send((engine.pending_count(), latest_error.clone()));
                continue;
            }
            let Some(facts) = facts.as_mut() else { continue; };
            let current_edits = merged_edits(edits.data.as_deref(), &last_edit);
            automation_facts::refresh_quiescence_edits(
                facts,
                &current_edits,
                activity.data.as_deref(),
            );
            for (branch, tip) in &mut facts.context.branch_tips {
                if let Some(seen) = persisted_tips.get(branch) { tip.seen = Some(seen.clone()); }
            }
            automation_facts::refresh_sessions(facts, sessions.data.as_deref(), inventory.data.as_deref());
            if requirements_board_revision != Some(base_board.revision) {
                automation_facts::refresh_requirements(&context, facts, base_board.data.as_deref());
                requirements_board_revision = Some(base_board.revision);
            }
            facts.context.now_ms = now_ms();
            facts.context.github_fresh = github.state == SourceState::Ready && github.updated_at.is_some_and(|at| at.elapsed() <= Duration::from_secs(90));
            let pass = pass_input(&context, facts, facts.context.now_ms);
            let tick = match engine.tick(pass).await {
                Ok(tick) => tick,
                Err(error) => { latest_error = Some(format!("automation engine: {error:#}")); continue; }
            };
            for claim in tick.claims {
                let context = context.clone();
                let github = facts_github(&github);
                let board = base_board.data.as_deref().cloned();
                dispatches.spawn(async move { dispatch_claim(context, claim, github, board).await });
            }
            let _ = status_tx.send((tick.pending, latest_error.clone()));
        }
        while let Some(result) = dispatches.join_next().await {
            if let Ok((claim, outcome, _)) = result
                && let Err(error) = engine.finish(&claim, outcome, now_ms()).await
            {
                tracing::warn!(%error, "cannot finish automation dispatch during shutdown");
            }
        }
    });
    AutomationHandle { board, commands }
}

fn passthrough(scope: &TaskScope, board: SourceHandle<Board>) -> AutomationHandle {
    let (cancel_tx, mut cancel_rx) = mpsc::channel(1);
    let commands = AutomationCommands { cancel: cancel_tx };
    let token = scope.token();
    scope.spawn(async move {
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
    AutomationHandle { board, commands }
}

async fn dispatch_claim(
    context: AppContext,
    claim: ClaimedFire,
    github: wt_github::GithubData,
    board: Option<Board>,
) -> (ClaimedFire, AutomationOutcome, Option<String>) {
    let fire = claim.fire().clone();
    let (outcome, error) = if fire.rule.run.starts_with("builtin:") {
        match crate::automation_builtins::execute(&context, &fire).await {
            crate::automation_builtins::BuiltinExecution::Retry { reason } => {
                tracing::info!(rule = %fire.rule.id, %reason, "automation will retry");
                (AutomationOutcome::NotStarted, None)
            }
            crate::automation_builtins::BuiltinExecution::Completed { message } => {
                tracing::info!(rule = %fire.rule.id, %message, "automation completed");
                (
                    AutomationOutcome::Delivered {
                        slot: crate::automation_engine::DeliverySlot::None,
                    },
                    None,
                )
            }
            crate::automation_builtins::BuiltinExecution::Failed { message } => {
                tracing::warn!(rule = %fire.rule.id, %message, "automation failed");
                (
                    AutomationOutcome::Delivered {
                        slot: crate::automation_engine::DeliverySlot::None,
                    },
                    Some(format!("{}: {message}", fire.rule.id)),
                )
            }
        }
    } else {
        (
            crate::action_dispatch::dispatch_automation(&context, &fire, &github, board.as_ref())
                .await,
            None,
        )
    };
    (claim, outcome, error)
}

fn overlay(
    scope: &TaskScope,
    input: SourceHandle<Board>,
    mut status_rx: watch::Receiver<(usize, Option<String>)>,
) -> SourceHandle<Board> {
    let (source, mut publisher) = source_channel();
    let cancel = scope.token();
    let mut updates = input.subscribe();
    scope.spawn(async move {
        updates.mark_changed();
        let mut previous: Option<(Board, SourceState)> = None;
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
            let (pending, error) = status_rx.borrow_and_update().clone();
            let Some(mut board) = snapshot.data.as_deref().cloned() else {
                continue;
            };
            board.activity.retain(|line| {
                !line.text.starts_with("Automations:")
                    && !line.text.starts_with("Automation error:")
            });
            board.attention.retain(|line| {
                !line.text.starts_with("Automations:")
                    && !line.text.starts_with("Automation error:")
            });
            if pending > 0 {
                crate::activity_source::append_activity(
                    &mut board,
                    "INFO",
                    "Automations",
                    &format!("Automations: {pending} pending"),
                );
            }
            if let Some(error) = error {
                crate::activity_source::append_attention(
                    &mut board,
                    "Automations",
                    &format!("Automation error: {error}"),
                );
            }
            if previous.as_ref() != Some(&(board.clone(), snapshot.state.clone())) {
                let state = snapshot.state.clone();
                publisher.publish(SourceSnapshot {
                    data: Some(Arc::new(board.clone())),
                    state,
                    updated_at: snapshot.updated_at,
                    revision: 0,
                });
                previous = Some((board, snapshot.state));
            }
        }
    });
    source
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

fn facts_github(snapshot: &SourceSnapshot<wt_github::GithubData>) -> wt_github::GithubData {
    if snapshot.state == SourceState::Ready
        && snapshot
            .updated_at
            .is_some_and(|at| at.elapsed() <= Duration::from_secs(90))
    {
        snapshot.data.as_deref().cloned().unwrap_or_default()
    } else {
        wt_github::GithubData::default()
    }
}

fn now_ms() -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
        .unwrap_or(i64::MAX)
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
                "ghIssue",
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
    serde_json::Value::Object(slugs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn passthrough_keeps_board_refresh_and_stops_command_responder() {
        let scope = TaskScope::new();
        let (board, publisher) = source_channel();
        publisher.publish(SourceSnapshot {
            data: Some(Arc::new(Board::default())),
            state: SourceState::Ready,
            updated_at: Some(tokio::time::Instant::now()),
            revision: 1,
        });
        let handle = passthrough(&scope, board.clone());
        let mut updates = handle.board.subscribe();
        assert_eq!(updates.borrow().revision, 1);
        assert_eq!(handle.commands.cancel_pending().await.unwrap(), 0);
        publisher.publish(SourceSnapshot {
            data: Some(Arc::new(Board::default())),
            state: SourceState::Ready,
            updated_at: Some(tokio::time::Instant::now()),
            revision: 2,
        });
        tokio::time::timeout(
            Duration::from_secs(1),
            updates.wait_for(|snapshot| snapshot.revision == 2),
        )
        .await
        .unwrap()
        .unwrap();

        scope.cancel();
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
        assert!(handle.commands.cancel_pending().await.is_err());
    }
}
