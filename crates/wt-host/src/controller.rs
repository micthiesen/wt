use anyhow::{Context, Result, bail};
use std::time::{Duration, Instant};
use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap, VecDeque},
    sync::Arc,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wt_app_core::host_errors::{Notice, error_reply};
use wt_app_core::lanes::{Admission, Lane, cleanup_lane, lane_for, lanes_conflict};
use wt_runtime::TaskScope;
use wt_tui::{ActionController, UiReply};

use wt_app_core::context::AppContext;

/// Commands serialize only where their resources conflict and drain on shutdown.
/// Independent operations retain capacity while lifecycle jobs are running.
/// Services own their own cancellation and irreversible commit boundaries.
pub struct Controller {
    task: JoinHandle<()>,
    cancel: CancellationToken,
    automations: Arc<wt_sources::automation_source::AutomationDrain>,
}

struct RunningAction {
    lane: Lane,
    request_id: u64,
    generation: u64,
    name: Cow<'static, str>,
    target: Option<String>,
    pr: Option<u64>,
    /// When the controller accepted the request. Progress counts from here.
    started: Instant,
    /// When the request left the queue and began to run.
    admitted: Instant,
    preparation: bool,
    /// Sent by the terminal on its own; see [`Queued::background`].
    background: bool,
}

/// A request waiting for its lane.
struct Queued {
    request: wt_tui::UiRequest,
    accepted_at: Instant,
    /// Sent by the terminal on its own (a prefetch, an overlay refresh).
    /// Counted apart from the user's requests so neither crowds out the other.
    background: bool,
}

/// Queue bounds. The terminal's channels add their own capacity on top.
const QUEUED_FOREGROUND: usize = 32;
const QUEUED_BACKGROUND: usize = 8;

#[derive(Clone)]
struct ActionLogContext {
    operation_id: u64,
    operation: Cow<'static, str>,
    host: String,
    target: Option<String>,
    pr: Option<u64>,
}

impl ActionLogContext {
    fn for_action(operation_id: u64, action: &wt_tui::UiAction, lane: &Lane) -> Self {
        Self {
            operation_id,
            operation: action_name(action),
            host: lane_host(lane).to_owned(),
            target: action_target(action),
            pr: action_pr(action),
        }
    }
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
        let mut pending = VecDeque::<(Lane, Queued)>::new();
        let mut busy = Admission::default();
        let mut running = tokio::task::JoinSet::new();
        let mut task_lanes = HashMap::new();
        let mut progress_tick = Box::pin(tokio::time::sleep(Duration::from_secs(3)));
        let mut failed_lanes = BTreeSet::new();
        let mut closed = false;
        let mut background_closed = false;
        let mut views_open = true;
        let mut perf_revision = port.views.borrow().perf_revision;
        let mut draining = false;
        loop {
            while let Some((lane, queued)) = take_available(&mut pending, &busy) {
                let Queued { request, accepted_at, background } = queued;
                let wt_tui::UiRequest { id, generation, action: command, cancellation } = request;
                let hold = busy.insert(lane.clone());
                // A remote host's worker schedules the planning of its own
                // removals; here the request only waits for that worker.
                if matches!(lane, Lane::Removal(Some(_), _)) {
                    hold.durable();
                }
                let operation_cancellation = command
                    .is_cancellable()
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
                let is_preparation = command.is_preparation();
                let log_context = ActionLogContext::for_action(id, &command, &task_lane);
                let (operation_name, operation_target, operation_pr) =
                    (log_context.operation.clone(), log_context.target.clone(), log_context.pr);
                let task = running.spawn(async move {
                    let retry = create_retry(&command);
                    let request_replies = RequestReplies { sender: replies, id, generation };
                    let execution = crate::host_dispatch::execute(
                        &context,
                        &fleet,
                        command,
                        &request_replies,
                        &shutdown,
                        &hold,
                    );
                    let (result, cancelled) = if let Some(cancellation) = operation_cancellation {
                        run_request_or_cancel(&shutdown, &cancellation, execution).await
                    } else {
                        (execution.await, false)
                    };
                    let mut reply = action_reply_with_context(result, retry, Some(&log_context));
                    reply.ui_generation = Some(generation);
                    reply.operation_id = Some(id);
                    (lane, reply, cancelled)
                });
                task_lanes.insert(task.id(), RunningAction {
                    lane: task_lane,
                    request_id: id,
                    generation,
                    name: operation_name,
                    target: operation_target,
                    pr: operation_pr,
                    started: accepted_at,
                    admitted: Instant::now(),
                    preparation: is_preparation,
                    background,
                });
            }
            if closed && pending.is_empty() && running.is_empty() { break; }
            let queued_background = pending.iter().filter(|(_, queued)| queued.background).count();
            let queued_foreground = pending.len() - queued_background;
            tokio::select! {
                biased;
                _ = shutdown.cancelled(), if !draining => {
                    draining = true;
                    port.requests.close();
                    port.background.close();
                    progress_tick.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(3));
                    // Pending menus, session entry and background reads can
                    // stop before they start. Drop them so shutdown drains
                    // only accepted work.
                    pending.retain(|(lane, queued)| {
                        let drained = !queued.background && !queued.request.action.is_cancellable();
                        if !drained {
                            log_unstarted(lane, queued, "cancelled");
                        }
                        drained
                    });
                },
                finished = running.join_next_with_id(), if !running.is_empty() => match finished {
                    Some(Ok((id, (lane, reply, cancelled)))) => {
                        let active = task_lanes.remove(&id);
                        if let Some(active) = &active {
                            if reply.failed || reply.outcome == Some(wt_tui::ReplyOutcome::Uncertain) {
                                tracing::warn!(
                                    operation_id = active.request_id,
                                    operation = %active.name,
                                    host = lane_host(&active.lane),
                                    target = active.target.as_deref().unwrap_or("-"),
                                    pr = active.pr,
                                    uncertain = reply.outcome == Some(wt_tui::ReplyOutcome::Uncertain),
                                    "action worker returned a failed or unconfirmed outcome"
                                );
                            }
                            log_request(
                                active.request_id,
                                &active.name,
                                &active.lane,
                                active.admitted.saturating_duration_since(active.started),
                                active.admitted.elapsed(),
                                request_outcome(&reply, cancelled),
                                active.background,
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
                                operation: active.name.clone(),
                                host: lane_host(&active.lane).to_owned(),
                                target: active.target.clone(),
                                pr: active.pr,
                            };
                            log_action_outcome(
                                Some(&log_context),
                                &reply,
                                reply.outcome == Some(wt_tui::ReplyOutcome::Uncertain),
                            );
                            log_request(
                                active.request_id,
                                &active.name,
                                &active.lane,
                                active.admitted.saturating_duration_since(active.started),
                                active.admitted.elapsed(),
                                request_outcome(&reply, false),
                                active.background,
                            );
                            if let Err(error) = port.replies.send(reply).await {
                                report_unobserved_reply(&active, &error.0);
                            }
                            failed_lanes.insert(active.lane.clone());
                            busy.remove(&active.lane);
                            let skipped = pending.iter().filter(|(queued, _)| lanes_conflict(queued, &active.lane)).count();
                            for (lane, queued) in pending.iter().filter(|(queued, _)| lanes_conflict(queued, &active.lane)) {
                                log_unstarted(lane, queued, "failed");
                                let _ = port.replies.send(UiReply { operation_id: Some(queued.request.id), ..UiReply::failure("Host action worker failed; operation did not start") }).await;
                            }
                            pending.retain(|(queued, _)| !lanes_conflict(queued, &active.lane));
                            if skipped > 0 {
                                let _ = port.replies.send(UiReply::failure(format!("{skipped} queued commands on the failed host were not started. Restart wt before retrying."))).await;
                            }
                        }
                    },
                    None => {},
                },
                // A running request gave capacity back; admit what waited on it.
                _ = busy.changed() => {},
                changed = port.views.changed(), if views_open => match changed {
                    Ok(()) => {
                        let views = *port.views.borrow_and_update();
                        crate::host_dispatch::apply_views(&fleet, views, &mut perf_revision);
                    },
                    Err(_) => views_open = false,
                },
                command = port.requests.recv(), if !closed && queued_foreground < QUEUED_FOREGROUND => match command {
                    Some(command) => {
                        if task_lanes.is_empty() && pending.is_empty() {
                            progress_tick.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(3));
                        }
                        enqueue(&mut pending, &failed_lanes, &port.replies, command, false).await;
                    },
                    None => closed = true,
                },
                command = port.background.recv(), if !closed && !background_closed && queued_background < QUEUED_BACKGROUND => match command {
                    Some(command) => enqueue(&mut pending, &failed_lanes, &port.replies, command, true).await,
                    None => background_closed = true,
                },
                _ = &mut progress_tick, if !task_lanes.is_empty() || !pending.is_empty() => {
                    for active in task_lanes.values() {
                        if draining {
                            eprintln!("{}", shutdown_progress(active, Instant::now()));
                        } else if active.request_id != 0 && active.started.elapsed() >= Duration::from_secs(3) {
                            let _ = port.replies.try_send(progress_reply(
                                active.request_id, active.generation,
                                format!("{} on {}: running ({})", active.name, lane_host(&active.lane), elapsed(active.started.elapsed())),
                            ));
                        }
                    }
                    for (index, (lane, queued)) in pending.iter().enumerate() {
                        let Queued { request, accepted_at, background } = queued;
                        if draining {
                            eprintln!("wt: shutdown waiting: {} on {} is queued ({} elapsed, operation {})",
                            action_name(&request.action),
                            lane_host(lane),
                            elapsed(accepted_at.elapsed()),
                            request.id,
                            );
                        } else if !background && accepted_at.elapsed() >= Duration::from_secs(3) {
                            let reason = waiting_reason(&busy, &task_lanes, &pending, index);
                            let _ = port.replies.try_send(progress_reply(
                                request.id, request.generation,
                                format!("{}: {reason} ({})", action_name(&request.action), elapsed(accepted_at.elapsed())),
                            ));
                        }
                    }
                    progress_tick.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(if draining { 10 } else { 3 }));
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

/// Queue a request in its lane, or answer at once why it cannot run.
async fn enqueue(
    pending: &mut VecDeque<(Lane, Queued)>,
    failed_lanes: &BTreeSet<Lane>,
    replies: &tokio::sync::mpsc::Sender<UiReply>,
    request: wt_tui::UiRequest,
    background: bool,
) {
    let refusal = match lane_for(&request.action) {
        Ok(lane) if !lane_failed(&lane, failed_lanes) => {
            // A newer identical prefetch replaces a queued one.
            if matches!(lane, Lane::Prefetch(_)) {
                pending.retain(|(queued, other)| {
                    let kept = !matches!(queued, Lane::Prefetch(_))
                        || other.request.action != request.action;
                    if !kept {
                        log_unstarted(queued, other, "replaced");
                    }
                    kept
                });
            }
            pending.push_back((
                lane,
                Queued {
                    request,
                    accepted_at: Instant::now(),
                    background,
                },
            ));
            return;
        }
        Ok(_) => {
            "Host command worker failed; this command was not started. Restart wt before retrying."
                .to_owned()
        }
        Err(error) => format!("{error:#}"),
    };
    tracing::info!(
        operation_id = request.id,
        operation = %action_name(&request.action),
        outcome = "failed",
        "controller request refused"
    );
    let _ = replies
        .send(UiReply {
            operation_id: Some(request.id),
            ..UiReply::failure(refusal)
        })
        .await;
}

/// Why a queued request has not started, for its progress notice.
fn waiting_reason(
    busy: &Admission,
    running: &HashMap<tokio::task::Id, RunningAction>,
    pending: &VecDeque<(Lane, Queued)>,
    index: usize,
) -> String {
    let lane = &pending[index].0;
    let blocker = busy
        .blocker(lane)
        .and_then(|blocking| running.values().find(|active| active.lane == *blocking));
    if let Some(active) = blocker {
        let target = active
            .target
            .as_deref()
            .map(|target| format!(" for {target}"))
            .unwrap_or_default();
        return format!(
            "waiting for {}{target} on {}",
            active.name,
            lane_host(&active.lane)
        );
    }
    match Admission::ahead(pending, index) {
        Some((earlier_lane, earlier)) => format!(
            "waiting behind queued {} on {}",
            action_name(&earlier.request.action),
            lane_host(earlier_lane)
        ),
        None => "waiting for command capacity".to_owned(),
    }
}

/// The lane's kind, without the host or checkout it names.
fn lane_kind(lane: &Lane) -> &'static str {
    match lane {
        Lane::Host(_) => "host",
        Lane::HostControl(_) => "host-control",
        Lane::Target(_, _) => "target",
        Lane::Action(_, _) => "action",
        Lane::SessionControl(_, _) => "session-control",
        Lane::Creation(_) => "creation",
        Lane::Removal(_, _) => "removal",
        Lane::Metadata(_, _) => "metadata",
        Lane::Preparation(_) => "preparation",
        Lane::Prefetch(_) => "prefetch",
        Lane::FleetCleanup(_) => "cleanup",
        Lane::Presentation => "presentation",
        Lane::Device => "device",
        Lane::ViewControl => "view-control",
        Lane::Terminal(_, _) => "terminal",
    }
}

