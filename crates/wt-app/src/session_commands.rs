//! Exact session selection and lifecycle operations shared by local UI actions
//! and the remote `_session` host command.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use wt_core::HarnessId;
use wt_harness::{DiscoveryRequest, HarnessSession, HarnessSpawnRequest};
use wt_platform::lock::FileLock;
use wt_tmux::PaneTarget;
use wt_tui::{LiveSessionIdentity, SessionMode, SessionSelection, SessionTarget};

use crate::{
    context::AppContext,
    harness::{AgentTarget, AppHarness, PreparedSession},
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOption {
    pub selection: SessionSelection,
    pub display_name: String,
    pub tmux_session_name: String,
    pub last_active_ms: Option<i64>,
    pub is_live: bool,
    pub state: Option<String>,
    pub waiting_for: Option<String>,
}

/// Discover resumable conversations for the exact selected target. Liveness is
/// joined from one tmux inventory snapshot; a failed inventory fails closed.
pub async fn list_session_options(
    context: &AppContext,
    key: Option<String>,
    target: SessionTarget,
) -> Result<Vec<SessionOption>> {
    let app = AppHarness::new(context);
    let agent = resolve_target(context, key.as_deref(), target).await?;
    if agent.remote {
        bail!("session discovery must run on the worktree host");
    }
    if !matches!(
        target,
        SessionTarget::Harness
            | SessionTarget::Manager
            | SessionTarget::Main
            | SessionTarget::WtSource
            | SessionTarget::Dotfiles
    ) {
        bail!("session picker is only available for harness targets");
    }
    let tmux = app.session_inventory(context).await?;
    let live_names = tmux
        .iter()
        .map(|session| session.name.clone())
        .collect::<Vec<_>>();
    let mut options = Vec::new();
    for harness in HarnessId::ALL {
        if context.config.harness.hidden.contains(&harness) {
            continue;
        }
        let tmux_name = session_name(&agent, harness);
        let live_session_id = tmux
            .iter()
            .find(|session| session.name == tmux_name)
            .and_then(|session| session.harness_session_id.clone());
        let discovered = app
            .service
            .discover(
                harness,
                &DiscoveryRequest {
                    slug: agent.slug.clone(),
                    worktree_path: agent.cwd.clone(),
                    live_session_id,
                },
                &live_names,
                &context.cancellation,
            )
            .await
            .with_context(|| format!("discover {harness:?} sessions for {}", agent.slug))?;
        let mut slots = tmux.clone();
        if harness == HarnessId::Codex
            && let Some(slot) = slots.iter_mut().find(|slot| slot.name == tmux_name)
            && slot.harness_session_id.is_none()
        {
            slot.harness_session_id = app
                .service
                .recover_codex_live_identity(&tmux_name, &discovered, &context.cancellation)
                .await?;
        }
        for session in discovered {
            let live = slots.iter().find(|live| {
                live.name == session.tmux_session_name
                    && slot_matches_selection(harness, live, &session.session_id)
            });
            options.push(option_for(target, key.clone(), harness, session, live));
        }
        if harness != HarnessId::Claude
            && let Some(slot) = slots.iter().find(|slot| slot.name == tmux_name)
            && !options
                .iter()
                .any(|option| option.selection.harness == harness && option.is_live)
        {
            options.push(live_slot_option(target, key.clone(), harness, slot));
        }
    }
    options.sort_by(|left, right| {
        right
            .is_live
            .cmp(&left.is_live)
            .then_with(|| left.selection.harness.cmp(&right.selection.harness))
            .then_with(|| right.last_active_ms.cmp(&left.last_active_ms))
            .then_with(|| left.display_name.cmp(&right.display_name))
    });
    Ok(options)
}

/// Prepare the exact selected conversation for terminal handoff. It never
/// replaces a live slot; the caller must first obtain explicit user intent and
/// stop that exact managed session.
pub async fn prepare_session(
    context: &AppContext,
    selection: &SessionSelection,
) -> Result<PreparedSession> {
    if !matches!(
        selection.target,
        SessionTarget::Harness
            | SessionTarget::Manager
            | SessionTarget::Main
            | SessionTarget::WtSource
            | SessionTarget::Dotfiles
    ) {
        bail!("selected target does not accept a harness session");
    }
    let app = AppHarness::new(context);
    let mut agent = resolve_target(context, selection.key.as_deref(), selection.target).await?;
    if selection.harness == HarnessId::Claude
        && let Some(managed_name) = selection.managed_name.as_ref()
    {
        agent.managed_name = Some(managed_name.clone());
    }
    if agent.remote {
        bail!("session preparation must run on the worktree host");
    }
    // A fresh Claude session from the picker or harness chooser without a
    // typed name takes the next automatic name, as the TS picker did.
    if selection.harness == HarnessId::Claude
        && selection.mode == SessionMode::New
        && selection.target == SessionTarget::Harness
        && selection.managed_name.is_none()
        && agent.managed_name.is_none()
    {
        agent.managed_name = Some(wt_harness::next_auto_name(
            &context.config.paths.cache_root,
            &agent.slug,
        )?);
    }
    if selection.harness == HarnessId::Claude
        && let Some(name) = selection.managed_name.as_deref()
        && name != "manager"
        && let Some(reason) = wt_harness::validate_session_name(name)
    {
        bail!("invalid Claude session name {name:?}: {reason}");
    }
    let managed_name = selection
        .managed_name
        .as_deref()
        .or(agent.managed_name.as_deref());
    let expected_claude_id = (selection.harness == HarnessId::Claude)
        .then(|| wt_harness::claude_session_id(&agent.cwd, managed_name));
    let wanted_id = selection
        .session_id
        .as_deref()
        .or(expected_claude_id.as_deref());
    let tmux_name = session_name(&agent, selection.harness);
    if selection.live_slot.is_some() {
        if selection.mode != SessionMode::Resume {
            bail!("new session selection must not include a live slot");
        }
        let sessions = app.session_inventory(context).await?;
        let live = sessions
            .iter()
            .find(|slot| slot.name == tmux_name)
            .context("selected live session has ended; reopen the session picker")?;
        validate_live_selection(context, &app, &agent, selection, live).await?;
        return Ok(app.attach_command(&live.id, &agent.cwd));
    }
    if selection.mode == SessionMode::Resume && wanted_id.is_none() {
        bail!("resume selection is missing its exact session id");
    }
    if selection.mode == SessionMode::New && selection.session_id.is_some() {
        bail!("new session selection must not include an existing session id");
    }

    let live = app.session_inventory(context).await?;
    if let Some(live_session) = live.iter().find(|session| session.name == tmux_name) {
        if let Some(wanted_id) = wanted_id
            && live_session.harness_session_id.as_deref() == Some(wanted_id)
        {
            return Ok(app.attach_command(&tmux_name, &agent.cwd));
        }
        if selection.harness == HarnessId::Claude
            && wanted_id == expected_claude_id.as_deref()
            && selection.mode == SessionMode::Resume
        {
            return Ok(app.attach_command(&tmux_name, &agent.cwd));
        }
        bail!(
            "{} is already live with a different or unknown conversation; select that session or stop it explicitly",
            tmux_name
        );
    }

    let discovered = app
        .service
        .discover(
            selection.harness,
            &DiscoveryRequest {
                slug: agent.slug.clone(),
                worktree_path: agent.cwd.clone(),
                live_session_id: wanted_id.map(str::to_owned),
            },
            &[],
            &context.cancellation,
        )
        .await
        .with_context(|| {
            format!(
                "discover {:?} sessions for {}",
                selection.harness, agent.slug
            )
        })?;
    if selection.mode == SessionMode::Resume {
        let wanted_id = wanted_id.expect("validated above");
        let exact = discovered
            .iter()
            .find(|session| session.session_id == wanted_id)
            .with_context(|| {
                format!(
                    "selected {:?} conversation {wanted_id} is no longer available",
                    selection.harness
                )
            })?;
        if selection.harness == HarnessId::Claude
            && exact.extras.managed_name.as_deref() != managed_name
        {
            bail!("selected Claude conversation no longer matches its managed name");
        }
    }

    let named_added = if selection.harness == HarnessId::Claude
        && selection.mode == SessionMode::New
        && let Some(name) = managed_name
        && name != "manager"
    {
        !wt_harness::list_claude_names(&context.config.paths.cache_root, &agent.slug)?
            .iter()
            .any(|stored| stored == name)
    } else {
        false
    };
    if named_added {
        wt_harness::add_claude_name(
            &context.config.paths.cache_root,
            &agent.slug,
            managed_name.expect("name checked"),
        )?;
    }
    let request = HarnessSpawnRequest {
        worktree_path: agent.cwd.clone(),
        slug: agent.slug.clone(),
        managed_name: managed_name.map(str::to_owned),
        resume_session_id: (selection.mode == SessionMode::Resume)
            .then(|| wanted_id.unwrap().to_owned()),
        display_label: None,
    };
    if let Err(error) = app
        .service
        .ensure_started(selection.harness, &request, &context.cancellation)
        .await
    {
        if named_added {
            let _ = wt_harness::remove_claude_name(
                &context.config.paths.cache_root,
                &agent.slug,
                managed_name.expect("name checked"),
            );
        }
        return Err(error).context("start selected harness session");
    }
    let after = app.session_inventory(context).await?;
    let started = after
        .iter()
        .find(|session| session.name == tmux_name)
        .context("harness start completed without a tmux session")?;
    if selection.mode == SessionMode::Resume
        && selection.harness != HarnessId::Claude
        && started.harness_session_id.as_deref() != wanted_id
    {
        bail!("harness slot started with a different or unknown conversation; refusing to attach");
    }
    Ok(app.attach_command(&tmux_name, &agent.cwd))
}

/// How a live session is ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionEnd {
    /// `; d`: Claude's tmux session is killed (its pane is never an input
    /// transport); other harnesses get Ctrl+D twice.
    Graceful,
    /// `; x`: every harness's tmux session is killed.
    Kill,
}

