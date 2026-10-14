use anyhow::{Context, Result, bail};
use std::time::{Duration, Instant};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::Arc,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wt_runtime::TaskScope;
use wt_tui::{ActionController, UiReply};

use crate::context::AppContext;

/// Commands serialize only where their resources conflict and drain on shutdown.
/// Independent operations retain capacity while lifecycle jobs are running.
/// Services own their own cancellation and irreversible commit boundaries.
pub struct Controller {
    task: JoinHandle<()>,
    cancel: CancellationToken,
    automations: Arc<crate::automation_source::AutomationDrain>,
}

struct RunningAction {
    lane: Lane,
    request_id: u64,
    generation: u64,
    name: &'static str,
    target: Option<String>,
    started: Instant,
    preparation: bool,
}

#[derive(Clone)]
struct ActionLogContext {
    operation_id: u64,
    operation: &'static str,
    host: String,
    target: Option<String>,
}

impl Controller {
    pub async fn shutdown(self) -> Result<()> {
        // The controller reports the exact accepted operations it is draining.
        // Do not impose a second deadline here: accepted writes own their
        // timeout and commit boundary and cannot safely be cancelled by UI quit.
        let result = self.task.await;
        let automations = self.automations.shutdown().await;
        self.cancel.cancel();
        result.context("action controller exited")?;
        automations
    }
}

pub fn start(
    scope: &TaskScope,
    mut context: AppContext,
    fleet: crate::remote_board::Fleet,
    mut port: ActionController,
) -> Controller {
    let shutdown = scope.token();
    let cancel = fleet.local.context.cancellation.clone();
    let automations = fleet.local.sources.automation_drain.clone();
    context.cancellation = cancel.clone();
    let context = Arc::new(context);
    let fleet = Arc::new(fleet);
    let task = scope.spawn(async move {
        let mut pending = VecDeque::new();
        let mut busy = BTreeSet::new();
        let mut running = tokio::task::JoinSet::new();
        let mut task_lanes = HashMap::new();
        let mut progress_tick = Box::pin(tokio::time::sleep(Duration::from_secs(3)));
        let mut failed_lanes = BTreeSet::new();
        let mut closed = false;
        let mut draining = false;
        loop {
            while let Some((lane, (request, accepted_at))) = take_available(&mut pending, &busy) {
                let wt_tui::UiRequest { id, generation, action: command, cancellation } = request;
                busy.insert(lane.clone());
                let operation_cancellation = command
                    .is_preparation()
                    .then(|| cancellation.unwrap_or_else(|| shutdown.child_token()));
                let mut request_context = (*context).clone();
                if let Some(cancellation) = &operation_cancellation {
                    request_context.cancellation = cancellation.clone();
                }
                let context = Arc::new(request_context);
                let fleet = fleet.clone();
                let replies = port.replies.clone();
                let shutdown = shutdown.clone();
                let task_lane = lane.clone();
                let operation_name = action_name(&command);
                let is_preparation = command.is_preparation();
                let operation_target = action_target(&command);
                let log_context = ActionLogContext {
                    operation_id: id,
                    operation: operation_name,
                    host: lane_host(&task_lane).to_owned(),
                    target: operation_target.clone(),
                };
                let task = running.spawn(async move {
                    let retry = create_retry(&command);
                    let request_replies = RequestReplies { sender: replies, id, generation };
                    let result = if let Some(cancellation) = operation_cancellation {
                        run_preparation_or_cancel(
                            &shutdown,
                            &cancellation,
                            crate::host_dispatch::execute(
                                &context,
                                &fleet,
                                command,
                                &request_replies,
                                &shutdown,
                            ),
                        )
                        .await
                    } else {
                        crate::host_dispatch::execute(&context, &fleet, command, &request_replies, &shutdown).await
                    };
                    let mut reply = action_reply_with_context(result, retry, Some(&log_context));
                    reply.ui_generation = Some(generation);
                    reply.operation_id = Some(id);
                    (lane, reply)
                });
                task_lanes.insert(task.id(), RunningAction {
                    lane: task_lane,
                    request_id: id,
                    generation,
                    name: operation_name,
                    target: operation_target,
                    started: accepted_at,
                    preparation: is_preparation,
                });
            }
            if closed && pending.is_empty() && running.is_empty() { break; }
            tokio::select! {
                biased;
                _ = shutdown.cancelled(), if !draining => {
                    draining = true;
                    port.requests.close();
                    progress_tick.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(3));
                    // Pending prompts and pickers have no accepted side effect
                    // to preserve. Drop them so shutdown drains only work the
                    // user already committed.
                    pending.retain(|(_, (request, _))| !request.action.is_preparation());
                },
                finished = running.join_next_with_id(), if !running.is_empty() => match finished {
                    Some(Ok((id, (lane, reply)))) => {
                        let active = task_lanes.remove(&id);
                        if let Some(active) = &active
                            && (reply.failed || reply.outcome == Some(wt_tui::ReplyOutcome::Uncertain))
                        {
                            tracing::warn!(
                                operation_id = active.request_id,
                                operation = active.name,
                                host = lane_host(&active.lane),
                                target = active.target.as_deref().unwrap_or("-"),
                                uncertain = reply.outcome == Some(wt_tui::ReplyOutcome::Uncertain),
                                "action worker returned a failed or unconfirmed outcome"
                            );
                        }
                        busy.remove(&lane);
                        let reply = sanitize_reply(reply);
                        if let Err(error) = port.replies.send(reply).await
                            && let Some(active) = &active
                        {
                            report_unobserved_reply(active, &error.0);
                        }
                    },
                    Some(Err(error)) => {
                        tracing::error!(%error, "action worker exited unexpectedly");
                        if let Some(active) = task_lanes.remove(&error.id()) {
                            let reply = worker_panic_reply(&active, &error.to_string());
                            let log_context = ActionLogContext {
                                operation_id: active.request_id,
                                operation: active.name,
                                host: lane_host(&active.lane).to_owned(),
                                target: active.target.clone(),
                            };
                            log_action_outcome(
                                Some(&log_context),
                                &reply,
                                reply.outcome == Some(wt_tui::ReplyOutcome::Uncertain),
                            );
                            if let Err(error) = port.replies.send(reply).await {
                                report_unobserved_reply(&active, &error.0);
                            }
                            failed_lanes.insert(active.lane.clone());
                            busy.remove(&active.lane);
                            let skipped = pending.iter().filter(|(queued, _)| lanes_conflict(queued, &active.lane)).count();
                            for (_, (request, _)) in pending.iter().filter(|(queued, _)| lanes_conflict(queued, &active.lane)) {
                                let _ = port.replies.send(UiReply { operation_id: Some(request.id), ..UiReply::failure("Host action worker failed; operation did not start") }).await;
                            }
                            pending.retain(|(queued, _)| !lanes_conflict(queued, &active.lane));
                            if skipped > 0 {
                                let _ = port.replies.send(UiReply::failure(format!("{skipped} queued commands on the failed host were not started. Restart wt before retrying."))).await;
                            }
                        }
                    },
                    None => {},
                },
                command = port.requests.recv(), if !closed && pending.len() < 32 => match command {
                    Some(mut command) => {
                        // Resolve aliases before deciding cancellation ownership as
                        // well as conflicts. Palette title generation is a write.
                        command.action = crate::host_dispatch::rename_as_title(command.action);
                        match lane_for(&command.action) {
                        Ok(lane) if !lane_failed(&lane, &failed_lanes) => pending.push_back((lane, (command, Instant::now()))),
                        Ok(_) => { let _ = port.replies.send(UiReply { operation_id: Some(command.id), ..UiReply::failure("Host command worker failed; this command was not started. Restart wt before retrying.") }).await; },
                        Err(error) => { let _ = port.replies.send(UiReply { operation_id: Some(command.id), ..UiReply::failure(format!("{error:#}")) }).await; },
                        }
                    },
                    None => closed = true,
                },
                _ = &mut progress_tick, if draining && !task_lanes.is_empty() => {
                    for active in task_lanes.values() {
                        eprintln!("{}", shutdown_progress(active, Instant::now()));
                    }
                    for (lane, (request, accepted_at)) in &pending {
                        eprintln!("wt: shutdown waiting: {} on {} is queued ({} elapsed, operation {})",
                            action_name(&request.action),
                            lane_host(lane),
                            elapsed(accepted_at.elapsed()),
                            request.id,
                        );
                    }
                    progress_tick.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(10));
                },
            }
        }
    });
    Controller {
        task,
        cancel,
        automations,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Lane {
    Host(Option<String>),
    /// Create operations serialize with each other, but a durable removal of
    /// an existing target must not hold up creating an unrelated target.
    Creation(Option<String>),
    /// Removal jobs are durable and already own the target's lifecycle lock.
    /// Keep the same target serialized while allowing unrelated work through.
    Removal(Option<String>, String),
    /// Target-scoped metadata writes must not wait for unrelated lifecycle jobs.
    Metadata(Option<String>, String),
    /// Read-only prompts and pickers must not sit behind a long accepted
    /// mutation on the same host. The final mutation still revalidates.
    Preparation(Option<String>),
    FleetCleanup,
    Presentation,
    /// Bounded, device-local controls must remain responsive while host jobs
    /// or interactive handoffs occupy all ordinary action slots.
    ViewControl,
    /// One device terminal, with host-local checkout identity for removal
    /// conflicts. Special slots have no worktree target.
    Terminal(Option<String>, Option<String>),
}

fn action_name(action: &wt_tui::UiAction) -> &'static str {
    match action {
        wt_tui::UiAction::OnHost { action, .. } => action_name(action),
        wt_tui::UiAction::Create { .. } | wt_tui::UiAction::CreateResolved { .. } => {
            "create worktree"
        }
        wt_tui::UiAction::Remove { .. } => "remove worktree",
        wt_tui::UiAction::Cleanup { .. } | wt_tui::UiAction::CleanupTargets { .. } => {
            "clean worktrees"
        }
        wt_tui::UiAction::PrepareCreate { .. } => "prepare worktree creation",
        wt_tui::UiAction::PrepareRemove { .. } => "prepare worktree removal",
        wt_tui::UiAction::PrepareCleanup => "prepare worktree cleanup",
        wt_tui::UiAction::PrepareGithub { .. } => "prepare GitHub action",
        wt_tui::UiAction::PrepareSessions { .. } => "prepare session picker",
        wt_tui::UiAction::PrepareHarnesses { .. } => "prepare harness picker",
        wt_tui::UiAction::ResolveCreation { .. } => "resolve worktree creation",
        wt_tui::UiAction::SetTitle { .. } => "save title",
        wt_tui::UiAction::GenerateTitle { .. } => "generate title",
        wt_tui::UiAction::SetStatus { .. } => "save status",
        wt_tui::UiAction::SetIssueOverride { .. } => "save issue",
        wt_tui::UiAction::Session { .. }
        | wt_tui::UiAction::EnterHarness { .. }
        | wt_tui::UiAction::SelectSession { .. } => "open session",
        _ => "host action",
    }
}

