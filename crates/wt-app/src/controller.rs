use anyhow::{Context, Result, bail};
use std::time::Duration;
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::Arc,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wt_runtime::TaskScope;
use wt_tui::{ActionController, UiReply};

use crate::context::AppContext;

/// Commands serialize within each host and drain on shutdown. Independent
/// hosts progress concurrently, so an unavailable host cannot block local work.
/// Services own their own cancellation and irreversible commit boundaries.
pub struct Controller {
    task: JoinHandle<()>,
    cancel: CancellationToken,
}

impl Controller {
    pub async fn shutdown(mut self) -> Result<()> {
        let result = match tokio::time::timeout(Duration::from_secs(30), &mut self.task).await {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!("waiting for accepted commands to finish before exiting");
                eprintln!("wt: waiting for accepted commands to finish before exiting");
                // Service operations own their timeouts and commit boundaries.
                // A UI shutdown deadline cannot safely cancel an accepted write.
                self.task.await
            }
        };
        self.cancel.cancel();
        result.context("action controller exited")
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
    context.cancellation = cancel.clone();
    let context = Arc::new(context);
    let fleet = Arc::new(fleet);
    let task = scope.spawn(async move {
        let mut pending = VecDeque::new();
        let mut busy = BTreeSet::new();
        let mut running = tokio::task::JoinSet::new();
        let mut task_lanes = HashMap::new();
        let mut failed_lanes = BTreeSet::new();
        let mut closed = false;
        let mut draining = false;
        loop {
            while running.len() < 4 {
                let Some((lane, request)) = take_available(&mut pending, &busy) else { break; };
                let wt_tui::UiRequest { generation, action: command } = request;
                busy.insert(lane.clone());
                let context = context.clone();
                let fleet = fleet.clone();
                let replies = port.replies.clone();
                let shutdown = shutdown.clone();
                let task_lane = lane.clone();
                let task = running.spawn(async move {
                    let retry = create_retry(&command);
                    let result = crate::host_dispatch::execute(&context, &fleet, command, &replies, &shutdown).await;
                    let mut reply = action_reply(result, retry);
                    reply.ui_generation = Some(generation);
                    (lane, reply)
                });
                task_lanes.insert(task.id(), task_lane);
            }
            if closed && pending.is_empty() && running.is_empty() { break; }
            tokio::select! {
                biased;
                _ = shutdown.cancelled(), if !draining => {
                    draining = true;
                    port.requests.close();
                },
                finished = running.join_next_with_id(), if !running.is_empty() => match finished {
                    Some(Ok((id, (lane, reply)))) => {
                        task_lanes.remove(&id);
                        busy.remove(&lane);
                        let _ = port.replies.send(sanitize_reply(reply)).await;
                    },
                    Some(Err(error)) => {
                        tracing::error!(%error, "action worker exited unexpectedly");
                        let _ = port.replies.send(UiReply { failed: true, message: format!("Action worker failed: {error}"), ..Default::default() }).await;
                        if let Some(lane) = task_lanes.remove(&error.id()) {
                            failed_lanes.insert(lane.clone());
                            busy.remove(&lane);
                            let skipped = pending.iter().filter(|(queued, _)| *queued == lane).count();
                            pending.retain(|(queued, _)| *queued != lane);
                            if skipped > 0 {
                                let _ = port.replies.send(UiReply { failed: true, message: format!("{skipped} queued commands on the failed host were not started. Restart wt before retrying."), ..Default::default() }).await;
                            }
                        }
                    },
                    None => {},
                },
                command = port.requests.recv(), if !closed && pending.len() < 32 => match command {
                    Some(command) => match lane_for(&command.action) {
                        Ok(lane) if !failed_lanes.contains(&lane) => pending.push_back((lane, command)),
                        Ok(_) => { let _ = port.replies.send(UiReply { failed: true, message: "Host command worker failed; this command was not started. Restart wt before retrying.".into(), ..Default::default() }).await; },
                        Err(error) => { let _ = port.replies.send(UiReply { failed: true, message: format!("{error:#}"), ..Default::default() }).await; },
                    },
                    None => closed = true,
                },
            }
        }
    });
    Controller { task, cancel }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Lane {
    Host(Option<String>),
    FleetCleanup,
    Presentation,
    Terminal,
}

fn lane_for(action: &wt_tui::UiAction) -> Result<Lane> {
    if is_cleanup_action(action) {
        // A single confirmation can contain revisions from several hosts.
        // Serialize it as one accepted operation so shutdown drains the whole
        // fleet mutation and no second cleanup races its prepared revisions.
        return Ok(Lane::FleetCleanup);
    }
    let (host, action, _) = crate::host_routing::resolve(action.clone())?;
    Ok(
        if matches!(
            action,
            wt_tui::UiAction::Session { .. }
                | wt_tui::UiAction::SelectSession { .. }
                | wt_tui::UiAction::EnterHarness { .. }
                | wt_tui::UiAction::PerfInvestigate { .. }
        ) {
            Lane::Terminal
        } else if crate::host_routing::controller_owned(&action) {
            Lane::Presentation
        } else {
            Lane::Host(host)
        },
    )
}

fn is_cleanup_action(action: &wt_tui::UiAction) -> bool {
    match action {
        wt_tui::UiAction::PrepareCleanup | wt_tui::UiAction::Cleanup { .. } => true,
        wt_tui::UiAction::OnHost { action, .. } => is_cleanup_action(action),
        _ => false,
    }
}

fn take_available<T>(
    pending: &mut VecDeque<(Lane, T)>,
    busy: &BTreeSet<Lane>,
) -> Option<(Lane, T)> {
    let position = pending
        .iter()
        .position(|(lane, _)| busy.iter().all(|active| !lanes_conflict(lane, active)))?;
    pending.remove(position)
}

fn lanes_conflict(left: &Lane, right: &Lane) -> bool {
    left == right
        || matches!(left, Lane::FleetCleanup) && matches!(right, Lane::Host(_))
        || matches!(right, Lane::FleetCleanup) && matches!(left, Lane::Host(_))
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

fn action_reply(result: Result<UiReply>, retry: Option<(Option<String>, String)>) -> UiReply {
    let (mut reply, ambiguous) = match result {
        Ok(reply) => (reply, false),
        Err(error) if error.downcast_ref::<Notice>().is_some() => (
            UiReply {
                message: error.to_string(),
                ..Default::default()
            },
            false,
        ),
        Err(error) => {
            // The full chain, so the feed names the cause, not just the step.
            tracing::error!(error = %format_args!("{error:#}"), "TUI action failed");
            let ambiguous = error
                .downcast_ref::<crate::remote_host::RemoteHostError>()
                .is_some_and(|error| {
                    matches!(error, crate::remote_host::RemoteHostError::Ambiguous { .. })
                });
            (
                UiReply {
                    message: format!("{error:#}"),
                    failed: true,
                    ..Default::default()
                },
                ambiguous,
            )
        }
    };
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

pub(crate) async fn handoff_prepared(
    prepared: crate::harness::PreparedSession,
    replies: &tokio::sync::mpsc::Sender<UiReply>,
    shutdown: &CancellationToken,
) -> Result<UiReply> {
    let (ready, suspended) = tokio::sync::oneshot::channel();
    let (resume, resumed) = tokio::sync::oneshot::channel();
    replies
        .send(UiReply {
            handoff: Some(wt_tui::TerminalHandoff { ready, resumed }),
            ..Default::default()
        })
        .await
        .context("terminal closed before session attach")?;
    tokio::select! {
        _ = shutdown.cancelled() => bail!("session attach cancelled"),
        result = suspended => result.context("terminal could not suspend for session attach")?,
    }
    let result: Result<()> = async {
        let mut child = tokio::process::Command::new(prepared.program)
            .args(prepared.args)
            .current_dir(prepared.cwd)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("attach tmux client")?;
        let status = tokio::select! {
            _ = shutdown.cancelled() => { child.kill().await?; return Ok(()); },
            status = child.wait() => status?,
        };
        if !status.success() {
            bail!("tmux client exited with {status}");
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
    Ok(UiReply {
        message: "Returned from session".into(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_tui::UiAction;

    #[tokio::test(start_paused = true)]
    async fn shutdown_keeps_accepted_work_alive_past_the_notice_deadline() {
        let cancel = CancellationToken::new();
        let (finish, work) = tokio::sync::oneshot::channel();
        let controller = Controller {
            task: tokio::spawn(async move {
                work.await.unwrap();
            }),
            cancel: cancel.clone(),
        };
        let shutdown = tokio::spawn(controller.shutdown());
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(36)).await;
        tokio::task::yield_now().await;
        assert!(!cancel.is_cancelled());
        assert!(!shutdown.is_finished());
        finish.send(()).unwrap();
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
        assert!(reply.failed);
        assert!(reply.modal.is_none());
    }

    #[test]
    fn a_notice_is_a_plain_toast_not_a_failure() {
        let reply = action_reply(Err(notice("already at top")), None);
        assert!(!reply.failed);
        assert_eq!(reply.message, "already at top");
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
    fn qualified_rows_and_captured_create_modals_keep_their_host_lane() {
        let action = UiAction::SetTitle {
            key: wt_core::remote_worktree_ledger_key("builder", "same"),
            title: "title".into(),
        };
        assert_eq!(
            lane_for(&action).unwrap(),
            Lane::Host(Some("builder".into()))
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

        let host = Lane::Host(Some("builder".into()));
        assert!(lanes_conflict(&Lane::FleetCleanup, &host));
        assert!(lanes_conflict(&host, &Lane::FleetCleanup));
        let mut pending = VecDeque::from([(host.clone(), "host mutation")]);
        assert_eq!(
            take_available(&mut pending, &BTreeSet::from([Lane::FleetCleanup])),
            None
        );
    }
}