fn request_outcome(reply: &UiReply, cancelled: bool) -> &'static str {
    if cancelled {
        "cancelled"
    } else if reply.stale {
        "stale"
    } else if reply.outcome == Some(wt_tui::ReplyOutcome::Uncertain) {
        "uncertain"
    } else if reply.failed {
        "failed"
    } else {
        "ok"
    }
}

/// A background read slower than this is logged like a user's request.
const SLOW_BACKGROUND: Duration = Duration::from_secs(1);

/// One record per request: how long it waited for its lane and how long it
/// ran. Carries names and identifiers only, never a request's content. The
/// reads the terminal sends on its own (a prefetch per rested row, an overlay
/// refresh per second) are recorded at debug unless one failed or was slow.
fn log_request(
    id: u64,
    operation: &str,
    lane: &Lane,
    queued: Duration,
    run: Duration,
    outcome: &'static str,
    background: bool,
) {
    let host = match lane {
        Lane::FleetCleanup(_) => "fleet",
        lane => match lane_host(lane) {
            LOCAL_HOST => "local",
            host => host,
        },
    };
    let lane = lane_kind(lane);
    let queued_ms = u64::try_from(queued.as_millis()).unwrap_or(u64::MAX);
    let run_ms = u64::try_from(run.as_millis()).unwrap_or(u64::MAX);
    let routine =
        background && !matches!(outcome, "failed" | "uncertain") && queued + run <= SLOW_BACKGROUND;
    if routine {
        tracing::debug!(
            operation_id = id,
            operation,
            lane,
            host,
            queued_ms,
            run_ms,
            outcome,
            "controller request"
        );
    } else {
        tracing::info!(
            operation_id = id,
            operation,
            lane,
            host,
            queued_ms,
            run_ms,
            outcome,
            "controller request"
        );
    }
}

/// A request that left the queue without running.
fn log_unstarted(lane: &Lane, queued: &Queued, outcome: &'static str) {
    log_request(
        queued.request.id,
        &action_name(&queued.request.action),
        lane,
        queued.accepted_at.elapsed(),
        Duration::ZERO,
        outcome,
        queued.background,
    );
}

fn progress_reply(id: u64, generation: u64, message: String) -> UiReply {
    UiReply {
        operation_id: Some(id),
        ui_generation: Some(generation),
        progress: Some(message),
        ..Default::default()
    }
}

