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
                    detail: None,
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
                    detail: None,
                });
                choices.push(SessionSelection {
                    key: key.clone(),
                    target,
                    harness,
                    session_id: None,
                    managed_name: None,
                    mode: SessionMode::New,
                    live: false,
                });
            }
            Ok(UiReply {
                modal: Some(UiModal::Picker {
                    action: PickerAction::Sessions { choices },
                    title: "Sessions".into(),
                    options,
                    selected: 0,
                }),
                ..Default::default()
            })
        }
        UiAction::StopSession { selection } => {
            let label = harness_label(selection.harness);
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
            let label = harness_label(selection.harness);
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
        UiAction::PrepareHarnesses { key } => {
            let primary = crate::harness::AppHarness::new(context).primary();
            Ok(UiReply {
                modal: Some(harness_picker(key, &context.config.harness.hidden, primary)),
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

/// Display name for a harness, as used in replies.
pub fn harness_label(harness: wt_core::HarnessId) -> &'static str {
    match harness {
        wt_core::HarnessId::Claude => "Claude",
        wt_core::HarnessId::Codex => "Codex",
        wt_core::HarnessId::Opencode => "OpenCode",
    }
}

/// Quick-pick letter for each harness in the TS pickers.
pub fn harness_letter(harness: wt_core::HarnessId) -> char {
    match harness {
        wt_core::HarnessId::Claude => 'c',
        wt_core::HarnessId::Codex => 'x',
        wt_core::HarnessId::Opencode => 'o',
    }
}

/// Shift+F12 chooser over the visible harnesses, cursor on the primary.
fn harness_picker(
    key: String,
    hidden: &std::collections::BTreeSet<wt_core::HarnessId>,
    primary: wt_core::HarnessId,
) -> UiModal {
    let visible: Vec<_> = wt_core::HarnessId::ALL
        .into_iter()
        .filter(|id| !hidden.contains(id))
        .collect();
    UiModal::Picker {
        action: PickerAction::Harness { key },
        title: "Start agent".into(),
        selected: visible.iter().position(|id| *id == primary).unwrap_or(0),
        options: visible
            .into_iter()
            .map(|id| PickerOption {
                value: Some(id.as_str().into()),
                label: harness_label(id).into(),
                chord: Some(harness_letter(id)),
                note: None,
                verify_after_merge: None,
                detail: None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_core::HarnessId;

    #[test]
    fn harness_picker_skips_hidden_and_starts_on_primary() {
        let UiModal::Picker {
            action,
            options,
            selected,
            ..
        } = harness_picker(
            "one".into(),
            &[HarnessId::Codex].into_iter().collect(),
            HarnessId::Opencode,
        )
        else {
            panic!("expected a picker");
        };
        assert_eq!(action, PickerAction::Harness { key: "one".into() });
        let values: Vec<_> = options
            .iter()
            .map(|option| (option.value.clone().unwrap(), option.chord))
            .collect();
        assert_eq!(
            values,
            vec![("claude".into(), Some('c')), ("opencode".into(), Some('o'))]
        );
        assert_eq!(selected, 1);
    }
}
