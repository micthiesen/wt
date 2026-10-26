//! Host-owned session actions: terminal stop, live session stop and kill.
//! Pickers are built from the board (`wt_tui::menus::sessions`); the host
//! answers a picker only for targets the board has no prepared entries for.
use crate::context::AppContext;
use anyhow::Result;
use wt_tui::{UiAction, UiReply};

pub async fn execute(context: &AppContext, action: UiAction) -> Result<UiReply> {
    match action {
        // The board answers running local terminals; the host reads tmux for
        // retained dead panes, remote rows and boards without inventory yet.
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
                modal: Some(wt_tui::stop_terminal_confirm(
                    &key,
                    target,
                    &name,
                    &session.id,
                    session.created_at,
                )),
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
                    return Err(crate::controller::stale(format!(
                        "{label} session changed since confirmation; open the stop dialog again"
                    )));
                }
                tmux.kill_session_id(&session_id, &context.cancellation)
                    .await?;
            }
            Ok(UiReply {
                message: format!("Stopped {label} session"),
                ..Default::default()
            })
        }
        UiAction::StopSession { selection } => {
            let label = wt_tui::harness_label(selection.harness);
            let closed = crate::harness::stop_managed_session(
                context,
                &selection,
                crate::harness::SessionEnd::Graceful,
            )
            .await?;
            Ok(UiReply {
                message: if closed {
                    format!("Closed {label} session")
                } else {
                    "Session isn't live, nothing to close".into()
                },
                ..Default::default()
            })
        }
        UiAction::KillSession { selection } => {
            let label = wt_tui::harness_label(selection.harness);
            if selection.live {
                let killed = crate::harness::stop_managed_session(
                    context,
                    &selection,
                    crate::harness::SessionEnd::Kill,
                )
                .await?;
                return Ok(UiReply {
                    message: if killed {
                        format!("Killed {label} session")
                    } else {
                        "Session isn't live, nothing to kill".into()
                    },
                    ..Default::default()
                });
            }
            if selection.harness != wt_core::HarnessId::Claude {
                anyhow::bail!(
                    "{label} session is dead; remove via {} CLI",
                    selection.harness.as_str()
                );
            }
            let name = crate::harness::forget_claude_session(context, &selection).await?;
            Ok(UiReply {
                message: format!("Forgot ghost session \"{name}\""),
                ..Default::default()
            })
        }
        _ => anyhow::bail!("not a session action"),
    }
}

async fn terminal_target(
    context: &AppContext,
    key: &str,
    target: wt_tui::SessionTarget,
) -> Result<(wt_tmux::TmuxClient, String, &'static str)> {
    if !matches!(
        target,
        wt_tui::SessionTarget::Shell | wt_tui::SessionTarget::Diff
    ) {
        anyhow::bail!("only shell and diff sessions use terminal stop");
    }
    let label = wt_tui::terminal_label(target);
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
