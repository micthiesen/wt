//! Resolve a palette action against current repository state, then dispatch it
//! to the durable headless worker or the selected live harness.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wt_actions::{
    ActionContext, ActionDestination, ActionHarness, ActionPlan, ActionPr, ActionRow,
    prepare_action, prepare_action_with_vars,
};
use wt_config::{ActionDef, HarnessId};
use wt_github::GithubData;
use wt_harness::{ClaudeMessageOutcome, CodexMessageOutcome, HarnessMessageOutcome};
use wt_tui::{ActionSurface, Board, SessionTarget};
use wt_vcs::WorktreeRecord;

use crate::context::AppContext;
use crate::harness::{
    AgentRoute, AgentTarget, AgentTargetKind, AppHarness, HarnessChoice, SelectionSource,
};

/// Dispatch an automation fire while retaining the durable-write ambiguity
/// boundary that the interactive palette's `Result<String>` API cannot express.
pub async fn dispatch_automation(
    context: &AppContext,
    fire: &wt_automations::AutomationFire,
    github: &GithubData,
    board: Option<&Board>,
) -> crate::automation_engine::AutomationOutcome {
    use crate::automation_engine::{AutomationOutcome, DeliverySlot};
    let attempt = async {
        let definition = context
            .config
            .actions
            .iter()
            .find(|definition| definition.id == fire.rule.run)
            .with_context(|| format!("automation action {:?} is not configured", fire.rule.run))?;
        let surface = if fire.frozen_vars.is_some() || fire.branch_range.is_some() {
            ActionSurface::Manager { key: None }
        } else {
            let row = context
                .repository
                .inventory(&context.cancellation)
                .await?
                .into_iter()
                .find(|row| !row.is_main && row.target.slug() == fire.slug)
                .context("automation worktree is no longer available")?;
            ActionSurface::Row {
                key: wt_core::worktree_target_key(&row.target),
            }
        };
        let mut action_context = prepare(context, &surface, github, board, None).await?;
        let mut overrides = fire.frozen_vars.clone().unwrap_or_default();
        if let Some(range) = &fire.branch_range {
            action_context.slug = "fleet".into();
            action_context.action_key = "fleet".into();
            action_context.cwd = context.config.paths.main_clone.clone();
            action_context.branch = range.branch.clone();
            action_context.base = range.from.clone();
            action_context.base_branch = range.from.clone();
            action_context.row = ActionRow::default();
            overrides.extend([
                ("slug".into(), "fleet".into()),
                ("branch".into(), range.branch.clone()),
                ("from".into(), range.from.clone()),
                ("to".into(), range.to.clone()),
            ]);
        }
        if fire.frozen_vars.is_some() {
            action_context.slug = fire.slug.clone();
            action_context.action_key = "main".into();
            action_context.cwd = context.config.paths.main_clone.clone();
            action_context.worktree_ref = None;
        }
        action_context.auto_fire_keys = fire.fire_keys.clone();
        let mut effective = definition.clone();
        if fire.frozen_vars.is_some() {
            effective.requires.clear();
        }
        if let Some(reason) =
            wt_actions::evaluate_requirements(&effective.requires, &action_context.row)
        {
            anyhow::bail!("{}: {reason}", effective.name);
        }
        let plan = prepare_action_with_vars(&effective, &action_context, "", &overrides)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("prepare automation action {}", effective.name))?;
        Ok::<_, anyhow::Error>((effective, surface, action_context, plan))
    }
    .await;
    let (definition, surface, action_context, plan) = match attempt {
        Ok(value) => value,
        Err(_error) => return AutomationOutcome::NotStarted,
    };
    match plan {
        ActionPlan::Tracked(prepared) => {
            let mut request = prepared.request;
            if let Some(path) = &context.config.repository_config {
                request
                    .config_selectors
                    .insert("WT_REPO_CONFIG".into(), path.to_string_lossy().into_owned());
            }
            match crate::actions::service(context) {
                Ok(service) => match service.start(request, &context.cancellation).await {
                    Ok(_) => AutomationOutcome::Delivered {
                        slot: DeliverySlot::Headless,
                    },
                    Err(wt_actions::ActionServiceError::StartAmbiguous { .. }) => {
                        AutomationOutcome::Ambiguous {
                            reason: "headless action start acknowledgement was lost".into(),
                            slot: DeliverySlot::Headless,
                        }
                    }
                    Err(_) => AutomationOutcome::NotStarted,
                },
                Err(_) => AutomationOutcome::NotStarted,
            }
        }
        ActionPlan::Prompt {
            destination,
            prompt,
            slug,
            ..
        } => {
            let (target, manager_prefix) = match (destination, &surface) {
                (ActionDestination::Manager, _) => (
                    "manager".to_owned(),
                    !fire.rule.run.starts_with("builtin:") && action_context.slug != "manager",
                ),
                (ActionDestination::Session, _) => (slug, false),
                (ActionDestination::Slot, ActionSurface::Slot { target }) => {
                    (slot_slug(*target).to_owned(), false)
                }
                _ => return AutomationOutcome::NotStarted,
            };
            let Ok((route_slug, cwd, branch, managed)) =
                delivery_identity(context, &surface, definition.target, &action_context)
            else {
                return AutomationOutcome::NotStarted;
            };
            let Ok(route) = delivery_route(context, route_slug, cwd, branch, managed).await else {
                return AutomationOutcome::NotStarted;
            };
            if route.target.remote || route.choice.source == SelectionSource::RemoteUnavailable {
                return AutomationOutcome::NotStarted;
            }
            let text = if manager_prefix {
                format!("[re: {}] {prompt}", fire.slug)
            } else {
                prompt
            };
            match AppHarness::new(context)
                .send(&route, &text, None, context)
                .await
            {
                Ok(outcome) => match outcome {
                    HarnessMessageOutcome::Claude(ClaudeMessageOutcome::Sent {
                        delivered: Some(true),
                        ..
                    })
                    | HarnessMessageOutcome::Codex(CodexMessageOutcome::Queued(_))
                    | HarnessMessageOutcome::Codex(CodexMessageOutcome::CliQueued { .. })
                    | HarnessMessageOutcome::Codex(CodexMessageOutcome::Terminal {
                        delivered: Some(true),
                        ..
                    })
                    | HarnessMessageOutcome::OpenCode(_) => AutomationOutcome::Delivered {
                        slot: if target == "manager" {
                            DeliverySlot::Manager
                        } else {
                            DeliverySlot::Session
                        },
                    },
                    HarnessMessageOutcome::Claude(ClaudeMessageOutcome::Sent {
                        delivered: Some(false),
                        ..
                    })
                    | HarnessMessageOutcome::Claude(ClaudeMessageOutcome::Sent {
                        delivered: None,
                        ..
                    })
                    | HarnessMessageOutcome::Claude(ClaudeMessageOutcome::Failed {
                        maybe_submitted: true,
                        ..
                    })
                    | HarnessMessageOutcome::Codex(CodexMessageOutcome::Terminal {
                        delivered: None | Some(false),
                        ..
                    })
                    | HarnessMessageOutcome::Codex(CodexMessageOutcome::Ambiguous { .. }) => {
                        AutomationOutcome::Ambiguous {
                            reason: "prompt may have been submitted; do not replay automatically"
                                .into(),
                            slot: if target == "manager" {
                                DeliverySlot::Manager
                            } else {
                                DeliverySlot::Session
                            },
                        }
                    }
                    HarnessMessageOutcome::Claude(ClaudeMessageOutcome::Failed {
                        maybe_submitted: false,
                        ..
                    })
                    | HarnessMessageOutcome::Codex(CodexMessageOutcome::Failed { .. })
                    | HarnessMessageOutcome::Codex(CodexMessageOutcome::NeedsTerminalFallback {
                        ..
                    }) => AutomationOutcome::NotStarted,
                },
                Err(_) => AutomationOutcome::Ambiguous {
                    reason: "harness send failed after entering the delivery path".into(),
                    slot: if target == "manager" {
                        DeliverySlot::Manager
                    } else {
                        DeliverySlot::Session
                    },
                },
            }
        }
    }
}