fn action_target(action: &wt_tui::UiAction) -> Option<String> {
    let key = match action {
        wt_tui::UiAction::OnHost { action, .. } => return action_target(action),
        wt_tui::UiAction::Remove { key, .. } | wt_tui::UiAction::PrepareRemove { key } => key,
        wt_tui::UiAction::SetTitle { key, .. }
        | wt_tui::UiAction::GenerateTitle { key }
        | wt_tui::UiAction::SetStatus { key, .. }
        | wt_tui::UiAction::SetIssueOverride { key, .. }
        | wt_tui::UiAction::EnterHarness { key, .. } => key,
        wt_tui::UiAction::Session { key, .. } => key.as_ref()?,
        wt_tui::UiAction::SelectSession { selection } => selection.key.as_ref()?,
        wt_tui::UiAction::CreateResolved { plan } => &plan.key,
        _ => return None,
    };
    match wt_core::parse_worktree_ledger_key(key)? {
        wt_core::WorktreeRef::Local { slug } | wt_core::WorktreeRef::Remote { slug, .. } => {
            Some(slug)
        }
    }
}

fn lane_host(lane: &Lane) -> &str {
    match lane {
        Lane::Host(Some(host))
        | Lane::Creation(Some(host))
        | Lane::Removal(Some(host), _)
        | Lane::Metadata(Some(host), _)
        | Lane::Preparation(Some(host))
        | Lane::Terminal(Some(host), _) => host,
        Lane::Host(None)
        | Lane::Creation(None)
        | Lane::Removal(None, _)
        | Lane::Metadata(None, _)
        | Lane::Preparation(None)
        | Lane::Terminal(None, _) => "local host",
        Lane::FleetCleanup => "local and remote hosts",
        Lane::Presentation | Lane::ViewControl => "local host",
    }
}

fn elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn shutdown_progress(active: &RunningAction, now: Instant) -> String {
    let target = active
        .target
        .as_deref()
        .map(|target| format!(" for {target}"))
        .unwrap_or_default();
    format!(
        "wt: shutdown waiting: {}{} on {} is running ({} elapsed, operation {})",
        active.name,
        target,
        lane_host(&active.lane),
        elapsed(now.saturating_duration_since(active.started)),
        active.request_id,
    )
}

fn report_unobserved_reply(active: &RunningAction, reply: &UiReply) {
    let Some(status) = unobserved_reply_status(reply) else {
        return;
    };
    if active.request_id == 0 {
        return;
    }
    let target = active
        .target
        .as_deref()
        .map(|target| format!(" for {target}"))
        .unwrap_or_default();
    let detail = reply.message.trim();
    if detail.is_empty() {
        eprintln!(
            "wt: shutdown: {} on {}{} {status} (operation {})",
            active.name,
            lane_host(&active.lane),
            target,
            active.request_id,
        );
    } else {
        eprintln!(
            "wt: shutdown: {} on {}{} {status} (operation {}): {detail}",
            active.name,
            lane_host(&active.lane),
            target,
            active.request_id,
        );
    }
}

fn unobserved_reply_status(reply: &UiReply) -> Option<&'static str> {
    if reply.failed {
        Some("failed")
    } else if reply.outcome == Some(wt_tui::ReplyOutcome::Uncertain) {
        Some("has an uncertain outcome")
    } else if matches!(reply.outcome, Some(wt_tui::ReplyOutcome::Accepted(_))) {
        Some("was accepted")
    } else if reply.outcome == Some(wt_tui::ReplyOutcome::NoChange) || reply.notice {
        None
    } else {
        Some("finished")
    }
}

async fn run_preparation_or_cancel<F>(
    shutdown: &CancellationToken,
    cancellation: &CancellationToken,
    operation: F,
) -> Result<UiReply>
where
    F: std::future::Future<Output = Result<UiReply>>,
{
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => Ok(UiReply::no_change("Preparation cancelled during shutdown")),
        _ = cancellation.cancelled() => Ok(UiReply::no_change("Preparation cancelled")),
        result = operation => result,
    }
}

