use anyhow::{Context, Result, bail};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wt_runtime::{SourceHandle, TaskScope};
use wt_tui::{ActionController, Board, UiAction, UiReply};

use crate::context::AppContext;

/// Accepted commands are serialized and drained on shutdown. Background
/// refreshes and input run independently, so a slow command cannot stall them.
/// Services own their own cancellation and irreversible commit boundaries.
pub struct Controller {
    task: JoinHandle<()>,
    cancel: CancellationToken,
}

impl Controller {
    pub async fn shutdown(mut self) -> Result<()> {
        match tokio::time::timeout(Duration::from_secs(30), &mut self.task).await {
            Ok(result) => result.context("action controller exited"),
            Err(_) => {
                self.cancel.cancel();
                if tokio::time::timeout(Duration::from_secs(5), &mut self.task)
                    .await
                    .is_err()
                {
                    self.task.abort();
                    let _ = self.task.await;
                }
                bail!("action shutdown timed out; unfinished commands were cancelled (see wt log)")
            }
        }
    }
}

pub fn start(
    scope: &TaskScope,
    mut context: AppContext,
    source: SourceHandle<Board>,
    board: SourceHandle<Board>,
    mut port: ActionController,
) -> Controller {
    let shutdown = scope.token();
    // UI/source cancellation closes admission but must not cancel commands
    // already accepted. Their own deadline and explicit drain own cancellation.
    let cancel = CancellationToken::new();
    context.cancellation = cancel.clone();
    let task = scope.spawn(async move {
        loop {
            let command = tokio::select! {
                biased;
                _ = shutdown.cancelled() => { port.requests.close(); port.requests.recv().await }
                command = port.requests.recv() => command,
            };
            let Some(command) = command else {
                break;
            };
            let retry_create = if let UiAction::Create { input } = &command {
                Some(input.clone())
            } else {
                None
            };
            let snapshot = board.snapshot();
            let result = if let UiAction::Session { key, target } = command {
                handoff(&context, key, target, &port.replies, &shutdown).await
            } else {
                crate::controller_actions::execute(&context, command, snapshot.data.as_deref())
                    .await
            };
            let reply = match result {
                Ok(reply) => {
                    source.refresh();
                    reply
                }
                Err(error) => {
                    tracing::error!(%error, "TUI action failed");
                    UiReply {
                        message: wt_core::sanitize_terminal_text(&format!("{error:#}")),
                        failed: true,
                        modal: retry_create.map(|initial| wt_tui::UiModal::Text {
                            action: wt_tui::TextAction::Create,
                            prompt: "New worktree".into(),
                            initial,
                            allow_empty: false,
                        }),
                        ..Default::default()
                    }
                }
            };
            // If the terminal closed, accepted commands still finish. A closed
            // response channel never cancels a durable write.
            let _ = port.replies.send(sanitize_reply(reply)).await;
        }
    });
    Controller { task, cancel }
}

fn sanitize_reply(mut reply: UiReply) -> UiReply {
    reply.message = wt_core::sanitize_terminal_text(&reply.message);
    if let Some(modal) = &mut reply.modal {
        let clean = wt_core::sanitize_terminal_text;
        match modal {
            wt_tui::UiModal::Confirm { title, lines, .. } => {
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

async fn handoff(
    context: &AppContext,
    key: Option<String>,
    target: wt_tui::SessionTarget,
    replies: &tokio::sync::mpsc::Sender<UiReply>,
    shutdown: &CancellationToken,
) -> Result<UiReply> {
    let prepared = crate::harness::ui_session(context, key, target).await?;
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