/// Stop only the currently live conversation named by a picker selection.
/// Single-slot harnesses are checked against tmux's persisted session UUID.
/// Returns whether a live session was actually ended.
pub async fn stop_managed_session(
    context: &AppContext,
    selection: &SessionSelection,
    end: SessionEnd,
) -> Result<bool> {
    if selection.mode != SessionMode::Resume {
        bail!("only an exact resumed session can be stopped");
    }
    let app = AppHarness::new(context);
    let mut agent = resolve_target(context, selection.key.as_deref(), selection.target).await?;
    if selection.harness == HarnessId::Claude
        && let Some(managed_name) = selection.managed_name.as_ref()
    {
        agent.managed_name = Some(managed_name.clone());
    }
    let name = session_name(&agent, selection.harness);
    if selection.live_slot.is_some() {
        let lock_dir = context.config.paths.cache_root.join("locks");
        let _guard = FileLock::acquire(
            &lock_dir,
            &format!("__start__{name}"),
            "stop selected harness session",
            &context.cancellation,
        )
        .await?;
        let sessions = app.session_inventory(context).await?;
        let live = sessions
            .iter()
            .find(|slot| slot.name == name)
            .context("selected live session has ended; reopen the session picker")?;
        validate_live_selection(context, &app, &agent, selection, live).await?;
        if end == SessionEnd::Kill || selection.harness == HarnessId::Claude {
            return Ok(app
                .tmux
                .kill_session_id(&live.id, &context.cancellation)
                .await?);
        }
        return close_exact_pane(context, &app, &agent, selection, live).await;
    }
    let expected = if selection.harness == HarnessId::Claude {
        wt_harness::claude_session_id(
            &agent.cwd,
            selection
                .managed_name
                .as_deref()
                .or(agent.managed_name.as_deref()),
        )
    } else {
        selection
            .session_id
            .clone()
            .context("selected conversation has no session id")?
    };
    if selection.session_id.as_deref() != Some(expected.as_str()) {
        bail!("selected session identity is invalid; refusing to stop {name}");
    }
    if selection.harness == HarnessId::Claude {
        app.claude_sessions
            .stop_exact(
                &wt_harness::ClaudeSessionTarget {
                    slug: agent.slug,
                    cwd: agent.cwd,
                    managed_name: selection.managed_name.clone().or(agent.managed_name),
                },
                &expected,
                &context.cancellation,
            )
            .await
            .context("stop selected Claude session")
    } else {
        let lock_dir = context.config.paths.cache_root.join("locks");
        let _guard = FileLock::acquire(
            &lock_dir,
            &format!("__start__{name}"),
            "stop selected harness session",
            &context.cancellation,
        )
        .await?;
        let sessions = app.session_inventory(context).await?;
        let Some(live) = sessions.iter().find(|session| session.name == name) else {
            return Ok(false);
        };
        if live.harness_session_id.as_deref() != Some(expected.as_str()) {
            bail!(
                "live {name} slot no longer matches the selected conversation; refusing to stop it"
            );
        }
        if end == SessionEnd::Kill {
            return Ok(app
                .tmux
                .kill_session_id(&live.id, &context.cancellation)
                .await?);
        }
        close_exact_pane(context, &app, &agent, selection, live).await
    }
}

