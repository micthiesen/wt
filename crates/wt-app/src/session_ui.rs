//! Host-owned session picker preparation. Only terminal attachment belongs to
//! the controller; discovery and stop operations run on the selected host.
use crate::context::AppContext;
use anyhow::Result;
use wt_tui::{
    PickerAction, PickerOption, SessionMode, SessionSelection, UiAction, UiModal, UiReply,
};

pub async fn execute(context: &AppContext, action: UiAction) -> Result<UiReply> {
    match action {
        UiAction::PrepareStopTerminal { key, target } => {
            let (tmux, name, label) = terminal_target(context, &key, target).await?;
            let Some(session) = tmux
                .list_sessions(&context.cancellation)
                .await?
                .into_iter()
                .find(|session| session.name == name)
            else {
                return Ok(UiReply {
                    message: format!("No {label} session is running"),
                    ..Default::default()
                });
            };
            Ok(UiReply {
                modal: Some(UiModal::Confirm {
                    title: format!("Stop {label} session?"),
                    lines: vec![
                        name,
                        if target == wt_tui::SessionTarget::Shell {
                            "Processes running in this shell will stop."
                        } else {
                            "The diff viewer will restart fresh next time."
                        }
                        .into(),
                    ],
                    action: wt_tui::ConfirmAction::StopTerminal {
                        key,
                        target,
                        session_id: session.id,
                        created_at: session.created_at,
                    },
                    cancel_key: None,
                }),
                ..Default::default()
            })
        }
        UiAction::StopTerminal {
            key,
            target,
            session_id,
            created_at,
        } => {
            let (tmux, name, label) = terminal_target(context, &key, target).await?;
            let live = tmux.list_sessions(&context.cancellation).await?;
            if let Some(session) = live.iter().find(|session| session.name == name) {
                if session.id != session_id || session.created_at != created_at {
                    anyhow::bail!(
                        "{label} session changed since confirmation; open the stop dialog again"
                    );
                }
                tmux.kill_session_id(&session_id, &context.cancellation)
                    .await?;
            }
            Ok(UiReply {
                message: format!("Stopped {label} session"),
                ..Default::default()
            })
        }
        UiAction::PrepareSessions { key, target } => {
            let sessions =
                crate::harness::list_session_options(context, key.clone(), target).await?;
            let mut choices = Vec::new();
            let mut options = Vec::new();
            for session in sessions {
                options.push(PickerOption {
                    value: Some(choices.len().to_string()),
                    label: format!(
                        "{} / {}{}",
                        session.selection.harness.as_str(),
                        session.display_name,
                        if session.is_live { " · live" } else { "" }
                    ),
                    chord: None,
                    note: None,
                    verify_after_merge: None,
                });
                choices.push(session.selection);
            }
            for harness in wt_core::HarnessId::ALL
                .into_iter()
                .filter(|id| !context.config.harness.hidden.contains(id))
            {
                options.push(PickerOption {
                    value: Some(choices.len().to_string()),
                    label: format!("New {} session", harness.as_str()),
                    chord: None,
                    note: None,
                    verify_after_merge: None,
                });
                choices.push(SessionSelection {
                    key: key.clone(),
                    target,
                    harness,
                    session_id: None,
                    managed_name: None,
                    mode: SessionMode::New,
                });
            }
            Ok(UiReply {
                modal: Some(UiModal::Picker {
                    action: PickerAction::Sessions { choices },
                    title: "Sessions · d stops selected session".into(),
                    options,
                    selected: 0,
                }),
                ..Default::default()
            })
        }
        UiAction::PrepareStopSession { selection } => Ok(UiReply {
            modal: Some(UiModal::Confirm {
                title: "Stop this agent session?".into(),
                lines: vec![
                    format!(
                        "{} · {}",
                        selection.harness.as_str(),
                        selection.session_id.as_deref().unwrap_or("unknown session")
                    ),
                    "Ongoing work in this session will stop. The conversation remains resumable."
                        .into(),
                ],
                action: wt_tui::ConfirmAction::StopSession { selection },
                cancel_key: Some('d'),
            }),
            ..Default::default()
        }),
        UiAction::StopSession { selection } => {
            crate::harness::stop_managed_session(context, &selection).await?;
            Ok(UiReply {
                message: "Stopped selected session".into(),
                ..Default::default()
            })
        }
        _ => anyhow::bail!("not a session picker command"),
    }
}

async fn terminal_target(
    context: &AppContext,
    key: &str,
    target: wt_tui::SessionTarget,
) -> Result<(wt_tmux::TmuxClient, String, &'static str)> {
    let label = match target {
        wt_tui::SessionTarget::Shell => "shell",
        wt_tui::SessionTarget::Diff => "diff",
        _ => anyhow::bail!("only shell and diff sessions use terminal stop"),
    };
    let row = crate::lifecycle_ops::resolve_key(context, key).await?;
    if row.is_main || !matches!(row.target.location(), wt_core::WorktreeLocation::Local) {
        anyhow::bail!("terminal stop requires a worktree on this host");
    }
    let name = format!("{}-{label}", row.target.slug());
    Ok((
        wt_tmux::TmuxClient::new(
            context.processes.clone(),
            wt_tmux::TmuxServer::named(context.config.tmux.socket.clone())
                .with_cwd(context.home.clone()),
        ),
        name,
        label,
    ))
}