pub fn definitions(context: &AppContext, surface: &ActionSurface) -> Vec<ActionDef> {
    crate::action_builtins::definitions(&context.config, surface)
}

pub async fn prepare(
    context: &AppContext,
    surface: &ActionSurface,
    github: &GithubData,
    board: Option<&Board>,
    arg: Option<String>,
) -> Result<ActionContext> {
    let (target, key, slot) = match surface {
        ActionSurface::Row { key } => (None, Some(key.as_str()), None),
        ActionSurface::Manager { key } => (Some("manager"), key.as_deref(), None),
        ActionSurface::Slot { target } => (None, None, Some(*target)),
    };
    let row_snapshot = if let Some(key) = key {
        Some(resolve_row(context, key).await?)
    } else {
        None
    };
    let (slug, cwd, branch, stage, worktree_ref, action_key) = if let Some(snapshot) = &row_snapshot
    {
        let target = &snapshot.target;
        if matches!(target.location(), wt_core::WorktreeLocation::Remote { .. }) {
            bail!(
                "actions for remote worktrees must be sent through the worker runtime; local dispatch is refused"
            );
        }
        (
            target.slug().to_owned(),
            PathBuf::from(&target.path),
            target.branch.clone(),
            target.stage.clone(),
            Some(target.reference().clone()),
            wt_core::worktree_action_key(target),
        )
    } else if let Some(slot) = slot {
        resolve_slot(context, slot)?
    } else {
        let slug = target.unwrap_or("manager").to_owned();
        (
            slug.clone(),
            context.config.paths.main_clone.clone(),
            String::new(),
            String::new(),
            None,
            slug,
        )
    };
    let persisted = context
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let state_entry = persisted.get("slugs").and_then(|all| all.get(&slug));
    let base_branch = state_entry
        .and_then(|entry| entry.get("baseBranch"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&context.config.branch.base)
        .to_owned();
    let issue_override = state_entry
        .and_then(|entry| entry.get("issueId"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let issue_id =
        crate::issue_identity::resolve(&slug, issue_override.as_deref()).unwrap_or_default();
    let pr = row_snapshot
        .as_ref()
        .and_then(|snapshot| github.prs.get(&snapshot.target.branch));
    let pr_number = pr.map(|pr| pr.number.to_string());
    let board_row = board.and_then(|board| board.rows.iter().find(|row| row.slug == slug));
    let deployed = if let Some(snapshot) = &row_snapshot {
        let path = PathBuf::from(&snapshot.target.path);
        let prefix = context.config.stage.prefix.clone();
        tokio::task::spawn_blocking(move || {
            matches!(
                wt_sst::observe_local_deployment(&path, &prefix),
                wt_sst::DeploymentObservation::Deployed { .. }
            )
        })
        .await
        .context("inspect local deployment facts")?
    } else {
        false
    };
    let mut row = ActionRow {
        slug: slug.clone(),
        issue_id: issue_override.or_else(|| (!issue_id.is_empty()).then_some(issue_id.clone())),
        issue_prefix: context
            .config
            .issue_tracker
            .as_ref()
            .and_then(|issue| issue.prefix.clone()),
        pr: pr.map(|pr| ActionPr {
            state: pr.state.clone(),
            is_draft: pr.is_draft,
        }),
        pr_number,
        deployed,
    };
    // The rendered board's issue id is a presentation value; persisted explicit
    // empty string remains an explicit clear and takes precedence.
    if state_entry.and_then(|entry| entry.get("issueId")).is_none() {
        row.issue_id = board_row
            .and_then(|row| row.issue_id.clone())
            .or(row.issue_id);
    }
    let issue_id = row.issue_id.as_deref().unwrap_or_default().to_owned();
    let primary = AppHarness::new(context).primary();
    let harness = match primary {
        HarnessId::Claude => ActionHarness::Claude,
        HarnessId::Codex => ActionHarness::Codex,
        HarnessId::Opencode => ActionHarness::OpenCode,
    };
    let skill_prefix = match primary {
        HarnessId::Claude => "/",
        HarnessId::Codex | HarnessId::Opencode => "$",
    };
    Ok(ActionContext {
        action_key,
        slug,
        cwd,
        worktree_ref,
        base: base_branch.clone(),
        base_branch,
        branch,
        issue_id,
        stage,
        skill_prefix: skill_prefix.into(),
        arg,
        shell: std::env::var("SHELL").ok(),
        today: Some(today()),
        row,
        harness,
        auto_fire_keys: Vec::new(),
    })
}

// Keep the palette's explicit immutable inputs visible at this boundary;
// wrapping them in a second DTO would obscure the UI contract.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch(
    context: &AppContext,
    surface: &ActionSurface,
    definition: &ActionDef,
    github: &GithubData,
    board: Option<&Board>,
    arg: Option<String>,
    extras: &str,
    auto_fire_keys: Vec<String>,
) -> Result<String> {
    let current_definition = definitions(context, surface)
        .into_iter()
        .find(|current| current.id == definition.id)
        .with_context(|| format!("action {:?} is no longer available", definition.id))?;
    let definition = &current_definition;
    let mut action_context = prepare(context, surface, github, board, arg).await?;
    if matches!(surface, ActionSurface::Manager { .. })
        && crate::action_builtins::is_fleet(&definition.id)
    {
        action_context.action_key = "manager".into();
        action_context.slug = "manager".into();
        action_context.cwd = context.config.paths.main_clone.clone();
        action_context.worktree_ref = None;
        action_context.branch.clear();
        action_context.stage.clear();
        action_context.issue_id.clear();
        action_context.row = ActionRow::default();
    }
    action_context.auto_fire_keys = auto_fire_keys;
    if let Some(reason) =
        wt_actions::evaluate_requirements(&definition.requires, &action_context.row)
    {
        bail!("{}: {reason}", definition.name);
    }
    let delivery_route = if definition.kind == wt_config::ActionKind::Claude
        && definition.target != wt_config::ActionTarget::Headless
    {
        let (slug, cwd, branch, managed_name) =
            delivery_identity(context, surface, definition.target, &action_context)?;
        let route = delivery_route(context, slug, cwd, branch, managed_name).await?;
        let selected = route
            .choice
            .selected
            .context("no harness selected for action target")?;
        action_context.harness = harness_kind(selected);
        action_context.skill_prefix = skill_prefix(selected).into();
        Some(route)
    } else {
        None
    };
    let plan = prepare_action(definition, &action_context, extras)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("prepare action {}", definition.name))?;
    match plan {
        ActionPlan::Tracked(prepared) => {
            let mut request = prepared.request;
            if let Some(path) = &context.config.repository_config {
                request
                    .config_selectors
                    .insert("WT_REPO_CONFIG".into(), path.to_string_lossy().into_owned());
            }
            let start = crate::actions::service(context)?
                .start(request, &context.cancellation)
                .await
                .context("start tracked action")?;
            Ok(format!("started {} ({})", definition.name, start.session))
        }
        ActionPlan::Prompt {
            destination,
            prompt,
            slug,
            ..
        } => {
            let (route_slug, manager_row_prefix) = match (destination, surface) {
                (ActionDestination::Session, _) => (slug, false),
                (ActionDestination::Manager, _) => (
                    "manager".into(),
                    !crate::action_builtins::is_fleet(&definition.id)
                        && action_context.slug != "manager",
                ),
                (ActionDestination::Slot, ActionSurface::Slot { target }) => {
                    (slot_slug(*target).to_owned(), false)
                }
                (ActionDestination::Slot, _) => bail!("slot action requires a slot surface"),
                (ActionDestination::Headless, _) => {
                    bail!("headless action did not produce a tracked plan")
                }
            };
            let mut text = prompt;
            if manager_row_prefix {
                text = format!("[re: {}] {text}", action_context.slug);
            }
            let route = delivery_route
                .with_context(|| format!("no live target for action slot {route_slug:?}"))?;
            if route.target.remote || route.choice.source == SelectionSource::RemoteUnavailable {
                bail!("action target {route_slug:?} is remote and cannot be delivered locally");
            }
            let outcome = AppHarness::new(context)
                .send(&route, &text, None, context)
                .await
                .with_context(|| format!("deliver {}", definition.name))?;
            report_delivery(&definition.name, &route_slug, outcome)
        }
    }
}

async fn resolve_row(context: &AppContext, key: &str) -> Result<WorktreeRecord> {
    context
        .repository
        .inventory(&context.cancellation)
        .await?
        .into_iter()
        .find(|record| wt_core::worktree_target_key(&record.target) == key)
        .with_context(|| format!("selected worktree no longer exists: {key}"))
}

fn delivery_identity(
    app_context: &AppContext,
    surface: &ActionSurface,
    target: wt_config::ActionTarget,
    context: &ActionContext,
) -> Result<(String, PathBuf, Option<String>, Option<String>)> {
    match target {
        wt_config::ActionTarget::Session => match surface {
            ActionSurface::Row { .. } => Ok((
                context.slug.clone(),
                context.cwd.clone(),
                Some(context.branch.clone()),
                None,
            )),
            _ => bail!("session action requires a captured worktree row"),
        },
        wt_config::ActionTarget::Manager => Ok((
            "manager".into(),
            app_context.config.paths.main_clone.clone(),
            None,
            Some("manager".into()),
        )),
        wt_config::ActionTarget::Slot => {
            if let ActionSurface::Slot { target } = surface {
                let slug = slot_slug(*target);
                if slug.is_empty() {
                    bail!("worktree action slots require a captured worktree row");
                }
                let path = match target {
                    SessionTarget::Manager | SessionTarget::Main => context.cwd.clone(),
                    SessionTarget::WtSource => context.cwd.clone(),
                    SessionTarget::Dotfiles => context.cwd.clone(),
                    _ => unreachable!("slot_slug rejects worktree targets"),
                };
                let managed = (*target == SessionTarget::Manager).then(|| "manager".into());
                Ok((slug.into(), path, None, managed))
            } else {
                bail!("slot action requires a slot surface")
            }
        }
        wt_config::ActionTarget::Headless => {
            bail!("headless actions do not have a prompt delivery target")
        }
    }
}

async fn delivery_route(
    context: &AppContext,
    slug: String,
    cwd: PathBuf,
    branch: Option<String>,
    managed_name: Option<String>,
) -> Result<AgentRoute> {
    let app = AppHarness::new(context);
    let sessions = app
        .session_inventory(context)
        .await
        .context("inspect harness target sessions")?;
    let known_slugs = context
        .repository
        .inventory(&context.cancellation)
        .await?
        .into_iter()
        .map(|record| record.target.slug().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    let primary = app.primary();
    let mut live = Vec::new();
    for id in HarnessId::ALL {
        let name = match id {
            HarnessId::Claude if managed_name.as_deref() == Some("manager") => {
                format!("{slug}~manager")
            }
            HarnessId::Claude => slug.clone(),
            HarnessId::Codex => format!("{slug}-codex"),
            HarnessId::Opencode => format!("{slug}-opencode"),
        };
        if id == HarnessId::Codex && name != slug && known_slugs.contains(&name) {
            continue;
        }
        if sessions.iter().any(|session| session.name == name)
            || (id == HarnessId::Claude
                && sessions
                    .iter()
                    .any(|session| session.name.starts_with(&format!("{slug}~"))))
        {
            live.push(id);
        }
    }
    let selected = if live.contains(&primary) {
        primary
    } else {
        live.first().copied().unwrap_or(primary)
    };
    Ok(AgentRoute {
        target: AgentTarget {
            slug,
            kind: if managed_name.is_some() || branch.is_none() {
                AgentTargetKind::Special
            } else {
                AgentTargetKind::Worktree
            },
            branch,
            cwd,
            managed_name,
            remote: false,
        },
        choice: HarnessChoice {
            selected: Some(selected),
            source: if live.is_empty() {
                SelectionSource::Primary
            } else {
                SelectionSource::Live
            },
            live: Some(live),
        },
    })
}

fn harness_kind(id: HarnessId) -> ActionHarness {
    match id {
        HarnessId::Claude => ActionHarness::Claude,
        HarnessId::Codex => ActionHarness::Codex,
        HarnessId::Opencode => ActionHarness::OpenCode,
    }
}

fn skill_prefix(id: HarnessId) -> &'static str {
    match id {
        HarnessId::Claude => "/",
        HarnessId::Codex | HarnessId::Opencode => "$",
    }
}

fn report_delivery(name: &str, target: &str, outcome: HarnessMessageOutcome) -> Result<String> {
    match outcome {
        HarnessMessageOutcome::Claude(ClaudeMessageOutcome::Sent {
            cold_started,
            delivered,
            ..
        }) => match delivered {
            Some(false) => bail!(
                "{name} reached the {target} harness adapter but was not observed in the session; inspect the session before retrying"
            ),
            None => Ok(format!(
                "{name} submitted to {target}{}; arrival is unconfirmed",
                if cold_started {
                    " (session started)"
                } else {
                    ""
                }
            )),
            Some(true) => Ok(format!(
                "{name} delivered to {target}{}",
                if cold_started {
                    " (session started)"
                } else {
                    ""
                }
            )),
        },
        HarnessMessageOutcome::Claude(ClaudeMessageOutcome::Failed {
            reason,
            maybe_submitted,
        }) => {
            if maybe_submitted {
                bail!(
                    "{name} delivery to {target} is ambiguous: {reason}; do not retry automatically"
                );
            }
            bail!("{name} delivery to {target} failed: {reason}");
        }
        HarnessMessageOutcome::Codex(CodexMessageOutcome::Queued(delivery)) => Ok(format!(
            "{name} queued to {target}{}",
            if delivery.reconciled {
                " (reconciled after a lost reply)"
            } else {
                ""
            }
        )),
        HarnessMessageOutcome::Codex(CodexMessageOutcome::CliQueued { .. }) => {
            Ok(format!("{name} queued to {target}"))
        }
        HarnessMessageOutcome::Codex(CodexMessageOutcome::Terminal {
            cold_started,
            delivered,
            reason,
        }) => match delivered {
            Some(false) => bail!("{name} terminal delivery to {target} was not observed: {reason}"),
            None => Ok(format!(
                "{name} submitted to {target}{}; arrival is unconfirmed",
                if cold_started {
                    " (session started)"
                } else {
                    ""
                }
            )),
            Some(true) => Ok(format!(
                "{name} delivered to {target}{}",
                if cold_started {
                    " (session started)"
                } else {
                    ""
                }
            )),
        },
        HarnessMessageOutcome::Codex(CodexMessageOutcome::NeedsTerminalFallback { reason }) => {
            bail!(
                "{name} was not delivered to {target}: {reason}; this action cannot take over an interactive terminal automatically"
            )
        }
        HarnessMessageOutcome::Codex(CodexMessageOutcome::Ambiguous { reason }) => {
            bail!("{name} delivery to {target} is ambiguous: {reason}; do not retry automatically")
        }
        HarnessMessageOutcome::Codex(CodexMessageOutcome::Failed { reason }) => {
            bail!("{name} delivery to {target} failed: {reason}")
        }
        HarnessMessageOutcome::OpenCode(outcome) => Ok(format!(
            "{name} sent to {target}{}",
            if outcome.cold_started {
                " (session started)"
            } else {
                ""
            }
        )),
    }
}

fn resolve_slot(
    context: &AppContext,
    target: SessionTarget,
) -> Result<(
    String,
    PathBuf,
    String,
    String,
    Option<wt_core::WorktreeRef>,
    String,
)> {
    let (slug, path) = match target {
        SessionTarget::Manager => ("manager", context.config.paths.main_clone.clone()),
        SessionTarget::Main => ("main", context.config.paths.main_clone.clone()),
        SessionTarget::Dotfiles => ("dotfiles", context.config.paths.dotfiles.clone()),
        SessionTarget::WtSource => {
            let path = context
                .config
                .paths
                .wt_source
                .as_ref()
                .filter(|path| path.is_dir())
                .context(crate::harness::unavailable_source_message())?;
            ("wt", path.clone())
        }
        SessionTarget::Harness | SessionTarget::Shell | SessionTarget::Diff => {
            bail!("worktree action slots require a captured worktree row")
        }
    };
    if !path.is_dir() {
        bail!("action slot {slug:?} is unavailable at {}", path.display());
    }
    Ok((
        slug.into(),
        path,
        String::new(),
        String::new(),
        None,
        slug.into(),
    ))
}

fn slot_slug(target: SessionTarget) -> &'static str {
    match target {
        SessionTarget::Manager => "manager",
        SessionTarget::Main => "main",
        SessionTarget::WtSource => "wt",
        SessionTarget::Dotfiles => "dotfiles",
        _ => "",
    }
}