/// Keep both EOFs bound to the pane process selected before the first send.
/// Closing a pane can focus another pane without ending the tmux session.
async fn close_exact_pane(
    context: &AppContext,
    app: &AppHarness,
    agent: &AgentTarget,
    selection: &SessionSelection,
    live: &wt_tmux::SessionInfo,
) -> Result<bool> {
    let before = app.tmux.list_panes(&live.id, &context.cancellation).await?;
    let pane = before
        .iter()
        .find(|pane| pane.active && !pane.dead && pane.pid.is_some())
        .context("selected session has no live active pane; no input was sent")?;
    let target = PaneTarget::id(&pane.id);
    app.tmux
        .send_keys(&target, &["C-d"], &context.cancellation)
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let after = app.session_inventory(context).await?;
    let Some(after) = after.iter().find(|slot| slot.name == live.name) else {
        return Ok(true);
    };
    let mut captured = selection.clone();
    captured.live_slot = Some(live_identity(live));
    validate_live_selection(context, app, agent, &captured, after).await?;
    let panes = app.tmux.list_panes(&live.id, &context.cancellation).await?;
    let Some(current) = panes.iter().find(|current| current.id == pane.id) else {
        return Ok(true);
    };
    if !same_pane_process(pane, current) {
        bail!("selected pane process changed while closing it; no further input was sent");
    }
    app.tmux
        .send_keys(&target, &["C-d"], &context.cancellation)
        .await?;
    Ok(true)
}