fn lane_failed(lane: &Lane, failed: &BTreeSet<Lane>) -> bool {
    failed.iter().any(|failed| lanes_conflict(lane, failed))
}

fn worker_panic_reply(active: &RunningAction, error: &str) -> UiReply {
    if active.preparation {
        UiReply {
            operation_id: Some(active.request_id),
            ui_generation: Some(active.generation),
            ..UiReply::failure(format!("Preparation worker failed: {error}"))
        }
    } else {
        UiReply {
            operation_id: Some(active.request_id),
            ui_generation: Some(active.generation),
            ..UiReply::uncertain(format!(
                "Action worker stopped unexpectedly: {error}; the outcome is unknown and will not be resent. Check its result before retrying."
            ))
        }
    }
}

pub(crate) fn lane_for(action: &wt_tui::UiAction) -> Result<Lane> {
    if is_cleanup_action(action) {
        // A single confirmation can contain revisions from several hosts.
        // Serialize it as one accepted operation so shutdown drains the whole
        // fleet mutation and no second cleanup races its prepared revisions.
        return Ok(Lane::FleetCleanup);
    }
    let (host, action, _) =
        crate::host_routing::resolve(crate::host_dispatch::rename_as_title(action.clone()))?;
    Ok(
        if matches!(
            action,
            wt_tui::UiAction::SetAttentionSeen { .. }
                | wt_tui::UiAction::SetHistoryActive { .. }
                | wt_tui::UiAction::SetPerf { .. }
        ) {
            Lane::ViewControl
        } else if action.is_terminal_handoff() {
            let key = match action {
                wt_tui::UiAction::Session { key, .. } => key,
                wt_tui::UiAction::SelectSession { selection } => selection.key,
                wt_tui::UiAction::EnterHarness { key, .. } => Some(key),
                _ => None,
            };
            Lane::Terminal(host, key)
        } else if crate::host_routing::controller_owned(&action) {
            Lane::Presentation
        } else if action.is_preparation() {
            Lane::Preparation(host)
        } else if matches!(
            action,
            wt_tui::UiAction::Create { .. } | wt_tui::UiAction::CreateResolved { .. }
        ) {
            Lane::Creation(host)
        } else if let wt_tui::UiAction::Remove { key, .. } = action {
            Lane::Removal(host, key)
        } else if let wt_tui::UiAction::SetTitle { key, .. }
        | wt_tui::UiAction::GenerateTitle { key }
        | wt_tui::UiAction::SetStatus { key, .. }
        | wt_tui::UiAction::SetIssueOverride { key, .. } = action
        {
            Lane::Metadata(host, key)
        } else {
            Lane::Host(host)
        },
    )
}

fn is_cleanup_action(action: &wt_tui::UiAction) -> bool {
    match action {
        wt_tui::UiAction::Cleanup { .. } | wt_tui::UiAction::CleanupTargets { .. } => true,
        wt_tui::UiAction::OnHost { action, .. } => is_cleanup_action(action),
        _ => false,
    }
}

pub(crate) fn take_available<T>(
    pending: &mut VecDeque<(Lane, T)>,
    busy: &BTreeSet<Lane>,
) -> Option<(Lane, T)> {
    // Reserve slots for view controls, read-only preparations, creation, and
    // terminal entry. Unrelated host jobs cannot consume interactive capacity;
    // lane conflicts still serialize terminal handoffs with each other.
    let ordinary_full = busy
        .iter()
        .filter(|lane| {
            !matches!(
                lane,
                Lane::ViewControl
                    | Lane::Preparation(_)
                    | Lane::Creation(_)
                    | Lane::Terminal(_, _)
                    | Lane::Metadata(_, _)
            )
        })
        .count()
        >= 4;
    let preparation_busy = busy.iter().any(|lane| matches!(lane, Lane::Preparation(_)));
    let creation_busy = busy.iter().any(|lane| matches!(lane, Lane::Creation(_)));
    let metadata_busy = busy.iter().any(|lane| matches!(lane, Lane::Metadata(_, _)));
    let position = pending.iter().position(|(lane, _)| {
        let has_capacity = match lane {
            Lane::ViewControl | Lane::Terminal(_, _) => true,
            Lane::Preparation(_) => !preparation_busy,
            Lane::Creation(_) => !creation_busy,
            Lane::Metadata(_, _) => !metadata_busy,
            _ => !ordinary_full,
        };
        has_capacity && busy.iter().all(|active| !lanes_conflict(lane, active))
    })?;
    pending.remove(position)
}

pub(crate) fn lanes_conflict(left: &Lane, right: &Lane) -> bool {
    if left == right {
        return true;
    }
    if matches!(left, Lane::FleetCleanup) || matches!(right, Lane::FleetCleanup) {
        return matches!(
            (left, right),
            (
                Lane::FleetCleanup,
                Lane::Host(_)
                    | Lane::Creation(_)
                    | Lane::Removal(_, _)
                    | Lane::Metadata(_, _)
                    | Lane::Terminal(_, Some(_))
            ) | (
                Lane::Host(_)
                    | Lane::Creation(_)
                    | Lane::Removal(_, _)
                    | Lane::Metadata(_, _)
                    | Lane::Terminal(_, Some(_)),
                Lane::FleetCleanup
            )
        );
    }
    match (left, right) {
        (Lane::Terminal(_, _), Lane::Terminal(_, _)) => true,
        (Lane::Terminal(host, Some(key)), Lane::Removal(other_host, other_key))
        | (Lane::Removal(other_host, other_key), Lane::Terminal(host, Some(key))) => {
            host == other_host && key == other_key
        }
        (Lane::Host(left), Lane::Creation(right))
        | (Lane::Host(left), Lane::Metadata(right, _))
        | (Lane::Metadata(left, _), Lane::Host(right))
        | (Lane::Creation(left), Lane::Host(right))
        | (Lane::Host(left), Lane::Removal(right, _))
        | (Lane::Removal(left, _), Lane::Host(right)) => left == right,
        (Lane::Removal(left_host, left_key), Lane::Removal(right_host, right_key)) => {
            left_host == right_host && left_key == right_key
        }
        (Lane::Metadata(host, key), Lane::Removal(other_host, other_key))
        | (Lane::Removal(other_host, other_key), Lane::Metadata(host, key)) => {
            host == other_host && key == other_key
        }
        _ => false,
    }
}

fn create_retry(action: &wt_tui::UiAction) -> Option<(Option<String>, String)> {
    let (host, action, _) = crate::host_routing::resolve(action.clone()).ok()?;
    match action {
        wt_tui::UiAction::Create { input } => Some((host, input)),
        _ => None,
    }
}

/// A key with nothing to act on (no PR, already at the edge). It answers
/// with a toast, like a success, and is not logged as a failed action.
#[derive(Debug)]
pub struct Notice(pub String);

impl std::fmt::Display for Notice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Notice {}

pub fn notice(message: impl Into<String>) -> anyhow::Error {
    Notice(message.into()).into()
}

#[cfg(test)]
fn action_reply(result: Result<UiReply>, retry: Option<(Option<String>, String)>) -> UiReply {
    action_reply_with_context(result, retry, None)
}