/// The operation a log line or progress notice names. Exhaustive so each new
/// action declares how it is attributed instead of logging "host action".
fn action_name(action: &wt_tui::UiAction) -> Cow<'static, str> {
    use wt_tui::UiAction as A;
    Cow::Borrowed(match action {
        A::OnHost { action, .. } => return action_name(action),
        A::RunAction { id, .. } => return Cow::Owned(format!("run action {id}")),
        A::ManagedSignIn { provider, .. } => return Cow::Owned(format!("sign in to {provider}")),
        A::Create { .. } | A::CreateResolved { .. } => "create worktree",
        A::Remove { .. } => "remove worktree",
        A::DismissFailedRemoval { .. } => "dismiss failed removal",
        A::Cleanup { .. } | A::CleanupTargets { .. } => "clean worktrees",
        A::PrepareCreate { .. } => "prepare worktree creation",
        A::PrepareRemove { .. } => "prepare worktree removal",
        A::PrepareGithub { .. } => "prepare GitHub action",
        A::PrepareReviewers { .. } => "prepare reviewer picker",
        A::PrepareReviewCheckout { .. } => "prepare review checkout",
        A::PrepareRestoreRemoved { .. } => "prepare worktree restore",
        A::PrepareHardRefresh => "prepare cache refresh",
        A::PrepareActions { .. } => "prepare action palette",
        A::PrepareDevLogs { .. } => "load dev server logs",
        A::PrepareSection { .. } => "prepare section picker",
        A::PrepareDeleteSection { .. } => "prepare section delete",
        A::PrepareStatus { .. } => "prepare status picker",
        A::PrepareBase { .. } => "prepare fork base picker",
        A::PrepareNotify { .. } => "prepare notify picker",
        A::SetNotify { .. } => "save notification",
        A::PrepareSessions { .. } => "prepare session picker",
        A::DiscoverSessions { .. } => "discover sessions",
        A::PrepareHarnesses { .. } => "prepare harness picker",
        A::PrepareStopTerminal { .. } => "prepare session stop",
        A::GithubFailedChecks { .. } => "load failed checks",
        A::ResolveCreation { .. } => "resolve worktree creation",
        A::SetTitle { .. } => "save title",
        A::SetStatus { .. } => "save status",
        A::SetIssueOverride { .. } => "save issue",
        A::SetBase { .. } => "save fork base",
        A::CyclePrimary => "change default harness",
        A::ToggleAutomations { .. } | A::ToggleRemovedAutomations { .. } => {
            "change automation preference"
        }
        A::CancelAutomations => "cancel queued automations",
        A::GithubMarkReady { .. } => "mark pull request ready",
        A::GithubSetAutoMerge { .. } => "change pull request auto-merge",
        A::GithubShip { .. } => "ship pull request",
        A::SubmitReviewers { .. } => "request reviewers",
        A::DismissReviewRequest { .. } => "dismiss review request",
        A::ReviewCheckout { .. } => "check out review",
        A::RestoreRemoved { .. } => "restore worktree",
        A::Restack { .. } => "restack",
        A::KillAction { .. } => "stop action",
        A::HardRefresh => "refresh caches",
        A::SetAttentionSeen { .. } => "acknowledge attention",
        A::SetHistoryActive { .. } => "change history view",
        A::SetPerf { .. } => "change performance view",
        A::PerfInvestigate { .. } => "investigate performance",
        A::OpenLink { .. } | A::OpenPrLink { .. } | A::OpenPrDefault { .. } | A::OpenUrl { .. } => {
            "open link"
        }
        A::OpenSlotEditor { .. } | A::OpenEditor { .. } => "open editor",
        A::Copy { .. } => "copy",
        A::FoldSection { .. } => "fold section",
        A::MoveSection { .. } => "move to section",
        A::Reorder { .. } => "reorder",
        A::RenameSection { .. } => "rename section",
        A::DeleteSection { .. } => "delete section",
        A::ToggleArchive { .. } => "toggle archive",
        A::Session { .. } | A::EnterHarness { .. } | A::SelectSession { .. } => "open session",
        A::StopTerminal { .. } | A::StopSession { .. } => "stop session",
        A::KillSession { .. } => "kill session",
    })
}

/// The pull request a GitHub write was pinned to, for log attribution.
fn action_pr(action: &wt_tui::UiAction) -> Option<u64> {
    use wt_tui::UiAction as A;
    match action {
        A::OnHost { action, .. } => action_pr(action),
        A::GithubMarkReady { pin, .. }
        | A::GithubSetAutoMerge { pin, .. }
        | A::GithubShip { pin, .. } => pin.as_ref().map(|pin| pin.number),
        A::SubmitReviewers { pr_number, .. } => Some(*pr_number),
        _ => None,
    }
}

/// The worktree (or session slot) an action is about. Exhaustive so each new
/// action declares its target instead of logging "-".
fn action_target(action: &wt_tui::UiAction) -> Option<String> {
    use wt_tui::UiAction as A;
    let surface_key = |surface: &wt_tui::ActionSurface| -> Result<Option<String>, String> {
        match surface {
            wt_tui::ActionSurface::Row { key } => Ok(Some(key.clone())),
            wt_tui::ActionSurface::Manager { key: Some(key) } => Ok(Some(key.clone())),
            wt_tui::ActionSurface::Manager { key: None } => Err("manager".to_owned()),
            wt_tui::ActionSurface::Slot { target } => Err(slot_label(target).to_owned()),
        }
    };
    let key = match action {
        A::OnHost { action, .. } => return action_target(action),
        A::RunAction { surface, .. } | A::PrepareActions { surface } => {
            match surface_key(surface) {
                Ok(key) => key?,
                Err(label) => return Some(label),
            }
        }
        A::Remove { key, .. }
        | A::DismissFailedRemoval { key, .. }
        | A::PrepareRemove { key }
        | A::PrepareDevLogs { key }
        | A::SetTitle { key, .. }
        | A::SetStatus { key, .. }
        | A::PrepareStatus { key }
        | A::SetIssueOverride { key, .. }
        | A::SetBase { key, .. }
        | A::PrepareBase { key }
        | A::SetNotify { key, .. }
        | A::PrepareNotify { key }
        | A::EnterHarness { key, .. }
        | A::PrepareHarnesses { key }
        | A::StopTerminal { key, .. }
        | A::PrepareStopTerminal { key, .. }
        | A::GithubMarkReady { key, .. }
        | A::GithubSetAutoMerge { key, .. }
        | A::GithubShip { key, .. }
        | A::GithubFailedChecks { key }
        | A::PrepareGithub { key, .. }
        | A::SubmitReviewers { key, .. }
        | A::PrepareReviewers { key }
        | A::Restack { key }
        | A::RestoreRemoved { key, .. }
        | A::PrepareRestoreRemoved { key }
        | A::ToggleRemovedAutomations { key, .. }
        | A::ToggleArchive { key, .. }
        | A::MoveSection { key, .. }
        | A::OpenEditor { key }
        | A::OpenUrl { key, .. } => key.clone(),
        A::Session { key, .. }
        | A::PrepareSessions { key, .. }
        | A::DiscoverSessions { key, .. }
        | A::ToggleAutomations { key, .. } => key.clone()?,
        A::SelectSession { selection }
        | A::StopSession { selection }
        | A::KillSession { selection } => selection.key.clone()?,
        A::CreateResolved { plan } => plan.key.clone(),
        A::Cleanup { .. } | A::CleanupTargets { .. } => {
            return cleanup_target_summary(action);
        }
        A::ReviewCheckout { branch, .. } | A::PrepareReviewCheckout { branch, .. } => {
            return Some(branch.clone());
        }
        A::OpenSlotEditor { target } => return Some(slot_label(target).to_owned()),
        // Section keys, URLs, inputs and device controls name no checkout.
        A::PrepareCreate { .. }
        | A::ResolveCreation { .. }
        | A::Create { .. }
        | A::ManagedSignIn { .. }
        | A::PrepareHardRefresh
        | A::HardRefresh
        | A::SetAttentionSeen { .. }
        | A::CancelAutomations
        | A::SetHistoryActive { .. }
        | A::SetPerf { .. }
        | A::OpenLink { .. }
        | A::OpenPrLink { .. }
        | A::OpenPrDefault { .. }
        | A::PerfInvestigate { .. }
        | A::DismissReviewRequest { .. }
        | A::KillAction { .. }
        | A::FoldSection { .. }
        | A::PrepareSection { .. }
        | A::PrepareDeleteSection { .. }
        | A::Reorder { .. }
        | A::RenameSection { .. }
        | A::DeleteSection { .. }
        | A::CyclePrimary
        | A::Copy { .. } => return None,
    };
    let key = &key;
    match wt_core::parse_worktree_ledger_key(key)? {
        wt_core::WorktreeRef::Local { slug } | wt_core::WorktreeRef::Remote { slug, .. } => {
            Some(slug)
        }
    }
}