fn today() -> String {
    OffsetDateTime::now_utc()
        .format(&time::macros::format_description!(
            "[weekday repr:long], [month repr:long] [day], [year]"
        ))
        .unwrap_or_else(|_| {
            OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_else(|_| "unknown date".into())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::CommandFixture;

    #[test]
    fn manager_fleet_and_direct_actions_are_explicit() {
        assert!(crate::action_builtins::is_fleet("manager-digest"));
        assert!(!crate::action_builtins::is_fleet("manager-ask-row"));
        assert!(crate::action_builtins::is_direct("manager-compact"));
        assert!(!crate::action_builtins::is_direct("manager-digest"));
    }

    #[test]
    fn prompt_delivery_reports_ambiguity_without_retrying() {
        let result = report_delivery(
            "manager digest",
            "manager",
            HarnessMessageOutcome::Codex(CodexMessageOutcome::Ambiguous {
                reason: "queue reply lost".into(),
            }),
        );
        let error = result.unwrap_err().to_string();
        assert!(error.contains("ambiguous"));
        assert!(error.contains("do not retry automatically"));
    }

    #[tokio::test]
    async fn prepare_resolves_current_row_without_git_status_scans() {
        let fixture = CommandFixture::new().await.unwrap();
        let records = fixture
            .ctx
            .repository
            .inventory(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let record = records
            .iter()
            .find(|record| record.target.slug() == "one")
            .unwrap();
        let key = wt_core::worktree_target_key(&record.target);
        let prepared = prepare(
            &fixture.ctx,
            &ActionSurface::Row { key },
            &GithubData::default(),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(prepared.slug, "one");
        assert_eq!(prepared.branch, "feature/one");
        assert_eq!(
            prepared.worktree_ref,
            Some(record.target.reference().clone())
        );
        assert_eq!(prepared.issue_id, "");
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn prepare_rejects_a_stale_canonical_worktree_key() {
        let fixture = CommandFixture::new().await.unwrap();
        let error = prepare(
            &fixture.ctx,
            &ActionSurface::Row {
                key: "missing-key".into(),
            },
            &GithubData::default(),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("selected worktree no longer exists")
        );
        fixture.close().await.unwrap();
    }
}