fn action_reply_with_context(
    result: Result<UiReply>,
    retry: Option<(Option<String>, String)>,
    context: Option<&ActionLogContext>,
) -> UiReply {
    let (mut reply, ambiguous) = match result {
        Ok(reply) => (reply, false),
        Err(error) if error.downcast_ref::<Notice>().is_some() => (
            // A notice means nothing was written (e.g. already at the top),
            // so a pending change ends at once instead of waiting.
            UiReply {
                notice: true,
                ..UiReply::no_change(error.to_string())
            },
            false,
        ),
        Err(error) => {
            let ambiguous = error
                .downcast_ref::<crate::remote_host::RemoteHostError>()
                .is_some_and(|error| {
                    matches!(error, crate::remote_host::RemoteHostError::Ambiguous { .. })
                });
            // A lost reply after sending is not a failure: the write may
            // have happened. The TUI keeps showing it as unconfirmed until
            // a read settles it or its deadline passes; nothing is resent.
            let reply = if ambiguous {
                UiReply::uncertain(format!(
                    "{error:#}; the result is unknown and will not be resent; check before retrying"
                ))
            } else {
                UiReply::failure(format!("{error:#}"))
            };
            (reply, ambiguous)
        }
    };
    if reply.outcome == Some(wt_tui::ReplyOutcome::Uncertain) {
        log_action_outcome(context, &reply, true);
    } else if reply.failed {
        log_action_outcome(context, &reply, false);
    }
    if reply.failed
        && !ambiguous
        && reply.modal.is_none()
        && let Some((host, input)) = retry
    {
        reply.modal_host = host;
        reply.modal = Some(wt_tui::UiModal::Text {
            action: wt_tui::TextAction::Create,
            prompt: "New worktree name: ".into(),
            initial: input,
            allow_empty: false,
        });
    }
    reply
}

fn log_action_outcome(context: Option<&ActionLogContext>, reply: &UiReply, uncertain: bool) {
    let event_text = context.map_or_else(
        || reply.message.clone(),
        |context| {
            let target = context
                .target
                .as_deref()
                .map(|target| format!(" for {target}"))
                .unwrap_or_default();
            wt_core::sanitize_terminal_text(&format!(
                "{} on {}{}: {}",
                context.operation, context.host, target, reply.message
            ))
        },
    );
    if let Some(context) = context {
        if uncertain {
            tracing::warn!(
                target: "wt_attention",
                event_channel = "attention",
                event_source = "wt",
                event_text = %event_text,
                operation_id = context.operation_id,
                operation = context.operation,
                host = %context.host,
                worktree = context.target.as_deref().unwrap_or("-"),
                "action outcome unknown"
            );
        } else {
            tracing::error!(
                target: "wt_attention",
                event_channel = "attention",
                event_source = "wt",
                event_text = %event_text,
                operation_id = context.operation_id,
                operation = context.operation,
                host = %context.host,
                worktree = context.target.as_deref().unwrap_or("-"),
                "action failed"
            );
        }
    } else if uncertain {
        tracing::warn!(
            target: "wt_attention",
            event_channel = "attention",
            event_source = "wt",
            event_text = %event_text,
            "action outcome unknown"
        );
    } else {
        tracing::error!(
            target: "wt_attention",
            event_channel = "attention",
            event_source = "wt",
            event_text = %event_text,
            "action failed"
        );
    }
}

fn sanitize_reply(mut reply: UiReply) -> UiReply {
    reply.message = wt_core::sanitize_terminal_text(&reply.message);
    if let Some(modal) = &mut reply.modal {
        let clean = wt_core::sanitize_terminal_text;
        match modal {
            wt_tui::UiModal::Reviewers { candidates, .. } => {
                for option in candidates {
                    option.label = wt_core::sanitize_terminal_text(&option.label);
                }
            }
            wt_tui::UiModal::Confirm { title, lines, .. }
            | wt_tui::UiModal::Log { title, lines, .. } => {
                *title = clean(title);
                for line in lines {
                    *line = clean(line);
                }
            }
            wt_tui::UiModal::Picker { title, options, .. } => {
                *title = clean(title);
                for option in options {
                    option.label = clean(&option.label);
                }
            }
            wt_tui::UiModal::Text {
                prompt, initial, ..
            } => {
                *prompt = clean(prompt);
                *initial = clean(initial);
            }
        }
    }
    reply
}

/// Intermediate replies retain the input intent which requested them. Progress
/// messages are not operation completions; only handoff tickets carry the ID.
pub(crate) struct RequestReplies {
    sender: tokio::sync::mpsc::Sender<UiReply>,
    id: u64,
    generation: u64,
}

impl RequestReplies {
    /// A creation cannot enter inventory until its optimistic row has the
    /// exact key. No acknowledgement means no checkout mutation is sent.
    pub(crate) async fn prepare_creation(
        &self,
        key: String,
        shutdown: &CancellationToken,
    ) -> Result<bool> {
        let (ready, received) = tokio::sync::oneshot::channel();
        let reply = UiReply {
            creation_prepared: Some(wt_tui::CreationPrepared { key, ready }),
            ..Default::default()
        };
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(false),
            result = self.send(reply) => result?,
        }
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => Ok(false),
            result = received => Ok(result.is_ok()),
        }
    }

    pub(crate) async fn send(&self, mut reply: UiReply) -> Result<()> {
        reply.ui_generation = Some(self.generation);
        if reply.handoff.is_some() || reply.creation_prepared.is_some() {
            reply.operation_id = Some(self.id);
        }
        self.sender
            .send(reply)
            .await
            .map_err(|_| anyhow::anyhow!("terminal reply channel closed"))
    }
}

pub(crate) async fn handoff_prepared(
    prepared: crate::harness::PreparedSession,
    replies: &RequestReplies,
    shutdown: &CancellationToken,
) -> Result<UiReply> {
    let (ready, suspended) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => return Ok(UiReply::no_change("")),
        result = replies.send(UiReply {
            handoff: Some(wt_tui::TerminalHandoff { ready, resumed }),
            ..Default::default()
        }) => {
            if result.is_err() {
                if shutdown.is_cancelled() {
                    return Ok(UiReply::no_change(""));
                }
                bail!("terminal closed before session attach");
            }
        }
    }
    tokio::select! {
        _ = shutdown.cancelled() => return Ok(UiReply::no_change("")),
        result = suspended => if result.is_err() {
            // A newer UI intent can decline this ticket before suspension.
            // No attach client was spawned; the prepared session stays alive.
            tracing::info!(operation_id = replies.id, ui_generation = replies.generation,
                "terminal handoff declined by newer input before attach");
            return Ok(UiReply::no_change(""));
        },
    }
    let result: Result<()> = async {
        let mut child = tokio::process::Command::new(prepared.program)
            .args(prepared.args)
            .current_dir(prepared.cwd)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("attach tmux client")?;
        let mut stderr_tail = Vec::new();
        let status = {
            let stderr = child.stderr.take().context("attach stderr pipe")?;
            let capture = forward_attach_stderr(stderr, std::io::stderr(), &mut stderr_tail);
            tokio::pin!(capture);
            let mut drained = false;
            let status = loop {
                tokio::select! {
                    _ = shutdown.cancelled() => { child.kill().await?; return Ok(()); },
                    status = child.wait() => break status?,
                    _ = &mut capture, if !drained => drained = true,
                }
            };
            // A descendant may inherit the pipe. It must not keep the board
            // suspended after the attach client has already exited.
            if !drained {
                let _ = tokio::time::timeout(Duration::from_millis(250), capture).await;
            }
            status
        };
        // The previous wt tmux bindings detach with 110/111/112 for the
        // shell/diff/harness keys. Existing agent servers retain those bindings
        // across application upgrades; returning to the board is not a failure.
        if !status.success() && !matches!(status.code(), Some(110..=112)) {
            let detail = wt_core::sanitize_terminal_text(&String::from_utf8_lossy(&stderr_tail));
            let detail = detail.trim();
            if detail.is_empty() {
                bail!("tmux client exited with {status}");
            }
            bail!("tmux client exited with {status}: {detail}");
        }
        Ok(())
    }
    .await;
    let _ = resume.send(
        result
            .as_ref()
            .map(|_| ())
            .map_err(|error| format!("{error:#}")),
    );
    result?;
    Ok(UiReply::message("Returned from session"))
}