fn slot_label(target: &wt_tui::SessionTarget) -> &'static str {
    match target {
        wt_tui::SessionTarget::Harness => "harness",
        wt_tui::SessionTarget::Shell => "shell",
        wt_tui::SessionTarget::Diff => "diff",
        wt_tui::SessionTarget::Manager => "manager",
        wt_tui::SessionTarget::Main => "main",
        wt_tui::SessionTarget::WtSource => "wt source",
        wt_tui::SessionTarget::Dotfiles => "dotfiles",
    }
}

fn cleanup_target_summary(action: &wt_tui::UiAction) -> Option<String> {
    let Ok(Lane::FleetCleanup(targets)) = cleanup_lane(action, None) else {
        return None;
    };
    match targets.len() {
        0 => None,
        1..=3 => Some(
            targets
                .into_iter()
                .map(|(_, slug)| slug)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        count => Some(format!("{count} worktrees")),
    }
}

const LOCAL_HOST: &str = "local host";

fn lane_host(lane: &Lane) -> &str {
    match lane {
        Lane::Host(Some(host))
        | Lane::HostControl(Some(host))
        | Lane::Target(Some(host), _)
        | Lane::Action(Some(host), _)
        | Lane::SessionControl(Some(host), _)
        | Lane::Creation(Some(host))
        | Lane::Removal(Some(host), _)
        | Lane::Metadata(Some(host), _)
        | Lane::Preparation(Some(host))
        | Lane::Prefetch(Some(host))
        | Lane::Terminal(Some(host), _) => host,
        Lane::Host(None)
        | Lane::HostControl(None)
        | Lane::Target(None, _)
        | Lane::Action(None, _)
        | Lane::SessionControl(None, _)
        | Lane::Creation(None)
        | Lane::Removal(None, _)
        | Lane::Metadata(None, _)
        | Lane::Preparation(None)
        | Lane::Prefetch(None)
        | Lane::Terminal(None, _) => LOCAL_HOST,
        Lane::FleetCleanup(targets) => {
            let mut hosts = targets.iter().map(|(host, _)| host.as_deref());
            match hosts.next() {
                Some(first) if hosts.all(|host| host == first) => first.unwrap_or(LOCAL_HOST),
                Some(_) => "local and remote hosts",
                None => LOCAL_HOST,
            }
        }
        Lane::Presentation | Lane::Device | Lane::ViewControl => LOCAL_HOST,
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

/// The operation's result, and whether it was cancelled before it produced one.
async fn run_request_or_cancel<F>(
    shutdown: &CancellationToken,
    cancellation: &CancellationToken,
    operation: F,
) -> (Result<UiReply>, bool)
where
    F: std::future::Future<Output = Result<UiReply>>,
{
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => (Ok(UiReply::no_change("Request cancelled during shutdown")), true),
        _ = cancellation.cancelled() => (Ok(UiReply::no_change("Request cancelled")), true),
        result = operation => (result, false),
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

/// Takes the queued request that may start now, in arrival order (see
/// [`Admission`] for the rule). The caller records it with
/// [`Admission::insert`].
pub fn take_available<T>(pending: &mut VecDeque<(Lane, T)>, busy: &Admission) -> Option<(Lane, T)> {
    let position = busy.next(pending)?;
    pending.remove(position)
}

fn create_retry(
    action: &wt_tui::UiAction,
) -> Option<(Option<String>, String, Option<wt_tui::CreatePlacement>)> {
    let (host, action, _) = wt_app_core::host_routing::resolve(action.clone()).ok()?;
    match action {
        wt_tui::UiAction::Create { input, placement } => Some((host, input, placement)),
        _ => None,
    }
}

#[cfg(test)]
fn action_reply(
    result: Result<UiReply>,
    retry: Option<(Option<String>, String, Option<wt_tui::CreatePlacement>)>,
) -> UiReply {
    action_reply_with_context(result, retry, None)
}

fn action_reply_with_context(
    result: Result<UiReply>,
    retry: Option<(Option<String>, String, Option<wt_tui::CreatePlacement>)>,
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
            let ambiguous = crate::remote_host::RemoteHostError::is_ambiguous(&error);
            // A lost reply after sending is not a failure: the write may
            // have happened. The TUI keeps showing it as unconfirmed until
            // a read settles it or its deadline passes; nothing is resent.
            let reply = if ambiguous {
                UiReply::uncertain(format!(
                    "{error:#}; the result is unknown and will not be resent; check before retrying"
                ))
            } else {
                error_reply(&error)
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
        && let Some((host, input, placement)) = retry
    {
        reply.modal_host = host;
        reply.modal = Some(wt_tui::UiModal::Text {
            action: wt_tui::TextAction::Create { placement },
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
                operation = %context.operation,
                host = %context.host,
                worktree = context.target.as_deref().unwrap_or("-"),
                pr = context.pr,
                "action outcome unknown"
            );
        } else {
            tracing::error!(
                target: "wt_attention",
                event_channel = "attention",
                event_source = "wt",
                event_text = %event_text,
                operation_id = context.operation_id,
                operation = %context.operation,
                host = %context.host,
                worktree = context.target.as_deref().unwrap_or("-"),
                pr = context.pr,
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
pub struct RequestReplies {
    sender: tokio::sync::mpsc::Sender<UiReply>,
    id: u64,
    generation: u64,
}

impl RequestReplies {
    /// A creation cannot enter inventory until its optimistic row has the
    /// exact key. No acknowledgement means no checkout mutation is sent.
    pub async fn prepare_creation(
        &self,
        key: String,
        already_exists: bool,
        shutdown: &CancellationToken,
    ) -> Result<bool> {
        let (ready, received) = tokio::sync::oneshot::channel();
        let reply = UiReply {
            creation_prepared: Some(wt_tui::CreationPrepared {
                key,
                already_exists,
                ready,
            }),
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

    pub async fn send(&self, mut reply: UiReply) -> Result<()> {
        reply.ui_generation = Some(self.generation);
        if reply.handoff.is_some()
            || reply.creation_prepared.is_some()
            || reply.creation_placement.is_some()
        {
            reply.operation_id = Some(self.id);
        }
        self.sender
            .send(reply)
            .await
            .map_err(|_| anyhow::anyhow!("terminal reply channel closed"))
    }
}

pub async fn handoff_prepared(
    prepared: wt_app_core::harness::PreparedSession,
    replies: &RequestReplies,
    shutdown: &CancellationToken,
) -> Result<UiReply> {
    handoff_navigable(prepared, replies, shutdown, None).await
}

pub async fn handoff_navigable(
    mut prepared: wt_app_core::harness::PreparedSession,
    replies: &RequestReplies,
    shutdown: &CancellationToken,
    mut navigation: Option<wt_app_core::harness::SessionNavigation<'_>>,
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
        loop {
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
            // Private detach codes select another session. Callers without
            // worktree navigation return to the board for these codes.
            if !status.success() && !matches!(status.code(), Some(110..=112)) {
                let detail =
                    wt_core::sanitize_terminal_text(&String::from_utf8_lossy(&stderr_tail));
                let detail = detail.trim();
                if detail.is_empty() {
                    bail!("tmux client exited with {status}");
                }
                // tmux rejects the inherited TERM's terminfo; name it, since
                // the board's process environment is otherwise invisible.
                if detail.contains("open terminal failed") {
                    let term = std::env::var("TERM").map_or_else(
                        |_| "unset".into(),
                        |term| wt_core::sanitize_terminal_text(&term),
                    );
                    bail!("tmux client exited with {status}: {detail} (TERM={term})");
                }
                bail!("tmux client exited with {status}: {detail}");
            }
            if let Some(navigation) = &mut navigation
                && let Some(next) = navigation.next(status.code()).await?
            {
                prepared = next;
                continue;
            }
            return Ok(());
        }
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
    use wt_app_core::host_errors::notice;

    #[tokio::test]
    async fn cancelled_preparations_complete_as_no_change_without_failure() {
        let shutdown = CancellationToken::new();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = run_request_or_cancel(
            &shutdown,
            &cancellation,
            std::future::pending::<Result<UiReply>>(),
        )
        .await;
        assert!(result.1, "a cancelled request reports itself as cancelled");
        let result = result.0.unwrap();
        assert!(!result.failed);
        assert_eq!(result.outcome, Some(wt_tui::ReplyOutcome::NoChange));
        let reply = action_reply(Ok(result), None);
        assert!(!reply.failed);
        assert_eq!(reply.outcome, Some(wt_tui::ReplyOutcome::NoChange));
    }

    #[tokio::test]
    async fn cancelled_remote_entry_releases_terminal_admission_for_local_entry() {
        let remote = wt_tui::UiAction::OnHost {
            host: Some("offline".into()),
            action: Box::new(wt_tui::UiAction::Session {
                key: Some("remote-tree".into()),
                target: wt_tui::SessionTarget::Shell,
            }),
        };
        let local = wt_tui::UiAction::Session {
            key: Some("local-tree".into()),
            target: wt_tui::SessionTarget::Shell,
        };
        assert!(remote.is_cancellable());
        let remote_lane = lane_for(&remote).unwrap();
        let local_lane = lane_for(&local).unwrap();
        let mut busy = Admission::from([remote_lane.clone()]);
        let mut pending = VecDeque::from([(local_lane.clone(), local)]);
        assert!(take_available(&mut pending, &busy).is_none());

        let shutdown = CancellationToken::new();
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let worker = tokio::spawn(async move {
            run_request_or_cancel(
                &shutdown,
                &worker_cancellation,
                std::future::pending::<Result<UiReply>>(),
            )
            .await
        });
        tokio::task::yield_now().await;
        cancellation.cancel();
        let reply = tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap()
            .0
            .unwrap();
        assert!(!reply.failed);
        assert_eq!(reply.outcome, Some(wt_tui::ReplyOutcome::NoChange));

        busy.remove(&remote_lane);
        assert_eq!(take_available(&mut pending, &busy).unwrap().0, local_lane);
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
            name: "remove worktree".into(),
            target: Some("row".into()),
            pr: None,
            started: Instant::now(),
            admitted: Instant::now(),
            background: false,
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
            unobserved_reply_status(&UiReply::no_change("Request cancelled")),
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
                    .prepare_creation("canonical-key".into(), false, &shutdown)
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
                .prepare_creation("canonical-key".into(), false, &cancel)
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
                wt_app_core::harness::PreparedSession {
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
                wt_app_core::harness::PreparedSession {
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
                wt_app_core::harness::PreparedSession {
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
            automations: wt_sources::automation_source::AutomationDrain::new(
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

    /// JSON log records written on this thread while the guard lives.
    fn capture_logs() -> (
        tracing::subscriber::DefaultGuard,
        Arc<std::sync::Mutex<Vec<u8>>>,
    ) {
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

        /// Parallel tests hit these callsites with no subscriber. Reporting
        /// `sometimes` keeps any interest rebuilt while this subscriber is
        /// alive from caching `never`, so each event asks it directly.
        struct PerEventInterest;

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PerEventInterest {
            fn register_callsite(
                &self,
                _: &'static tracing::Metadata<'static>,
            ) -> tracing::subscriber::Interest {
                tracing::subscriber::Interest::sometimes()
            }
        }

        use tracing_subscriber::layer::SubscriberExt;
        let output = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_ansi(false)
            .with_writer({
                let output = output.clone();
                move || LogWriter(output.clone())
            })
            .finish()
            .with(PerEventInterest);
        (tracing::subscriber::set_default(subscriber), output)
    }

    #[tokio::test]
    async fn a_request_is_answered_and_logged_once_with_its_lane_and_timings() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let (logging, output) = capture_logs();
        let scope = TaskScope::new();
        let host = Arc::new(crate::host_service::HostService::start(
            &scope,
            fixture.ctx.clone(),
            CancellationToken::new(),
        ));
        let fleet = crate::remote_board::start(&scope, &fixture.ctx, host);
        let (mut actions, port) = wt_tui::action_channel();
        let controller = start(&scope, fixture.ctx.clone(), fleet, port);
        actions
            .requests
            .send(wt_tui::UiRequest {
                id: 7,
                generation: 3,
                action: UiAction::FoldSection {
                    key: "private-section".into(),
                    folded: true,
                },
                cancellation: None,
            })
            .await
            .unwrap();
        let reply = loop {
            let reply = tokio::time::timeout(Duration::from_secs(30), actions.replies.recv())
                .await
                .expect("the controller answers")
                .expect("the controller is running");
            if reply.operation_id == Some(7) && reply.progress.is_none() {
                break reply;
            }
        };
        assert!(matches!(
            reply.outcome,
            Some(wt_tui::ReplyOutcome::Accepted(_))
        ));
        assert_eq!(reply.ui_generation, Some(3));
        scope.cancel();
        controller.shutdown().await.unwrap();
        let _ = scope.shutdown(Duration::from_secs(5)).await;
        fixture.close().await.unwrap();
        drop(logging);

        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        let records = logs
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| record["fields"]["message"] == "controller request")
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1, "{logs}");
        let record = &records[0];
        assert_eq!(record["level"], "INFO");
        let fields = &record["fields"];
        assert_eq!(fields["operation_id"], 7);
        assert_eq!(fields["operation"], "fold section");
        assert_eq!(fields["lane"], "presentation");
        assert_eq!(fields["host"], "local");
        assert_eq!(fields["outcome"], "ok");
        assert!(fields["queued_ms"].is_u64() && fields["run_ms"].is_u64());
        // The record names the operation, never what it carried.
        assert!(!record.to_string().contains("private-section"));
    }

    #[test]
    fn request_records_name_the_outcome_and_the_executing_host() {
        assert_eq!(request_outcome(&UiReply::message("done"), false), "ok");
        assert_eq!(request_outcome(&UiReply::failure("no"), false), "failed");
        assert_eq!(request_outcome(&UiReply::stale("changed"), false), "stale");
        assert_eq!(
            request_outcome(&UiReply::uncertain("lost"), false),
            "uncertain"
        );
        assert_eq!(
            request_outcome(&UiReply::no_change("Request cancelled"), true),
            "cancelled"
        );
        let (logging, output) = capture_logs();
        log_request(
            1,
            "remove worktree",
            &Lane::Removal(Some("builder".into()), "row".into()),
            Duration::from_millis(1500),
            Duration::from_millis(20),
            "ok",
            false,
        );
        log_request(
            2,
            "clean worktrees",
            &cleanup([(None, "a"), (Some("builder".into()), "b")]),
            Duration::ZERO,
            Duration::ZERO,
            "failed",
            false,
        );
        // The terminal's own reads are routine: only a failed or slow one
        // is worth a line at this level.
        let prefetch = Lane::Prefetch(None);
        let quick = Duration::from_millis(40);
        for (id, run, outcome) in [
            (3, quick, "ok"),
            (4, quick, "cancelled"),
            (5, quick, "replaced"),
            (6, quick, "failed"),
            (7, Duration::from_millis(1200), "ok"),
        ] {
            log_request(
                id,
                "prefetch action palette",
                &prefetch,
                quick,
                run,
                outcome,
                true,
            );
        }
        drop(logging);
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        let records = logs
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            records
                .iter()
                .map(|record| record["fields"]["operation_id"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [1, 2, 6, 7],
            "{logs}"
        );
        let fields = &records[0]["fields"];
        assert_eq!(fields["host"], "builder");
        assert_eq!(fields["lane"], "removal");
        assert_eq!(fields["queued_ms"], 1500);
        assert_eq!(fields["run_ms"], 20);
        assert_eq!(records[1]["fields"]["host"], "fleet");
        assert_eq!(records[1]["fields"]["lane"], "cleanup");
    }

    #[test]
    fn a_queued_request_names_what_it_waits_for() {
        let queued = |action: UiAction| Queued {
            request: wt_tui::UiRequest {
                id: 1,
                generation: 0,
                action,
                cancellation: None,
            },
            accepted_at: Instant::now(),
            background: false,
        };
        let restack = UiAction::Restack { key: "row".into() };
        let remove = UiAction::Remove {
            key: "later".into(),
            force: false,
            revision: None,
            seen: None,
        };
        let pending = VecDeque::from([
            (lane_for(&restack).unwrap(), queued(restack)),
            (lane_for(&remove).unwrap(), queued(remove)),
        ]);
        let busy = Admission::from([Lane::Creation(None)]);
        let running = HashMap::new();
        // Nothing running excludes the later removal: it keeps its place
        // behind the whole-host request that arrived first.
        assert_eq!(
            waiting_reason(&busy, &running, &pending, 1),
            "waiting behind queued restack on local host"
        );
    }

    #[test]
    fn remote_create_failure_restores_input_but_ambiguous_outcome_does_not() {
        let retry = Some((Some("builder".into()), "keep my title".into(), None));
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

        /// Parallel tests hit these callsites with no subscriber. Reporting
        /// `sometimes` keeps any interest rebuilt while this subscriber is
        /// alive from caching `never`, so each event asks it directly.
        struct PerEventInterest;

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PerEventInterest {
            fn register_callsite(
                &self,
                _: &'static tracing::Metadata<'static>,
            ) -> tracing::subscriber::Interest {
                tracing::subscriber::Interest::sometimes()
            }
        }

        use tracing_subscriber::layer::SubscriberExt;
        let output = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_ansi(false)
            .with_writer({
                let output = output.clone();
                move || LogWriter(output.clone())
            })
            .finish()
            .with(PerEventInterest);
        let emit = || {
            action_reply(Ok(UiReply::failure("GitHub rejected the update")), None);
            action_reply_with_context(
                Err(anyhow::anyhow!("remote connection failed")),
                None,
                Some(&ActionLogContext {
                    operation_id: 7,
                    operation: "remove worktree".into(),
                    host: "MaxBook".into(),
                    target: Some("new-frontend".into()),
                    pr: None,
                }),
            );
            // Configured actions and GitHub writes name their own operation,
            // checkout and pinned pull request rather than a generic action.
            let run = UiAction::RunAction {
                surface: wt_tui::ActionSurface::Row {
                    key: "review-row".into(),
                },
                id: "address-pr-review".into(),
                arg: None,
                extras: String::new(),
                seen: None,
            };
            action_reply_with_context(
                Ok(UiReply::failure("Codex delivery failed")),
                None,
                Some(&ActionLogContext::for_action(
                    8,
                    &run,
                    &lane_for(&run).unwrap(),
                )),
            );
            let auto_merge = UiAction::GithubSetAutoMerge {
                key: wt_core::remote_worktree_ledger_key("builder", "auto-row"),
                enable: true,
                pin: Some(wt_tui::PrPin {
                    number: 42,
                    head_sha: Some("abc".into()),
                }),
            };
            action_reply_with_context(
                Ok(UiReply::failure("auto-merge rejected")),
                None,
                Some(&ActionLogContext::for_action(
                    9,
                    &auto_merge,
                    &lane_for(&auto_merge).unwrap(),
                )),
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
        };
        let guard = tracing::subscriber::set_default(subscriber);
        // Callsite interest is process-global: a parallel test that first
        // reaches one of these callsites with no subscriber can cache it as
        // disabled concurrently with our rebuild. Rebuild and re-emit until
        // the cache includes this subscriber, then judge the last pass only.
        for _ in 0..50 {
            output.lock().unwrap().clear();
            tracing::callsite::rebuild_interest_cache();
            emit();
            if output
                .lock()
                .unwrap()
                .iter()
                .filter(|byte| **byte == b'\n')
                .count()
                >= 5
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(guard);
        tracing::callsite::rebuild_interest_cache();

        let output = output.lock().unwrap().clone();
        let log = String::from_utf8(output).unwrap();
        let events = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 5, "{log}");
        assert_eq!(
            events
                .iter()
                .filter(|event| event["fields"]["event_channel"] == "attention")
                .count(),
            5,
            "{log}"
        );
        let by_id = |id: u64| {
            events
                .iter()
                .find(|event| event["fields"]["operation_id"] == id)
                .unwrap_or_else(|| panic!("operation {id} logged: {log}"))
        };
        let run = by_id(8);
        assert_eq!(run["fields"]["operation"], "run action address-pr-review");
        assert_eq!(run["fields"]["worktree"], "review-row");
        assert_eq!(run["fields"]["host"], "local host");
        assert!(run["fields"].get("pr").is_none(), "{log}");
        let auto_merge = by_id(9);
        assert_eq!(
            auto_merge["fields"]["operation"],
            "change pull request auto-merge"
        );
        assert_eq!(auto_merge["fields"]["worktree"], "auto-row");
        assert_eq!(auto_merge["fields"]["host"], "builder");
        assert_eq!(auto_merge["fields"]["pr"], 42);
        assert!(!log.contains("host action"), "{log}");
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
        // Exercise routing as well as admission, including managed hosts, so
        // no entry point regresses to a host lane.
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
                UiAction::SetIssueOverride {
                    key: "selected".into(),
                    issue_id: None,
                },
                UiAction::SetStatus {
                    key: "selected".into(),
                    state: None,
                    note: None,
                    verify_after_merge: None,
                    verify_in: None,
                    seen: None,
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
                assert!(!lanes_conflict(&lane, &Lane::Host(host.clone())));
                assert!(lanes_conflict(
                    &lane,
                    &cleanup([(host.clone(), "selected")])
                ));
                assert!(!lanes_conflict(&lane, &cleanup([(host.clone(), "other")])));
                assert!(!lanes_conflict(&lane, &Lane::Creation(host.clone())));
            }
        }
    }

    #[test]
    fn metadata_does_not_wait_for_compaction_or_github_host_jobs() {
        let action = wt_tui::UiAction::SetTitle {
            key: "selected".into(),
            title: "Saved now".into(),
        };
        let lane = lane_for(&action).unwrap();
        let busy = Admission::from([
            Lane::Host(None),
            Lane::Host(Some("one".into())),
            Lane::Host(Some("two".into())),
            Lane::Host(Some("three".into())),
        ]);
        let mut pending = VecDeque::from([(lane.clone(), action.clone())]);
        assert_eq!(take_available(&mut pending, &busy), Some((lane, action)));
    }

    #[test]
    fn preferences_and_checkout_actions_do_not_wait_for_creation_on_either_host() {
        use wt_tui::UiAction;
        for host in [None, Some("MaxBook".to_owned())] {
            for action in [
                UiAction::CyclePrimary,
                UiAction::CancelAutomations,
                UiAction::ToggleAutomations {
                    key: None,
                    seen: None,
                },
                UiAction::GithubMarkReady {
                    key: "existing".into(),
                    pin: None,
                },
                UiAction::DiscoverSessions {
                    key: Some("existing".into()),
                    target: wt_tui::SessionTarget::Harness,
                },
            ] {
                let action = UiAction::OnHost {
                    host: host.clone(),
                    action: Box::new(action),
                };
                let lane = lane_for(&action).unwrap();
                let mut pending = VecDeque::from([(lane.clone(), action.clone())]);
                assert_eq!(
                    take_available(
                        &mut pending,
                        &Admission::from([Lane::Creation(host.clone())])
                    ),
                    Some((lane, action))
                );
            }
            let action = UiAction::OnHost {
                host: host.clone(),
                action: Box::new(UiAction::CyclePrimary),
            };
            let lane = lane_for(&action).unwrap();
            let mut pending = VecDeque::from([(lane.clone(), action.clone())]);
            let busy = (0..4)
                .map(|i| Lane::Removal(host.clone(), format!("busy-{i}")))
                .collect();
            assert_eq!(take_available(&mut pending, &busy), Some((lane, action)));
        }
    }

    #[test]
    fn target_commands_keep_real_conflicts_and_control_capacity_is_bounded() {
        let target = Lane::Target(Some("MaxBook".into()), "existing".into());
        for (other, expected) in [
            (
                Lane::Removal(Some("MaxBook".into()), "existing".into()),
                true,
            ),
            (Lane::Removal(Some("MaxBook".into()), "other".into()), false),
            (Lane::Removal(None, "existing".into()), false),
            (
                Lane::Terminal(Some("MaxBook".into()), Some("existing".into())),
                false,
            ),
            (Lane::Host(Some("MaxBook".into())), true),
            (cleanup([(Some("MaxBook".into()), "existing")]), true),
            (cleanup([(Some("MaxBook".into()), "other")]), false),
            (cleanup([(None, "existing")]), false),
        ] {
            assert_eq!(lanes_conflict(&target, &other), expected);
            assert_eq!(lanes_conflict(&other, &target), expected);
        }
        let stop = Lane::SessionControl(Some("MaxBook".into()), Some("existing".into()));
        let terminal = Lane::Terminal(Some("MaxBook".into()), Some("existing".into()));
        assert!(lanes_conflict(&stop, &terminal));
        assert!(lanes_conflict(&terminal, &stop));
        assert!(!lanes_conflict(
            &stop,
            &Lane::Terminal(None, Some("existing".into()))
        ));
        // One control at a time per host; a view control is this device's own.
        let mut pending = VecDeque::from([(Lane::HostControl(None), ())]);
        assert!(
            take_available(&mut pending, &Admission::from([Lane::HostControl(None)])).is_none()
        );
        assert!(take_available(&mut pending, &Admission::from([Lane::ViewControl])).is_some());
    }

    #[test]
    fn progress_is_not_a_completion_or_a_worker_protocol_message() {
        let reply = progress_reply(7, 9, "create worktree: running (3s)".into());
        assert_eq!(reply.operation_id, Some(7));
        assert_eq!(reply.ui_generation, Some(9));
        assert!(reply.outcome.is_none());
        assert!(!reply.failed);
        let wire = serde_json::to_value(&reply).unwrap();
        assert!(wire.get("progress").is_none());
        assert!(
            serde_json::from_value::<UiReply>(wire)
                .unwrap()
                .progress
                .is_none()
        );
    }

    #[test]
    fn a_background_read_never_holds_a_menu_the_user_is_waiting_for() {
        let warm = lane_for(&wt_tui::UiAction::DiscoverSessions {
            key: Some("one".into()),
            target: wt_tui::SessionTarget::Harness,
        })
        .unwrap();
        assert_eq!(warm, Lane::Prefetch(None));
        let menu = Lane::Preparation(None);
        // A running prefetch leaves the preparation slot free...
        let mut pending = VecDeque::from([(warm.clone(), 1), (menu.clone(), 2)]);
        let busy = Admission::from([warm.clone()]);
        assert_eq!(take_available(&mut pending, &busy), Some((menu.clone(), 2)));
        // ...and only one prefetch runs at a time.
        assert_eq!(take_available(&mut pending, &busy), None);
        // A foreground preparation does not wait for the prefetch either.
        let mut pending = VecDeque::from([(warm.clone(), 3)]);
        let busy = Admission::from([menu]);
        assert_eq!(take_available(&mut pending, &busy), Some((warm, 3)));
    }

    #[test]
    fn metadata_admission_is_bounded_without_consuming_ordinary_capacity() {
        let active = Lane::Metadata(None, "first".into());
        let next = Lane::Metadata(None, "second".into());
        let removal = Lane::Removal(None, "third".into());
        let mut pending = VecDeque::from([(next.clone(), 1), (removal.clone(), 2)]);
        let busy = Admission::from([active]);
        assert_eq!(take_available(&mut pending, &busy), Some((removal, 2)));
        assert_eq!(take_available(&mut pending, &busy), None);
        assert_eq!(
            take_available(&mut pending, &Admission::default()),
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
        let busy = Admission::from([remote.clone()]);
        assert_eq!(
            take_available(&mut pending, &busy),
            Some((local, title("local")))
        );
        assert_eq!(take_available(&mut pending, &busy), None);
        assert_eq!(
            take_available(&mut pending, &Admission::default()),
            Some((remote.clone(), title("first")))
        );
        assert_eq!(
            take_available(&mut pending, &Admission::default()),
            Some((remote, title("second")))
        );
    }

    #[test]
    fn read_only_preparations_run_in_a_lane_separate_from_host_mutations() {
        let preparation = UiAction::PrepareRemove { key: "row".into() };
        let mutation = UiAction::Remove {
            key: "row".into(),
            force: false,
            revision: Some(wt_tui::RemovalRevision {
                key: "row".into(),
                path: "/work/row".into(),
                branch: "row".into(),
                head: "head".into(),
                digest: "digest".into(),
                hazards: vec![],
                published_base: None,
            }),
            seen: None,
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
        let busy = Admission::from([Lane::Host(None)]);
        assert_eq!(
            take_available(&mut pending, &busy),
            Some((preparation_lane, preparation))
        );
    }

    #[test]
    fn cleanup_owns_only_the_checkouts_it_removes() {
        assert_eq!(
            lane_for(&UiAction::Cleanup { revisions: vec![] }).unwrap(),
            Lane::FleetCleanup(BTreeSet::new())
        );
    }

    #[test]
    fn durable_removal_only_blocks_same_target_and_global_host_operations() {
        let active = Lane::Removal(None, "old-worktree".into());
        let other_remove = Lane::Removal(None, "other-worktree".into());
        let create = lane_for(&UiAction::Create {
            placement: None,
            input: "new".into(),
        })
        .unwrap();
        assert_eq!(create, Lane::Creation(None));
        assert!(!lanes_conflict(&active, &other_remove));
        assert!(!lanes_conflict(&active, &create));
        assert!(lanes_conflict(&active, &Lane::Host(None)));
        assert!(lanes_conflict(&active, &cleanup([(None, "old-worktree")])));
        assert!(!lanes_conflict(&active, &cleanup([(None, "unrelated")])));

        let mut pending = VecDeque::from([
            (other_remove.clone(), "other removal"),
            (create.clone(), "new creation"),
            (
                Lane::Removal(None, "old-worktree".into()),
                "same-target removal",
            ),
            (Lane::Host(None), "global host write"),
        ]);
        let busy = Admission::from([active]);
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
                revision: Some(wt_tui::RemovalRevision {
                    key: "new-frontend".into(),
                    path: "/work/new-frontend".into(),
                    branch: "new-frontend".into(),
                    head: "head".into(),
                    digest: "digest".into(),
                    hazards: vec![],
                    published_base: None,
                }),
                seen: None,
            }),
            Some("new-frontend".into())
        );
        assert_eq!(
            action_target(&UiAction::Create {
                placement: None,
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
                name: "remove worktree".into(),
                target: Some("new-frontend".into()),
                pr: None,
                started,
                admitted: started,
                background: false,
            },
            Instant::now(),
        );
        assert!(progress.contains("remove worktree for new-frontend on MaxBook is running"));
        assert!(progress.contains("1m15s elapsed"));
        assert!(progress.contains("operation 42"));
    }

    #[test]
    fn view_controls_have_reserved_capacity_while_host_work_is_saturated() {
        let busy = Admission::from([
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
                &Admission::from([
                    Lane::Terminal(Some("other-host".into()), Some("one".into())),
                    Lane::Creation(None)
                ])
            ),
            None
        );
        assert_eq!(
            take_available(
                &mut pending,
                &Admission::from([
                    Lane::Creation(None),
                    Lane::Removal(None, "other".into()),
                    cleanup([(None, "other")])
                ])
            ),
            Some((Lane::Terminal(None, None), terminal))
        );
    }

    #[test]
    fn terminal_checkout_conflicts_only_with_removal_or_cleanup_of_that_checkout() {
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
            (cleanup([(Some(host.clone()), "one")]), true),
            (cleanup([(Some(host.clone()), "two")]), false),
            (cleanup([(None, "one")]), false),
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
        let long_jobs = ["a", "b", "c", "d"].map(|key| Lane::Removal(None, key.into()));
        let busy = Admission::from(long_jobs.clone());
        let preparation = UiAction::PrepareRemove { key: "row".into() };
        let preparation_lane = lane_for(&preparation).unwrap();
        // A fifth long job on a checkout nothing else touches, so the test
        // isolates capacity from conflicts.
        let fifth_long_job = Lane::Action(None, Some("e".into()));
        let creation_lane = Lane::Creation(None);
        let mut pending = VecDeque::from([
            (fifth_long_job, UiAction::HardRefresh),
            (preparation_lane.clone(), preparation.clone()),
            (
                creation_lane.clone(),
                UiAction::Create {
                    placement: None,
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
                    placement: None,
                    input: "new-row".into()
                }
            ))
        );
        let with_reserved = long_jobs
            .into_iter()
            .chain([preparation_lane, creation_lane])
            .collect::<Admission>();
        assert_eq!(take_available(&mut pending, &with_reserved), None);
    }

    #[test]
    fn view_controls_still_serialize_and_do_not_consume_host_capacity() {
        let busy = Admission::from([
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
                placement: None,
                input: "retry title".into(),
            }),
        };
        assert_eq!(
            create_retry(&action),
            Some((Some("builder".into()), "retry title".into(), None))
        );
    }

    #[test]
    fn fleet_cleanup_lane_owns_exactly_its_local_and_remote_targets() {
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
        assert_eq!(
            lane_for(&action).unwrap(),
            cleanup([(None, "local-row"), (Some("builder".into()), "remote-row")])
        );
        assert_eq!(
            lane_for(&UiAction::CleanupTargets { targets: vec![] }).unwrap(),
            Lane::FleetCleanup(BTreeSet::new())
        );
        // An explicit host route owns host-local keys, as host routing does.
        let routed = UiAction::OnHost {
            host: Some("builder".into()),
            action: Box::new(UiAction::CleanupTargets {
                targets: vec![wt_tui::CleanupTarget {
                    key: "remote-local-key".into(),
                    path: "/remote/local".into(),
                    branch: "b".into(),
                    head: "h".into(),
                }],
            }),
        };
        assert_eq!(
            lane_for(&routed).unwrap(),
            cleanup([(Some("builder".into()), "remote-local-key")])
        );
        // A remote worker receives host-local keys without a route.
        assert_eq!(
            lane_for(&UiAction::CleanupTargets {
                targets: vec![wt_tui::CleanupTarget {
                    key: "remote-local-key".into(),
                    path: "/remote/local".into(),
                    branch: "b".into(),
                    head: "h".into(),
                }],
            })
            .unwrap(),
            cleanup([(None, "remote-local-key")])
        );

        let active = lane_for(&action).unwrap();
        let host = Lane::Host(Some("builder".into()));
        assert!(lanes_conflict(&active, &host));
        assert!(lanes_conflict(&host, &active));
        assert!(!lanes_conflict(
            &active,
            &Lane::Host(Some("elsewhere".into()))
        ));
        assert!(lanes_conflict(&active, &cleanup([(None, "unrelated")])));
        assert!(!lanes_conflict(
            &active,
            &Lane::Preparation(Some("builder".into()))
        ));
        let preparation = Lane::Preparation(Some("builder".into()));
        let mut pending = VecDeque::from([(preparation.clone(), "open removal review")]);
        assert_eq!(
            take_available(&mut pending, &Admission::from([active.clone()])),
            Some((preparation, "open removal review"))
        );
        let mut pending = VecDeque::from([(host.clone(), "host mutation")]);
        assert_eq!(
            take_available(&mut pending, &Admission::from([active])),
            None
        );
    }

    #[test]
    fn busy_cleanup_admits_unrelated_checkouts_and_holds_its_own_targets() {
        let remote = Some("builder".to_owned());
        let busy = Admission::from([cleanup([(None, "a"), (remote.clone(), "b")])]);
        for admitted in [
            Lane::Terminal(None, Some("c".into())),
            Lane::Terminal(remote.clone(), Some("c".into())),
            Lane::Terminal(None, Some("b".into())),
            Lane::Terminal(None, None),
            Lane::Metadata(None, "c".into()),
            Lane::Target(None, "c".into()),
            Lane::SessionControl(None, Some("c".into())),
            Lane::Removal(None, "c".into()),
            Lane::Creation(None),
            Lane::Creation(remote.clone()),
            Lane::Host(Some("elsewhere".into())),
        ] {
            let mut pending = VecDeque::from([(admitted.clone(), ())]);
            assert_eq!(
                take_available(&mut pending, &busy),
                Some((admitted.clone(), ())),
                "{admitted:?} should not wait for unrelated cleanup"
            );
        }
        for held in [
            Lane::Terminal(None, Some("a".into())),
            Lane::Terminal(remote.clone(), Some("b".into())),
            Lane::Metadata(None, "a".into()),
            Lane::Metadata(remote.clone(), "b".into()),
            Lane::Target(remote.clone(), "b".into()),
            Lane::SessionControl(None, Some("a".into())),
            Lane::Removal(remote.clone(), "b".into()),
            Lane::Host(None),
            Lane::Host(remote.clone()),
            cleanup([(None, "z")]),
        ] {
            let mut pending = VecDeque::from([(held.clone(), ())]);
            assert_eq!(
                take_available(&mut pending, &busy),
                None,
                "{held:?} must wait for cleanup of its target"
            );
        }
    }

    #[test]
    fn waiting_progress_names_cleanup_targets() {
        let action = UiAction::CleanupTargets {
            targets: ["a", "b"]
                .into_iter()
                .map(|key| wt_tui::CleanupTarget {
                    key: key.into(),
                    path: format!("/{key}"),
                    branch: key.into(),
                    head: "h".into(),
                })
                .collect(),
        };
        assert_eq!(action_target(&action).as_deref(), Some("a, b"));
        assert_eq!(lane_host(&lane_for(&action).unwrap()), "local host");
    }

    fn cleanup<const N: usize>(targets: [(Option<String>, &str); N]) -> Lane {
        Lane::FleetCleanup(
            targets
                .into_iter()
                .map(|(host, key)| (host, key.to_owned()))
                .collect(),
        )
    }
}