fn same_pane_process(before: &wt_tmux::PaneInfo, after: &wt_tmux::PaneInfo) -> bool {
    before.id == after.id && before.pid.is_some() && before.pid == after.pid && !after.dead
}

/// Forget a dead Claude session's stored name so it leaves the picker.
/// Refuses while the named session is live.
pub async fn forget_claude_session(
    context: &AppContext,
    selection: &SessionSelection,
) -> Result<String> {
    if selection.harness != HarnessId::Claude {
        bail!("only Claude session names can be forgotten");
    }
    let name = selection
        .managed_name
        .clone()
        .context("this Claude session has no stored name to forget")?;
    let mut agent = resolve_target(context, selection.key.as_deref(), selection.target).await?;
    agent.managed_name = Some(name.clone());
    let tmux_name = wt_harness::claude_tmux_name(&agent.slug, Some(&name));
    let app = AppHarness::new(context);
    if app
        .session_inventory(context)
        .await?
        .iter()
        .any(|session| session.name == tmux_name)
    {
        bail!("{tmux_name} is live; it was not forgotten");
    }
    wt_harness::remove_claude_name(&context.config.paths.cache_root, &agent.slug, &name)?;
    Ok(name)
}

fn option_for(
    target: SessionTarget,
    key: Option<String>,
    harness: HarnessId,
    session: HarnessSession,
    live: Option<&wt_tmux::SessionInfo>,
) -> SessionOption {
    let is_live = live.is_some();
    SessionOption {
        selection: SessionSelection {
            key,
            target,
            harness,
            session_id: Some(session.session_id),
            live_slot: live.map(live_identity),
            managed_name: session.extras.managed_name,
            mode: SessionMode::Resume,
            live: is_live,
        },
        display_name: session.display_name,
        tmux_session_name: session.tmux_session_name,
        last_active_ms: session.last_active_ms,
        is_live,
        state: session
            .extras
            .derived_state
            .map(|state| format!("{state:?}")),
        waiting_for: session.extras.waiting_for,
    }
}

fn live_identity(slot: &wt_tmux::SessionInfo) -> LiveSessionIdentity {
    LiveSessionIdentity {
        id: slot.id.clone(),
        created_at: slot.created_at,
    }
}