/// Keep stderr visible during the handoff and retain a bounded diagnostic for
/// the board, which otherwise replaces that output as soon as attach fails.
async fn forward_attach_stderr(
    mut input: impl tokio::io::AsyncRead + Unpin,
    mut terminal: impl std::io::Write,
    tail: &mut Vec<u8>,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;
    const LIMIT: usize = 8 * 1024;
    let mut chunk = [0; 4096];
    loop {
        let count = input.read(&mut chunk).await?;
        if count == 0 {
            return Ok(());
        }
        tail.extend_from_slice(&chunk[..count]);
        if tail.len() > LIMIT {
            tail.drain(..tail.len() - LIMIT);
        }
        // A failed terminal write must not stop draining the child's pipe.
        let _ = terminal.write_all(&chunk[..count]);
        let _ = terminal.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_preparations_complete_as_no_change_without_failure() {
        let shutdown = CancellationToken::new();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = run_preparation_or_cancel(
            &shutdown,
            &cancellation,
            std::future::pending::<Result<UiReply>>(),
        )
        .await
        .unwrap();
        assert!(!result.failed);
        assert_eq!(result.outcome, Some(wt_tui::ReplyOutcome::NoChange));
        let reply = action_reply(Ok(result), None);
        assert!(!reply.failed);
        assert_eq!(reply.outcome, Some(wt_tui::ReplyOutcome::NoChange));
    }

    #[test]
    fn failed_lane_blocks_every_conflicting_lane_after_worker_panic() {
        let host = Lane::Host(Some("builder".into()));
        let failed = BTreeSet::from([host.clone()]);
        assert!(lane_failed(
            &Lane::Creation(Some("builder".into())),
            &failed
        ));
        assert!(lane_failed(
            &Lane::Removal(Some("builder".into()), "row".into()),
            &failed
        ));
        assert!(!lane_failed(&Lane::Host(Some("other".into())), &failed));

        let removal = Lane::Removal(Some("builder".into()), "row".into());
        let failed = BTreeSet::from([removal.clone()]);
        assert!(lane_failed(&removal, &failed));
        assert!(lane_failed(&Lane::Host(Some("builder".into())), &failed));
        assert!(!lane_failed(
            &Lane::Removal(Some("builder".into()), "other".into()),
            &failed
        ));
    }

    #[test]
    fn mutation_worker_panic_is_uncertain_but_preparation_panic_is_failure() {
        let mut active = RunningAction {
            lane: Lane::Removal(None, "row".into()),
            request_id: 7,
            generation: 3,
            name: "remove worktree",
            target: Some("row".into()),
            started: Instant::now(),
            preparation: false,
        };
        let reply = worker_panic_reply(&active, "panic");
        assert!(!reply.failed);
        assert_eq!(reply.outcome, Some(wt_tui::ReplyOutcome::Uncertain));

        active.preparation = true;
        let reply = worker_panic_reply(&active, "panic");
        assert!(reply.failed);
        assert!(reply.outcome.is_none());
    }

    #[test]
    fn shutdown_reports_background_acceptance_without_calling_it_finished() {
        let ongoing = UiReply {
            message: "Removal job 12-a continues in the background; use `wt logs` to follow it"
                .into(),
            outcome: Some(wt_tui::ReplyOutcome::Accepted(wt_runtime::Stamp::now())),
            ..Default::default()
        };
        assert_eq!(unobserved_reply_status(&ongoing), Some("was accepted"));
        assert_eq!(
            unobserved_reply_status(&UiReply::no_change("Preparation cancelled")),
            None
        );
    }

    #[tokio::test]
    async fn creation_waits_for_exact_identity_presentation_before_permission_to_mutate() {
        for acknowledge in [false, true] {
            let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
            let replies = RequestReplies {
                sender,
                id: 71,
                generation: 8,
            };
            let shutdown = CancellationToken::new();
            let worker = tokio::spawn(async move {
                replies
                    .prepare_creation("canonical-key".into(), &shutdown)
                    .await
                    .unwrap()
            });
            let mut reply = receiver.recv().await.unwrap();
            assert_eq!(reply.operation_id, Some(71));
            assert_eq!(reply.ui_generation, Some(8));
            let ticket = reply.creation_prepared.take().unwrap();
            assert_eq!(ticket.key, "canonical-key");
            assert!(
                !worker.is_finished(),
                "host mutation escaped before presentation"
            );
            if acknowledge {
                ticket.ready.send(()).unwrap();
            } else {
                drop(ticket);
            }
            assert_eq!(worker.await.unwrap(), acknowledge);
        }
    }

    #[tokio::test]
    async fn shutdown_cancels_creation_waiting_for_identity_acknowledgement() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let replies = RequestReplies {
            sender,
            id: 72,
            generation: 8,
        };
        let shutdown = CancellationToken::new();
        let cancel = shutdown.clone();
        let worker = tokio::spawn(async move {
            replies
                .prepare_creation("canonical-key".into(), &cancel)
                .await
                .unwrap()
        });
        let ticket = receiver.recv().await.unwrap();
        shutdown.cancel();
        assert!(!worker.await.unwrap());
        drop(ticket);
    }
    use wt_tui::UiAction;

    #[tokio::test]
    async fn attach_stderr_is_forwarded_in_full_but_only_keeps_a_bounded_tail() {
        let mut output = Vec::new();
        let mut tail = Vec::new();
        let mut diagnostic = vec![b'x'; 32 * 1024];
        diagnostic.extend_from_slice(b"\nno such session: missing\n");
        forward_attach_stderr(diagnostic.as_slice(), &mut output, &mut tail)
            .await
            .unwrap();
        assert_eq!(output, diagnostic);
        assert_eq!(tail, diagnostic[diagnostic.len() - 8 * 1024..]);
    }

    #[tokio::test]
    async fn declined_stale_handoff_releases_controller_without_starting_client() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("attached");
        let marker_arg = marker.to_string_lossy().into_owned();
        let (replies, mut received) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(async move {
            handoff_prepared(
                crate::harness::PreparedSession {
                    program: "sh".into(),
                    args: vec![
                        "-c".into(),
                        "touch \"$1\"".into(),
                        "attach-test".into(),
                        marker_arg.into(),
                    ],
                    cwd: std::env::temp_dir(),
                },
                &RequestReplies {
                    sender: replies,
                    id: 42,
                    generation: 7,
                },
                &CancellationToken::new(),
            )
            .await
        });
        let reply = received.recv().await.unwrap();
        assert_eq!(reply.operation_id, Some(42));
        assert_eq!(reply.ui_generation, Some(7));
        drop(reply);
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!result.failed);
        assert_eq!(result.outcome, Some(wt_tui::ReplyOutcome::NoChange));
        assert!(result.message.is_empty());
        assert!(
            !marker.exists(),
            "declined handoff must not start an attach client"
        );
    }

    #[tokio::test]
    async fn failed_attach_retains_the_cause_after_resuming_the_board() {
        let (replies, mut received) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(async move {
            handoff_prepared(
                crate::harness::PreparedSession {
                    program: "sh".into(),
                    args: vec![
                        "-c".into(),
                        "printf '\\033[31mno such session: missing\\033[0m\\n' >&2; exit 1".into(),
                    ],
                    cwd: std::env::temp_dir(),
                },
                &RequestReplies {
                    sender: replies,
                    id: 1,
                    generation: 7,
                },
                &CancellationToken::new(),
            )
            .await
        });
        let handoff = received.recv().await.unwrap().handoff.unwrap();
        handoff.ready.send(()).unwrap();
        let resumed = handoff.resumed.await.unwrap().unwrap_err();
        assert!(resumed.contains("no such session: missing"), "{resumed}");
        assert!(!resumed.contains('\x1b'));
        let error = task.await.unwrap().unwrap_err().to_string();
        assert_eq!(resumed, error);
    }

    #[tokio::test]
    async fn shutdown_during_handoff_is_a_no_change_not_an_attach_failure() {
        let (replies, mut received) = tokio::sync::mpsc::channel(1);
        let shutdown = CancellationToken::new();
        let worker_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            handoff_prepared(
                crate::harness::PreparedSession {
                    program: "sh".into(),
                    args: vec!["-c".into(), "exit 9".into()],
                    cwd: std::env::temp_dir(),
                },
                &RequestReplies {
                    sender: replies,
                    id: 3,
                    generation: 4,
                },
                &worker_shutdown,
            )
            .await
        });
        let ticket = received.recv().await.unwrap().handoff.unwrap();
        shutdown.cancel();
        let reply = task.await.unwrap().unwrap();
        assert_eq!(reply.outcome, Some(wt_tui::ReplyOutcome::NoChange));
        drop(ticket);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_keeps_accepted_work_alive_past_the_notice_deadline() {
        let cancel = CancellationToken::new();
        let (finish, work) = tokio::sync::oneshot::channel();
        let (finish_automation, automation) = tokio::sync::oneshot::channel();
        let controller = Controller {
            task: tokio::spawn(async move {
                work.await.unwrap();
            }),
            cancel: cancel.clone(),
            automations: crate::automation_source::AutomationDrain::new(
                CancellationToken::new(),
                tokio::spawn(async move {
                    automation.await.unwrap();
                }),
            ),
        };
        let shutdown = tokio::spawn(controller.shutdown());
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::task::yield_now().await;
        assert!(!cancel.is_cancelled());
        assert!(!shutdown.is_finished());
        finish.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(
            !cancel.is_cancelled(),
            "accepted automation still owns its command token"
        );
        assert!(!shutdown.is_finished());
        finish_automation.send(()).unwrap();
        shutdown.await.unwrap().unwrap();
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn remote_create_failure_restores_input_but_ambiguous_outcome_does_not() {
        let retry = Some((Some("builder".into()), "keep my title".into()));
        let failed = UiReply {
            failed: true,
            message: "branch already exists".into(),
            ..Default::default()
        };
        let reply = action_reply(Ok(failed), retry.clone());
        assert_eq!(reply.modal_host.as_deref(), Some("builder"));
        assert!(
            matches!(reply.modal, Some(wt_tui::UiModal::Text { initial, .. }) if initial == "keep my title")
        );
        let ambiguous = crate::remote_host::RemoteHostError::Ambiguous {
            host: "builder".into(),
            detail: "SSH stream closed".into(),
        };
        let reply = action_reply(Err(ambiguous.into()), retry);
        assert!(!reply.failed);
        assert_eq!(reply.outcome, Some(wt_tui::ReplyOutcome::Uncertain));
        assert!(reply.message.contains("ambiguous"));
        assert!(reply.modal.is_none());
    }

    #[test]
    fn a_notice_is_a_plain_toast_not_a_failure() {
        let reply = action_reply(Err(notice("already at top")), None);
        assert!(!reply.failed);
        assert!(reply.notice);
        assert_eq!(reply.message, "already at top");
    }

    #[test]
    fn failures_and_ambiguity_are_logged_once_as_attention_but_notice_is_not() {
        use std::sync::Arc;

        #[derive(Clone)]
        struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for LogWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let output = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_ansi(false)
            .with_writer({
                let output = output.clone();
                move || LogWriter(output.clone())
            })
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        // These callsites may have been evaluated earlier by parallel tests
        // with no thread-local subscriber installed.
        tracing::callsite::rebuild_interest_cache();

        action_reply(Ok(UiReply::failure("GitHub rejected the update")), None);
        action_reply_with_context(
            Err(anyhow::anyhow!("remote connection failed")),
            None,
            Some(&ActionLogContext {
                operation_id: 7,
                operation: "remove worktree",
                host: "MaxBook".into(),
                target: Some("new-frontend".into()),
            }),
        );
        action_reply(
            Err(crate::remote_host::RemoteHostError::Ambiguous {
                host: "builder".into(),
                detail: "reply was lost after send".into(),
            }
            .into()),
            None,
        );
        action_reply(Err(notice("already at top")), None);
        drop(guard);
        tracing::callsite::rebuild_interest_cache();

        let output = output.lock().unwrap().clone();
        let log = String::from_utf8(output).unwrap();
        let events = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 3, "{log}");
        assert_eq!(
            events
                .iter()
                .filter(|event| event["fields"]["event_channel"] == "attention")
                .count(),
            3,
            "{log}"
        );
        assert!(
            events
                .iter()
                .all(|event| event["fields"]["event_source"] == "wt"),
            "{log}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["fields"]["event_text"] == "GitHub rejected the update")
                .count(),
            1,
            "{log}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["fields"]["event_text"]
                    .as_str()
                    .is_some_and(|text| text.contains("remote connection failed")))
                .count(),
            1,
            "{log}"
        );
        let contextual = events
            .iter()
            .find(|event| event["fields"]["operation_id"] == 7)
            .expect("contextual failure event");
        assert_eq!(contextual["fields"]["operation"], "remove worktree");
        assert_eq!(contextual["fields"]["host"], "MaxBook");
        assert_eq!(contextual["fields"]["worktree"], "new-frontend");
        assert!(
            contextual["fields"]["event_text"]
                .as_str()
                .unwrap()
                .contains("remove worktree on MaxBook for new-frontend")
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["fields"]["event_text"]
                    .as_str()
                    .is_some_and(|text| text.contains("result is unknown")))
                .count(),
            1,
            "{log}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event["level"] == "WARN")
                .count(),
            1,
            "{log}"
        );
        assert!(!log.contains("already at top"), "{log}");
    }

    #[test]
    fn metadata_has_reserved_capacity_during_unrelated_removals() {
        // Exercise routing as well as admission, including managed hosts and
        // the palette alias, so neither entry point regresses to a host lane.
        for host in [None, Some("MaxBook".to_owned())] {
            let route = |action| match &host {
                Some(host) => UiAction::OnHost {
                    host: Some(host.clone()),
                    action: Box::new(action),
                },
                None => action,
            };
            let actions = [
                UiAction::SetTitle {
                    key: "selected".into(),
                    title: "new title".into(),
                },
                UiAction::GenerateTitle {
                    key: "selected".into(),
                },
                UiAction::SetIssueOverride {
                    key: "selected".into(),
                    issue_id: None,
                },
                UiAction::SetStatus {
                    key: "selected".into(),
                    state: None,
                    note: None,
                    verify_after_merge: None,
                },
                UiAction::PrepareAction {
                    surface: wt_tui::ActionSurface::Row {
                        key: "selected".into(),
                    },
                    id: crate::action_builtins::RENAME_ID.into(),
                    arg: None,
                },
            ];
            let busy = (0..4)
                .map(|i| Lane::Removal(host.clone(), format!("other-{i}")))
                .collect();
            for action in actions {
                let action = route(action);
                let lane = lane_for(&action).unwrap();
                assert_eq!(lane, Lane::Metadata(host.clone(), "selected".into()));
                let mut pending = VecDeque::from([(lane.clone(), action.clone())]);
                assert_eq!(
                    take_available(&mut pending, &busy),
                    Some((lane.clone(), action))
                );
                assert!(lanes_conflict(
                    &lane,
                    &Lane::Removal(host.clone(), "selected".into())
                ));
                assert!(lanes_conflict(&lane, &lane));
                assert!(lanes_conflict(&lane, &Lane::Host(host.clone())));
                assert!(lanes_conflict(&lane, &Lane::FleetCleanup));
                assert!(!lanes_conflict(&lane, &Lane::Creation(host.clone())));
            }
        }
    }

    #[test]
    fn metadata_admission_is_bounded_without_consuming_ordinary_capacity() {
        let active = Lane::Metadata(None, "first".into());
        let next = Lane::Metadata(None, "second".into());
        let removal = Lane::Removal(None, "third".into());
        let mut pending = VecDeque::from([(next.clone(), 1), (removal.clone(), 2)]);
        let busy = BTreeSet::from([active]);
        assert_eq!(take_available(&mut pending, &busy), Some((removal, 2)));
        assert_eq!(take_available(&mut pending, &busy), None);
        assert_eq!(
            take_available(&mut pending, &BTreeSet::new()),
            Some((next, 1))
        );
    }

    #[test]
    fn busy_host_preserves_its_order_without_blocking_other_hosts() {
        let remote = Lane::Host(Some("builder".into()));
        let local = Lane::Host(None);
        let title = |value: &str| UiAction::SetTitle {
            key: "same".into(),
            title: value.into(),
        };
        let mut pending = VecDeque::from([
            (remote.clone(), title("first")),
            (remote.clone(), title("second")),
            (local.clone(), title("local")),
        ]);
        let busy = BTreeSet::from([remote.clone()]);
        assert_eq!(
            take_available(&mut pending, &busy),
            Some((local, title("local")))
        );
        assert_eq!(take_available(&mut pending, &busy), None);
        assert_eq!(
            take_available(&mut pending, &BTreeSet::new()),
            Some((remote.clone(), title("first")))
        );
        assert_eq!(
            take_available(&mut pending, &BTreeSet::new()),
            Some((remote, title("second")))
        );
    }

    #[test]
    fn read_only_preparations_run_in_a_lane_separate_from_host_mutations() {
        let preparation = UiAction::PrepareRemove { key: "row".into() };
        let mutation = UiAction::Remove {
            key: "row".into(),
            force: false,
            revision: wt_tui::RemovalRevision {
                key: "row".into(),
                path: "/work/row".into(),
                branch: "row".into(),
                head: "head".into(),
                digest: "digest".into(),
                hazards: vec![],
                published_base: None,
            },
        };
        let preparation_lane = lane_for(&preparation).unwrap();
        let mutation_lane = lane_for(&mutation).unwrap();
        assert_eq!(preparation_lane, Lane::Preparation(None));
        assert_eq!(mutation_lane, Lane::Removal(None, "row".into()));
        assert!(!lanes_conflict(&preparation_lane, &mutation_lane));

        let mut pending = VecDeque::from([
            (preparation_lane.clone(), preparation.clone()),
            (mutation_lane.clone(), mutation),
        ]);
        let busy = BTreeSet::from([Lane::Host(None)]);
        assert_eq!(
            take_available(&mut pending, &busy),
            Some((preparation_lane, preparation))
        );
    }

    #[test]
    fn cleanup_preparation_does_not_claim_the_mutation_lane() {
        assert_eq!(
            lane_for(&UiAction::PrepareCleanup).unwrap(),
            Lane::Preparation(None)
        );
        assert_eq!(
            lane_for(&UiAction::Cleanup { revisions: vec![] }).unwrap(),
            Lane::FleetCleanup
        );
    }

    #[test]
    fn durable_removal_only_blocks_same_target_and_global_host_operations() {
        let active = Lane::Removal(None, "old-worktree".into());
        let other_remove = Lane::Removal(None, "other-worktree".into());
        let create = lane_for(&UiAction::Create {
            input: "new".into(),
        })
        .unwrap();
        assert_eq!(create, Lane::Creation(None));
        assert!(!lanes_conflict(&active, &other_remove));
        assert!(!lanes_conflict(&active, &create));
        assert!(lanes_conflict(&active, &Lane::Host(None)));
        assert!(lanes_conflict(&active, &Lane::FleetCleanup));

        let mut pending = VecDeque::from([
            (other_remove.clone(), "other removal"),
            (create.clone(), "new creation"),
            (
                Lane::Removal(None, "old-worktree".into()),
                "same-target removal",
            ),
            (Lane::Host(None), "global host write"),
        ]);
        let busy = BTreeSet::from([active]);
        assert_eq!(
            take_available(&mut pending, &busy),
            Some((other_remove, "other removal"))
        );
        assert_eq!(
            take_available(&mut pending, &busy),
            Some((create, "new creation"))
        );
        assert_eq!(take_available(&mut pending, &busy), None);
    }

    #[test]
    fn operation_log_targets_are_safe_slugs_not_qualified_keys_or_user_input() {
        let key = wt_core::remote_worktree_ledger_key("MaxBook", "new-frontend");
        assert_eq!(
            action_target(&UiAction::Remove {
                key,
                force: true,
                revision: wt_tui::RemovalRevision {
                    key: "new-frontend".into(),
                    path: "/work/new-frontend".into(),
                    branch: "new-frontend".into(),
                    head: "head".into(),
                    digest: "digest".into(),
                    hazards: vec![],
                    published_base: None,
                },
            }),
            Some("new-frontend".into())
        );
        assert_eq!(
            action_target(&UiAction::Create {
                input: "potentially sensitive title".into(),
            }),
            None
        );
    }

    #[test]
    fn shutdown_progress_identifies_the_accepted_operation_and_host() {
        let started = Instant::now() - Duration::from_secs(75);
        let progress = shutdown_progress(
            &RunningAction {
                lane: Lane::Host(Some("MaxBook".into())),
                request_id: 42,
                generation: 0,
                preparation: false,
                name: "remove worktree",
                target: Some("new-frontend".into()),
                started,
            },
            Instant::now(),
        );
        assert!(progress.contains("remove worktree for new-frontend on MaxBook is running"));
        assert!(progress.contains("1m15s elapsed"));
        assert!(progress.contains("operation 42"));
    }

    #[test]
    fn view_controls_have_reserved_capacity_while_host_work_is_saturated() {
        let busy = BTreeSet::from([
            Lane::Host(None),
            Lane::Host(Some("builder-a".into())),
            Lane::Host(Some("builder-b".into())),
            Lane::Presentation,
        ]);
        let control = UiAction::SetAttentionSeen { at_ms: 42 };
        let control_lane = lane_for(&control).unwrap();
        assert_eq!(control_lane, Lane::ViewControl);
        let mut pending = VecDeque::from([
            (
                Lane::Terminal(None, Some("existing".into())),
                UiAction::EnterHarness {
                    key: "existing".into(),
                    harness: wt_core::HarnessId::Codex,
                },
            ),
            (control_lane.clone(), control.clone()),
        ]);
        assert_eq!(
            take_available(&mut pending, &busy),
            Some((
                Lane::Terminal(None, Some("existing".into())),
                UiAction::EnterHarness {
                    key: "existing".into(),
                    harness: wt_core::HarnessId::Codex
                }
            ))
        );
        assert_eq!(
            take_available(&mut pending, &busy),
            Some((control_lane, control))
        );
        assert_eq!(take_available(&mut pending, &busy), None);
    }

    #[test]
    fn special_terminal_entry_only_waits_for_another_terminal_entry() {
        let terminal = UiAction::Session {
            key: None,
            target: wt_tui::SessionTarget::WtSource,
        };
        assert_eq!(lane_for(&terminal).unwrap(), Lane::Terminal(None, None));
        let mut pending = VecDeque::from([(Lane::Terminal(None, None), terminal.clone())]);
        assert_eq!(
            take_available(
                &mut pending,
                &BTreeSet::from([
                    Lane::Terminal(Some("other-host".into()), Some("one".into())),
                    Lane::Creation(None)
                ])
            ),
            None
        );
        assert_eq!(
            take_available(
                &mut pending,
                &BTreeSet::from([
                    Lane::Creation(None),
                    Lane::Removal(None, "other".into()),
                    Lane::FleetCleanup
                ])
            ),
            Some((Lane::Terminal(None, None), terminal))
        );
    }

    #[test]
    fn terminal_checkout_conflicts_only_with_its_own_removal_or_fleet_cleanup() {
        let host = "builder".to_owned();
        let terminal = lane_for(&UiAction::Session {
            key: Some(wt_core::remote_worktree_ledger_key(&host, "one")),
            target: wt_tui::SessionTarget::Harness,
        })
        .unwrap();
        assert_eq!(
            terminal,
            Lane::Terminal(Some(host.clone()), Some("one".into()))
        );
        for (other, conflict) in [
            (Lane::Removal(Some(host.clone()), "one".into()), true),
            (Lane::Removal(None, "one".into()), false),
            (Lane::Removal(Some(host.clone()), "two".into()), false),
            (Lane::Host(Some(host.clone())), false),
            (Lane::Creation(Some(host.clone())), false),
            (Lane::FleetCleanup, true),
        ] {
            assert_eq!(lanes_conflict(&terminal, &other), conflict, "{other:?}");
            assert_eq!(
                lanes_conflict(&other, &terminal),
                conflict,
                "symmetric {other:?}"
            );
        }
        let explicit = UiAction::OnHost {
            host: Some(host.clone()),
            action: Box::new(UiAction::EnterHarness {
                key: "one".into(),
                harness: wt_core::HarnessId::Codex,
            }),
        };
        assert_eq!(lane_for(&explicit).unwrap(), terminal);
    }

    #[test]
    fn preparation_has_one_reserved_slot_beyond_four_ordinary_actions() {
        let busy = BTreeSet::from([
            Lane::Host(None),
            Lane::Host(Some("builder-a".into())),
            Lane::Host(Some("builder-b".into())),
            Lane::Presentation,
        ]);
        let preparation = UiAction::PrepareRemove { key: "row".into() };
        let preparation_lane = lane_for(&preparation).unwrap();
        let fifth_host_lane = Lane::Host(Some("builder-c".into()));
        // Use an unrelated remote host so the test isolates the reserved
        // creation slot from same-host serialization with Host actions.
        let creation_lane = Lane::Creation(Some("builder-c".into()));
        let mut pending = VecDeque::from([
            (fifth_host_lane, UiAction::HardRefresh),
            (preparation_lane.clone(), preparation.clone()),
            (
                creation_lane.clone(),
                UiAction::Create {
                    input: "new-row".into(),
                },
            ),
        ]);
        assert_eq!(
            take_available(&mut pending, &busy),
            Some((preparation_lane.clone(), preparation))
        );
        assert_eq!(
            take_available(&mut pending, &busy),
            Some((
                creation_lane.clone(),
                UiAction::Create {
                    input: "new-row".into()
                }
            ))
        );
        let with_reserved = busy
            .into_iter()
            .chain([preparation_lane, creation_lane])
            .collect::<BTreeSet<_>>();
        assert_eq!(take_available(&mut pending, &with_reserved), None);
    }

    #[test]
    fn view_controls_still_serialize_and_do_not_consume_host_capacity() {
        let busy = BTreeSet::from([
            Lane::ViewControl,
            Lane::Host(None),
            Lane::Host(Some("builder-a".into())),
            Lane::Host(Some("builder-b".into())),
        ]);
        let next_host = Lane::Host(Some("builder-c".into()));
        let mut pending = VecDeque::from([
            (Lane::ViewControl, "another view control"),
            (next_host.clone(), "host job"),
        ]);
        assert_eq!(
            take_available(&mut pending, &busy),
            Some((next_host, "host job"))
        );
        assert_eq!(take_available(&mut pending, &busy), None);
        for action in [
            UiAction::SetHistoryActive { active: true },
            UiAction::SetPerf {
                active: true,
                continuous: false,
                refresh: true,
            },
        ] {
            assert_eq!(lane_for(&action).unwrap(), Lane::ViewControl);
        }
    }

    #[test]
    fn qualified_rows_and_captured_create_modals_keep_their_host_lane() {
        let action = UiAction::SetTitle {
            key: wt_core::remote_worktree_ledger_key("builder", "same"),
            title: "title".into(),
        };
        assert_eq!(
            lane_for(&action).unwrap(),
            Lane::Metadata(Some("builder".into()), "same".into())
        );
        let action = UiAction::OnHost {
            host: Some("builder".into()),
            action: Box::new(UiAction::Create {
                input: "retry title".into(),
            }),
        };
        assert_eq!(
            create_retry(&action),
            Some((Some("builder".into()), "retry title".into()))
        );
    }

    #[test]
    fn fleet_cleanup_uses_its_own_accepted_operation_lane() {
        let action = UiAction::Cleanup {
            revisions: vec![
                wt_tui::RemovalRevision {
                    key: "local-row".into(),
                    path: "/local/row".into(),
                    branch: "local-row".into(),
                    head: "head".into(),
                    digest: "digest".into(),
                    hazards: vec![],
                    published_base: None,
                },
                wt_tui::RemovalRevision {
                    key: wt_core::remote_worktree_ledger_key("builder", "remote-row"),
                    path: "/remote/row".into(),
                    branch: "remote-row".into(),
                    head: "head".into(),
                    digest: "digest".into(),
                    hazards: vec![],
                    published_base: None,
                },
            ],
        };
        assert_eq!(lane_for(&action).unwrap(), Lane::FleetCleanup);
        assert_eq!(
            lane_for(&UiAction::CleanupTargets { targets: vec![] }).unwrap(),
            Lane::FleetCleanup
        );

        let host = Lane::Host(Some("builder".into()));
        assert!(lanes_conflict(&Lane::FleetCleanup, &host));
        assert!(lanes_conflict(&host, &Lane::FleetCleanup));
        assert!(!lanes_conflict(
            &Lane::FleetCleanup,
            &Lane::Preparation(Some("builder".into()))
        ));
        let preparation = Lane::Preparation(Some("builder".into()));
        let mut pending = VecDeque::from([(preparation.clone(), "open removal review")]);
        assert_eq!(
            take_available(&mut pending, &BTreeSet::from([Lane::FleetCleanup])),
            Some((preparation, "open removal review"))
        );
        let mut pending = VecDeque::from([(host.clone(), "host mutation")]);
        assert_eq!(
            take_available(&mut pending, &BTreeSet::from([Lane::FleetCleanup])),
            None
        );
    }
}
