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
    replies: &crate::controller::RequestReplies,
    shutdown: &CancellationToken,
) -> Result<UiReply> {
    let command = rename_as_title(command);
    // Cleanup is a fleet-wide confirmation: prepare the candidate set from
    // every configured host, then route the captured revisions back to their
    // owners. Explicitly host-scoped legacy requests still use the normal path.
    if !matches!(command, UiAction::OnHost { .. }) {
        match command {
            UiAction::PrepareCleanup => return Ok(crate::fleet_cleanup::prepare(fleet).await),
            UiAction::CleanupTargets { targets } => {
                return Ok(crate::fleet_cleanup::cleanup_targets(fleet, targets).await);
            }
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
        let effect = action.effect(&wt_tui::Board::default());
        let result = fleet
            .local
            .context
            .database
            .call(move |store| {
                store.set_attention_seen(at_ms)?;
                Ok(())
            })
            .await
            .map(|()| {
                crate::host_service::complete_local(
                    &effect,
                    UiReply {
                        message: "Attention marked seen".into(),
                        ..Default::default()
                    },
                )
            });
        fleet.local.sources.metadata.refresh();
        return result;
    }
    if let UiAction::PerfInvestigate { report } = action {
        if report.iter().all(|line| line.trim().is_empty()) {
            return Ok(UiReply {
                message: "No perf sample yet".into(),
                ..Default::default()
            });
        }
        if !context
            .config
            .paths
            .wt_source
            .as_ref()
            .is_some_and(|path| path.is_dir())
        {
            return Ok(UiReply {
                message: "Investigate needs a wt checkout: set paths.wt_source".into(),
                failed: true,
                ..Default::default()
            });
        }
        let sent = fleet
            .local
            .execute(UiAction::RunAction {
                surface: wt_tui::ActionSurface::Slot {
                    target: SessionTarget::WtSource,
                },
                id: crate::action_palette::CUSTOM_ID.into(),
                arg: None,
                extras: perf_investigation_prompt(&report),
            })
            .await
            .context("send the perf snapshot to the wt session")?;
        if sent.failed {
            return Ok(sent);
        }
        let prepared = crate::harness::ui_session(context, None, SessionTarget::WtSource).await?;
        return crate::controller::handoff_prepared(prepared, replies, shutdown).await;
    }
    // Only Ctrl+N (PrepareCreate without a captured host) asks for a host.
    // Like the TS TUI, `n`, hard refresh, and the global automation keys act
    // on this machine; cleanup is fleet-wide above.
    if !explicit && !fleet.remotes.is_empty() && matches!(action, UiAction::PrepareCreate { .. }) {
        let mut options = vec![PickerOption {
            value: None,
            label: "This machine".into(),
            chord: None,
            note: None,
            verify_after_merge: None,
            detail: None,
        }];
        options.extend(fleet.remotes.iter().map(|remote| {
            let owed = remote
                .managed
                .borrow()
                .as_ref()
                .map(|status| status.report.sign_ins())
                .unwrap_or_default();
            PickerOption {
                value: Some(remote.endpoint.key()),
                label: remote.endpoint.label.clone(),
                chord: None,
                note: (!owed.is_empty()).then(|| {
                    format!(
                        "sign-in needed: {}",
                        owed.iter()
                            .map(|provider| provider.label())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }),
                verify_after_merge: None,
                detail: None,
            }
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
    if matches!(action, UiAction::PrepareSessions { .. }) {
        let location = remote
            .map(|remote| remote.endpoint.label.as_str())
            .unwrap_or("this machine");
        replies
            .send(UiReply::message(format!("Loading sessions on {location}…")))
            .await
            .context("show session discovery progress")?;
    }
    if let UiAction::ManagedSignIn { provider, then } = action {
        let remote = remote.context("sign-in belongs to a managed remote host")?;
        return Box::pin(managed_sign_in(
            context, fleet, remote, &provider, then, replies, shutdown,
        ))
        .await;
    }
    // A create submitted during initial provisioning has not been sent yet.
    // Keep its placeholder and wait for the connection instead of immediately
    // bouncing the user back into the input prompt. No sent command is retried.
    if let Some(remote) = remote
        && !remote.is_ready()
    {
        replies
            .send(UiReply::message(format!(
                "Connecting to {}…",
                remote.endpoint.label
            )))
            .await
            .context("show remote connection progress")?;
        remote.wake();
        let ready = tokio::select! {
            _ = shutdown.cancelled() => bail!("remote connection cancelled; no command was sent"),
            ready = remote.wait_for_action(&action, context.config.harness.primary, std::time::Duration::from_secs(90)) => ready,
        };
        if !ready {
            bail!(
                "{} is not ready; no command was sent: {}",
                remote.endpoint.label,
                remote.connection_detail()
            );
        }
    }
    if let Some(remote) = remote
        && remote.endpoint.managed
        && crate::managed::required_provider(&action, context.config.harness.primary)
            == Some(wt_remote::managed::SignIn::Codex)
    {
        let desired = crate::codex_reauth::controller_generation(
            &crate::codex_reauth::controller_home(&context.home),
        )?;
        let observed = remote
            .managed
            .borrow()
            .as_ref()
            .and_then(|status| status.report.codex_auth_generation);
        let pending_codex = remote.managed.borrow().as_ref().is_some_and(|status| {
            crate::managed::required_sign_in(
                &status.report,
                &action,
                context.config.harness.primary,
            ) == Some(wt_remote::managed::SignIn::Codex)
        });
        if (desired.is_some() && desired != observed) || pending_codex {
            // An account change must invalidate sign-in readiness even while
            // the worker stream remains connected. This is action preparation,
            // never filesystem work on the terminal input path.
            let client = remote.staged_client()?;
            let report = crate::managed::provision(context, &client).await?;
            remote.set_managed(report);
        }
    }
    if let Some(remote) = remote
        && let Some(status) = remote.managed.borrow().clone()
        && let Some(provider) = crate::managed::required_sign_in(
            &status.report,
            &action,
            context.config.harness.primary,
        )
    {
        return Ok(UiReply {
            modal: Some(crate::managed::sign_in_modal(
                &remote.endpoint.label,
                provider,
                action,
            )),
            modal_host: host,
            ..Default::default()
        });
    }
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
    if let UiAction::EnterHarness { key, harness } = action {
        let prepared = match (remote, harness) {
            (None, wt_core::HarnessId::Claude) => {
                crate::harness::prepare_session(context, &new_claude_selection(key)).await?
            }
            (None, harness) => {
                crate::harness::ui_session_with_harness(
                    context,
                    Some(key),
                    SessionTarget::Harness,
                    Some(harness),
                )
                .await?
            }
            (Some(remote), harness) => {
                let client = remote.session_client().await?;
                let prepared = if harness == wt_core::HarnessId::Claude {
                    client.interactive_selected_session(&serde_json::to_string(
                        &new_claude_selection(key),
                    )?)
                } else {
                    client.interactive_session(&key, "harness", Some(harness.as_str()))
                };
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
        let empty = wt_tui::Board::default();
        let effect = action.effect(snapshot.data().map_or(&empty, |board| board.as_ref()));
        let result = async {
            if let UiAction::OpenEditor { key } = &action
                && let Some(remote) = remote
            {
                let row = snapshot
                    .data()
                    .and_then(|board| board.rows.iter().find(|row| row.key == *key))
                    .context("remote worktree metadata is still loading")?;
                let uri = crate::editor::remote_uri(&remote.endpoint.host, &row.path)?;
                crate::editor::open(context, std::path::Path::new(&uri)).await?;
                Ok(UiReply {
                    message: format!("Opened {} on {}", row.slug, remote.endpoint.label),
                    ..Default::default()
                })
            } else {
                crate::controller_actions::execute(
                    context,
                    action,
                    snapshot.data().map(|board| board.as_ref()),
                    // The controller owns no GitHub leaf; these actions act on
                    // local presentation and durable state only.
                    None,
                )
                .await
            }
        }
        .await
        .map(|reply| crate::host_service::complete_local(&effect, reply));
        // A failed local write may have partially changed durable metadata.
        fleet.local.sources.metadata.refresh();
        return result;
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
    let action = if let UiAction::Create { input } = action {
        // Resolving identity has not accepted a filesystem mutation. It must
        // remain cancellable even though the outer user request is Create.
        let preparation = async {
            match remote {
                Some(remote) => remote
                    .execute(UiAction::ResolveCreation { input })
                    .await
                    .map_err(anyhow::Error::from),
                None => {
                    fleet
                        .local
                        .execute(UiAction::ResolveCreation { input })
                        .await
                }
            }
        };
        let prepared = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(UiReply::no_change("Creation cancelled before checkout started")),
            prepared = preparation => prepared?,
        };
        if prepared.failed {
            return Ok(prepared);
        }
        let plan = prepared
            .creation_plan
            .context("host did not resolve creation identity")?;
        let key = host.as_ref().map_or_else(
            || plan.key.clone(),
            |host| wt_core::remote_worktree_ledger_key(host, &plan.key),
        );
        if !replies.prepare_creation(key, shutdown).await? {
            return Ok(UiReply::no_change(
                "Creation cancelled before checkout started",
            ));
        }
        UiAction::CreateResolved { plan }
    } else {
        action
    };
    let mut reply = match remote {
        Some(remote) => remote.execute(action).await?,
        None => {
            fleet
                .local
                .execute_with_shutdown(action, Some(shutdown))
                .await?
        }
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

/// Run a sign-in on a managed worker in the terminal, re-check the worker, and
/// continue the request that needed the sign-in once it succeeded.
async fn managed_sign_in(
    context: &AppContext,
    fleet: &Fleet,
    remote: &crate::remote_host::RemoteHost,
    provider: &str,
    then: Option<Box<UiAction>>,
    replies: &crate::controller::RequestReplies,
    shutdown: &CancellationToken,
) -> Result<UiReply> {
    let provider = wt_remote::managed::SignIn::parse(provider)
        .with_context(|| format!("unknown sign-in {provider:?}"))?;
    let client = remote.staged_client()?;
    let session = client.interactive_managed(
        &["login".into(), provider.as_str().into()],
        provider.forward_port(),
    );
    let handoff = crate::controller::handoff_prepared(
        crate::harness::PreparedSession {
            program: session.program,
            args: session.args,
            cwd: context.config.paths.main_clone.clone(),
        },
        replies,
        shutdown,
    )
    .await;
    if matches!(&handoff, Ok(reply) if reply.outcome == Some(wt_tui::ReplyOutcome::NoChange)) {
        return handoff;
    }
    let label = &remote.endpoint.label;
    let report = crate::managed::provision(context, &client)
        .await
        .with_context(|| format!("re-check {label} after sign-in"))?;
    let still_needed = report.sign_ins().contains(&provider);
    remote.set_managed(report);
    // A worker blocked on its clone retries now instead of after backoff.
    remote.wake();
    if still_needed {
        if let Err(error) = handoff {
            tracing::info!(host = %label, error = %format!("{error:#}"), "managed sign-in session ended unsuccessfully");
        }
        return Ok(UiReply {
            message: format!("{} sign-in on {label} did not complete", provider.label()),
            failed: true,
            ..Default::default()
        });
    }
    let Some(then) = then else {
        return Ok(UiReply {
            message: format!("Signed in to {} on {label}", provider.label()),
            ..Default::default()
        });
    };
    if !remote.wait_ready(std::time::Duration::from_secs(90)).await {
        return Ok(UiReply {
            message: format!(
                "Signed in to {} on {label}; the host is still connecting, so retry the request",
                provider.label()
            ),
            ..Default::default()
        });
    }
    Box::pin(execute(
        context,
        fleet,
        UiAction::OnHost {
            host: Some(remote.endpoint.key()),
            action: then,
        },
        replies,
        shutdown,
    ))
    .await
}

/// The row palette's `t` entry is the same request as `T`, so it shares the
/// title path, including remote pin migration.
fn rename_as_title(action: UiAction) -> UiAction {
    match action {
        UiAction::OnHost { host, action } => UiAction::OnHost {
            host,
            action: Box::new(rename_as_title(*action)),
        },
        UiAction::PrepareAction {
            surface: wt_tui::ActionSurface::Row { key },
            id,
            ..
        } if id == crate::action_builtins::RENAME_ID => UiAction::GenerateTitle { key },
        action => action,
    }
}

/// A fresh Claude session; the host picks the next automatic name.
fn new_claude_selection(key: String) -> wt_tui::SessionSelection {
    wt_tui::SessionSelection {
        key: Some(key),
        target: SessionTarget::Harness,
        harness: wt_core::HarnessId::Claude,
        session_id: None,
        live_slot: None,
        managed_name: None,
        mode: wt_tui::SessionMode::New,
        live: false,
    }
}

/// The TS perf overlay's investigation prompt followed by the shown report.
fn perf_investigation_prompt(report: &[String]) -> String {
    [
        "My machine feels slow and I captured a perf snapshot from wt's `P`",
        "overlay. Work out whether the load is reasonable for what's actually",
        "running, and if it isn't, what to do about it.",
        "",
        "Compare wt-downstream load with the machine total. Downstream describes",
        "process ancestry, not task ownership: tools launched by an external agent",
        "daemon may appear outside wt. Recheck current PIDs, working directories,",
        "elapsed and CPU time before attributing overlapping checks. Low RSS or",
        "cumulative swap counters alone do not establish memory pressure or its",
        "cause; use interval measurements. CPU percentages are averages.",
        "",
        "Do not kill any processes without asking me first.",
        "",
        "---",
        "",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain(report.iter().cloned())
    .collect::<Vec<_>>()
    .join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_prompt_carries_the_guardrail_and_the_report() {
        let prompt = perf_investigation_prompt(&["cpu 90%".into(), "rss 1G".into()]);
        assert!(prompt.contains("Do not kill any processes without asking me first."));
        assert!(prompt.ends_with("---\n\ncpu 90%\nrss 1G"));
    }
}