fn live_slot_option(
    target: SessionTarget,
    key: Option<String>,
    harness: HarnessId,
    slot: &wt_tmux::SessionInfo,
) -> SessionOption {
    SessionOption {
        selection: SessionSelection {
            key,
            target,
            harness,
            session_id: None,
            live_slot: Some(live_identity(slot)),
            managed_name: None,
            mode: SessionMode::Resume,
            live: true,
        },
        display_name: "current session (conversation not identified)".into(),
        tmux_session_name: slot.name.clone(),
        last_active_ms: None,
        is_live: true,
        state: None,
        waiting_for: None,
    }
}

fn witness_matches(selection: &SessionSelection, live: &wt_tmux::SessionInfo) -> bool {
    selection
        .live_slot
        .as_ref()
        .is_some_and(|witness| witness.id == live.id && witness.created_at == live.created_at)
}

async fn validate_live_selection(
    context: &AppContext,
    app: &AppHarness,
    agent: &AgentTarget,
    selection: &SessionSelection,
    live: &wt_tmux::SessionInfo,
) -> Result<()> {
    if !witness_matches(selection, live) {
        bail!("live session changed since selection; reopen the session picker");
    }
    let Some(id) = selection.session_id.as_deref() else {
        return Ok(());
    };
    if selection.harness == HarnessId::Claude {
        let expected = wt_harness::claude_session_id(
            &agent.cwd,
            selection
                .managed_name
                .as_deref()
                .or(agent.managed_name.as_deref()),
        );
        if id != expected {
            bail!("selected Claude identity no longer matches its managed name");
        }
    }
    if slot_matches_selection(selection.harness, live, id) {
        return Ok(());
    }
    if selection.harness == HarnessId::Codex && live.harness_session_id.is_none() {
        let candidate = HarnessSession {
            display_name: String::new(),
            session_id: id.into(),
            tmux_session_name: live.name.clone(),
            last_active_ms: None,
            is_live: false,
            extras: Default::default(),
        };
        if app
            .service
            .recover_codex_live_identity(&live.name, &[candidate], &context.cancellation)
            .await?
            .as_deref()
            == Some(id)
        {
            return Ok(());
        }
    }
    bail!("live session no longer matches the selected conversation; reopen the session picker")
}

async fn resolve_target(
    context: &AppContext,
    key: Option<&str>,
    target: SessionTarget,
) -> Result<AgentTarget> {
    match target {
        SessionTarget::Harness | SessionTarget::Shell | SessionTarget::Diff => {
            let key = key.context("worktree session requires a target key")?;
            let inventory = context
                .repository
                .inventory_status(&context.cancellation)
                .await?;
            let record = inventory
                .into_iter()
                .find(|record| wt_core::worktree_target_key(&record.worktree.target) == key)
                .context("selected worktree no longer exists")?;
            if record.worktree.is_main {
                bail!("main clone uses the dedicated Main session target");
            }
            if matches!(
                record.worktree.target.location(),
                wt_core::WorktreeLocation::Remote { .. }
            ) {
                bail!("remote session operation must be dispatched to its host");
            }
            Ok(AgentTarget {
                slug: record.worktree.target.slug().to_owned(),
                kind: super::AgentTargetKind::Worktree,
                branch: Some(record.worktree.target.branch.clone()),
                cwd: PathBuf::from(record.worktree.target.path.clone()),
                managed_name: None,
                remote: false,
            })
        }
        SessionTarget::Manager => Ok(super::special_target(
            "manager",
            context.config.paths.main_clone.clone(),
            Some("manager".into()),
        )),
        SessionTarget::Main => Ok(super::special_target(
            "main",
            context.config.paths.main_clone.clone(),
            None,
        )),
        SessionTarget::WtSource => {
            let path = context
                .config
                .paths
                .wt_source
                .as_ref()
                .filter(|path| path.is_dir())
                .context("wt source checkout is unavailable")?;
            Ok(super::special_target("wt", path.clone(), None))
        }
        SessionTarget::Dotfiles => {
            let path = &context.config.paths.dotfiles;
            if !path.is_dir() {
                bail!("dotfiles checkout is unavailable");
            }
            Ok(super::special_target("dotfiles", path.clone(), None))
        }
    }
}

fn session_name(agent: &AgentTarget, harness: HarnessId) -> String {
    match harness {
        HarnessId::Claude => {
            wt_harness::claude_tmux_name(&agent.slug, agent.managed_name.as_deref())
        }
        HarnessId::Codex => format!("{}-codex", agent.slug),
        HarnessId::Opencode => format!("{}-opencode", agent.slug),
    }
}

