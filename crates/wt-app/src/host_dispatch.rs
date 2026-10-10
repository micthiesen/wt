//! Controller routing and device-local effects. Host-owned operations use the
//! same HostService whether called directly or carried over SSH.
use crate::{context::AppContext, remote_board::Fleet};
use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;
use wt_tui::{PickerAction, PickerOption, SessionTarget, UiAction, UiModal, UiReply};

pub async fn execute(
    context: &AppContext,
    fleet: &Fleet,
    command: UiAction,
    replies: &tokio::sync::mpsc::Sender<UiReply>,
    shutdown: &CancellationToken,
) -> Result<UiReply> {
    // Cleanup is a fleet-wide confirmation: prepare the candidate set from
    // every configured host, then route the captured revisions back to their
    // owners. Explicitly host-scoped legacy requests still use the normal path.
    if !matches!(command, UiAction::OnHost { .. }) {
        match command {
            UiAction::PrepareCleanup => return Ok(crate::fleet_cleanup::prepare(fleet).await),
            UiAction::Cleanup { revisions } => {
                return Ok(crate::fleet_cleanup::cleanup(fleet, revisions).await);
            }
            _ => {}
        }
    }
    let (host, action, explicit) = crate::host_routing::resolve(command)?;
    if let UiAction::SetPerf {
        active,
        continuous,
        refresh,
    } = action
    {
        fleet.local.sources.perf.set_active(active);
        fleet.local.sources.perf.set_continuous(continuous);
        if refresh {
            fleet.local.sources.perf.refresh();
        }
        for remote in &fleet.remotes {
            remote.set_perf(active, continuous, refresh);
        }
        return Ok(UiReply::default());
    }
    if let UiAction::SetHistoryActive { active } = action {
        fleet.local.sources.history.set_active(active);
        for remote in &fleet.remotes {
            remote.set_history_active(active);
        }
        return Ok(UiReply::default());
    }
    if let UiAction::SetAttentionSeen { at_ms } = action {
        fleet
            .local
            .context
            .database
            .call(move |store| {
                store.set_attention_seen(at_ms)?;
                Ok(())
            })
            .await?;
        fleet.local.sources.metadata.refresh();
        return Ok(UiReply {
            message: "Attention marked seen".into(),
            ..Default::default()
        });
    }
    if !explicit
        && !fleet.remotes.is_empty()
        && matches!(
            action,
            UiAction::PrepareCreate { .. }
                | UiAction::PrepareHardRefresh
                | UiAction::PrepareCleanup
                | UiAction::ToggleAutomations { key: None }
                | UiAction::CancelAutomations
        )
    {
        let mut options = vec![PickerOption {
            value: None,
            label: "This machine".into(),
            chord: None,
            note: None,
            verify_after_merge: None,
        }];
        options.extend(fleet.remotes.iter().map(|remote| PickerOption {
            value: Some(remote.endpoint.key()),
            label: remote.endpoint.label.clone(),
            chord: None,
            note: None,
            verify_after_merge: None,
        }));
        return Ok(UiReply {
            modal: Some(UiModal::Picker {
                action: PickerAction::Host {
                    action: Box::new(action),
                },
                title: "Choose host".into(),
                options,
                selected: 0,
            }),
            ..Default::default()
        });
    }
    let remote = host
        .as_ref()
        .map(|host| {
            fleet
                .remotes
                .iter()
                .find(|remote| remote.endpoint.key() == *host)
                .with_context(|| format!("host {host:?} is no longer configured"))
        })
        .transpose()?;
    if let UiAction::SelectSession { selection } = action {
        let prepared = match remote {
            None => crate::harness::prepare_session(context, &selection).await?,
            Some(remote) => {
                let client = remote.session_client().await?;
                let prepared =
                    client.interactive_selected_session(&serde_json::to_string(&selection)?);
                crate::harness::PreparedSession {
                    program: prepared.program,
                    args: prepared.args,
                    cwd: context.config.paths.main_clone.clone(),
                }
            }
        };
        return crate::controller::handoff_prepared(prepared, replies, shutdown).await;
    }
    if let UiAction::Session { key, target } = action {
        let prepared = match remote {
            None => crate::harness::ui_session(context, key, target).await?,
            Some(remote) => {
                let client = remote.session_client().await?;
                let target_name = match target {
                    SessionTarget::Harness => "harness",
                    SessionTarget::Shell => "shell",
                    SessionTarget::Diff => "diff",
                    SessionTarget::Manager => "manager",
                    SessionTarget::Main => "main",
                    SessionTarget::WtSource => "wt",
                    SessionTarget::Dotfiles => "dotfiles",
                };
                let prepared =
                    client.interactive_session(key.as_deref().unwrap_or(""), target_name, None);
                crate::harness::PreparedSession {
                    program: prepared.program,
                    args: prepared.args,
                    cwd: context.config.paths.main_clone.clone(),
                }
            }
        };
        return crate::controller::handoff_prepared(prepared, replies, shutdown).await;
    }
    if crate::host_routing::controller_owned(&action) {
        let action = if let Some(host) = &host {
            crate::host_routing::qualify(action, host)?
        } else {
            action
        };
        let snapshot = fleet.board.snapshot();
        let reply = if let UiAction::OpenEditor { key } = &action
            && let Some(remote) = remote
        {
            let row = snapshot
                .data
                .as_ref()
                .and_then(|board| board.rows.iter().find(|row| row.key == *key))
                .context("remote worktree metadata is still loading")?;
            let uri = crate::editor::remote_uri(&remote.endpoint.host, &row.path)?;
            crate::editor::open(context, std::path::Path::new(&uri)).await?;
            UiReply {
                message: format!("Opened {} on {}", row.slug, remote.endpoint.label),
                ..Default::default()
            }
        } else {
            crate::controller_actions::execute(
                context,
                action,
                snapshot.data.as_deref(),
                &wt_github::GithubData::default(),
            )
            .await?
        };
        fleet.local.sources.metadata.refresh();
        return Ok(reply);
    }
    let migrated_pin = if let Some(remote) = remote {
        match &action {
            UiAction::SetTitle { key, .. } | UiAction::GenerateTitle { key } => {
                let ledger_key = wt_core::remote_worktree_ledger_key(&remote.endpoint.key(), key);
                let lookup = ledger_key.clone();
                let pin = context
                    .database
                    .call(move |store| {
                        let state = store.read_wt_state()?;
                        let entry = &state["slugs"][&lookup];
                        Ok(entry["manualTitle"]
                            .as_str()
                            .filter(|title| !title.trim().is_empty())
                            .map(|title| {
                                (
                                    title.to_owned(),
                                    entry["manualTitleRevision"].as_u64().unwrap_or(0),
                                )
                            }))
                    })
                    .await?;
                if let Some((title, revision)) = pin {
                    if matches!(action, UiAction::GenerateTitle { .. }) {
                        let reply = remote
                            .execute(UiAction::SetTitle {
                                key: key.clone(),
                                title,
                            })
                            .await?;
                        if reply.failed {
                            return Ok(reply);
                        }
                        clear_migrated_pin(context, &ledger_key, revision).await?;
                        fleet.local.sources.metadata.refresh();
                        None
                    } else {
                        Some((ledger_key, revision))
                    }
                } else {
                    None
                }
            }
            _ => None,
        }
    } else {
        None
    };
    let mut reply = match remote {
        Some(remote) => remote.execute(action).await?,
        None => fleet.local.execute(action).await?,
    };
    if !reply.failed
        && let Some((key, revision)) = migrated_pin
    {
        clear_migrated_pin(context, &key, revision).await?;
        fleet.local.sources.metadata.refresh();
    }
    if let Some(host) = host {
        reply.modal_host = Some(host.clone());
        if let Some(key) = &mut reply.select_when_visible {
            *key = wt_core::remote_worktree_ledger_key(&host, key);
        }
    }
    Ok(reply)
}

async fn clear_migrated_pin(context: &AppContext, key: &str, revision: u64) -> Result<()> {
    let key = key.to_owned();
    let cleared = context
        .database
        .call(move |store| Ok(store.clear_slug_manual_title(&key, revision)?))
        .await?;
    if !cleared {
        bail!(
            "The controller title changed during migration; the newer title was preserved. Retry the edit."
        );
    }
    Ok(())
}