fn slot_matches_selection(
    harness: HarnessId,
    live: &wt_tmux::SessionInfo,
    selected_id: &str,
) -> bool {
    harness == HarnessId::Claude || live.harness_session_id.as_deref() == Some(selected_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, session_id: Option<&str>) -> wt_tmux::SessionInfo {
        wt_tmux::SessionInfo {
            name: name.into(),
            id: "$1".into(),
            created_at: 0,
            attached_clients: 0,
            window_count: 1,
            harness_session_id: session_id.map(str::to_owned),
        }
    }

    #[test]
    fn graceful_close_never_retargets_a_new_active_pane_or_restarted_process() {
        let before = wt_tmux::PaneInfo {
            id: "%10".into(),
            session_name: "feature-codex".into(),
            window_id: "@1".into(),
            window_index: 0,
            index: 0,
            active: true,
            pid: Some(100),
            current_path: PathBuf::from("/tmp"),
            width: 80,
            height: 24,
            dead: false,
        };
        let target = PaneTarget::id(&before.id);
        let mut other = before.clone();
        other.id = "%11".into();
        other.pid = Some(101);
        assert!(!same_pane_process(&before, &other));
        assert_eq!(target.as_str(), "%10");
        let mut restarted = before.clone();
        restarted.pid = Some(102);
        assert!(!same_pane_process(&before, &restarted));
        restarted.pid = before.pid;
        restarted.dead = true;
        assert!(!same_pane_process(&before, &restarted));
        let mut unfocused = before.clone();
        unfocused.active = false;
        assert!(same_pane_process(&before, &unfocused));
        let mut unknown = before.clone();
        unknown.pid = None;
        assert!(!same_pane_process(&unknown, &unknown));
    }

    #[test]
    fn unidentified_live_slot_is_explicit_and_never_assigned_a_conversation() {
        let slot = info("feature-codex", None);
        let option = live_slot_option(
            SessionTarget::Harness,
            Some("feature".into()),
            HarnessId::Codex,
            &slot,
        );
        assert!(option.is_live);
        assert!(option.selection.session_id.is_none());
        assert_eq!(
            option.display_name,
            "current session (conversation not identified)"
        );
        assert!(witness_matches(&option.selection, &slot));
        let mut replacement = slot.clone();
        replacement.id = "$2".into();
        assert!(!witness_matches(&option.selection, &replacement));
        replacement.id = slot.id.clone();
        replacement.created_at += 1;
        assert!(!witness_matches(&option.selection, &replacement));
        assert!(!slot_matches_selection(
            HarnessId::Codex,
            &slot,
            "historical-thread"
        ));
        let wire = serde_json::to_value(&option.selection).unwrap();
        let restored: SessionSelection = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(restored, option.selection);
        let mut legacy = wire;
        legacy.as_object_mut().unwrap().remove("live_slot");
        let legacy: SessionSelection = serde_json::from_value(legacy).unwrap();
        assert!(legacy.live_slot.is_none());
    }

    #[test]
    fn identified_live_choice_captures_physical_witness() {
        let slot = info("feature-codex", Some("thread-a"));
        let session = HarnessSession {
            session_id: "thread-a".into(),
            tmux_session_name: slot.name.clone(),
            display_name: "primary".into(),
            last_active_ms: None,
            is_live: false,
            extras: Default::default(),
        };
        let option = option_for(
            SessionTarget::Harness,
            None,
            HarnessId::Codex,
            session,
            Some(&slot),
        );
        assert_eq!(option.selection.session_id.as_deref(), Some("thread-a"));
        assert_eq!(option.selection.live_slot, Some(live_identity(&slot)));
    }

    #[test]
    fn single_slot_resume_requires_exact_recorded_conversation_identity() {
        let live = info("feature-codex", Some("session-a"));
        assert!(slot_matches_selection(HarnessId::Codex, &live, "session-a"));
        assert!(!slot_matches_selection(
            HarnessId::Codex,
            &live,
            "session-b"
        ));
        assert!(!slot_matches_selection(
            HarnessId::Opencode,
            &info("feature-opencode", None),
            "session-a"
        ));
    }
}
