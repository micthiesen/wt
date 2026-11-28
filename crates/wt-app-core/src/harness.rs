//! Application boundary for harness selection, routing and messaging.
//!
//! The adapters own session formats; this module owns the app's paths, tmux
//! server and target selection. Tmux inventory failure is represented as
//! unknown and never treated as an empty server.

use std::ffi::OsString;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use wt_harness::{
    ClaudeHarness, ClaudeInjector, ClaudePaths, ClaudeSessionManager, ClaudeSessionTarget,
    CodexPaths, DiscoveryRequest, HarnessId, HarnessMessageOutcome, HarnessService,
    HarnessSpawnRequest, HarnessTarget, OpenCodePaths,
};
use wt_tmux::{
    CreateSession, SessionInfo, SessionShortcut, TmuxClient, TmuxServer, write_terminal_config,
};
use wt_tui::SessionTarget;

use crate::context::AppContext;

#[path = "session_commands.rs"]
mod session_commands;
pub use session_commands::{
    SessionEnd, forget_claude_session, prepare_session, stop_managed_session,
};
pub use session_commands::{picker_entries, picker_slug, visible_harnesses};
use session_commands::{session_name, slot_name};

/// How a delivered message reads in the target worktree's history.
#[derive(Clone, Copy, Debug, Default)]
pub struct MessageRecord<'a> {
    /// The sending worktree's slug, `manager`, `wt` for a prompt wt sent
    /// itself (an automation, a restack handoff), or `None` for the person.
    pub from: Option<&'a str>,
    /// Shown instead of the message's own first line.
    pub text: Option<&'a str>,
}

async fn record_message(
    route: &AgentRoute,
    text: &str,
    record: MessageRecord<'_>,
    outcome: &Result<HarnessMessageOutcome>,
    context: &AppContext,
) {
    if route.target.kind != AgentTargetKind::Worktree || route.target.remote {
        return;
    }
    let event = wt_core::WorktreeEventKind::Message {
        from: record
            .from
            .map(str::trim)
            .filter(|from| !from.is_empty())
            .map(str::to_owned),
        text: wt_core::event_text(record.text.unwrap_or(text)),
        delivered: message_delivered(outcome),
    };
    crate::worktree_history::record(context, &route.target.slug, event).await;
}

/// `Some(true)` when the harness proved arrival, `Some(false)` when it is
/// known not to have arrived, `None` when it was submitted without proof.
fn message_delivered(outcome: &Result<HarnessMessageOutcome>) -> Option<bool> {
    use wt_harness::{ClaudeMessageOutcome, CodexMessageOutcome};
    match outcome {
        Err(_) => Some(false),
        Ok(HarnessMessageOutcome::Claude(ClaudeMessageOutcome::Sent { delivered, .. }))
        | Ok(HarnessMessageOutcome::Codex(CodexMessageOutcome::Terminal { delivered, .. })) => {
            *delivered
        }
        Ok(HarnessMessageOutcome::Claude(ClaudeMessageOutcome::Failed {
            maybe_submitted, ..
        })) => (!maybe_submitted).then_some(false),
        Ok(HarnessMessageOutcome::Codex(
            CodexMessageOutcome::Queued(_) | CodexMessageOutcome::CliQueued { .. },
        )) => Some(true),
        Ok(HarnessMessageOutcome::Codex(CodexMessageOutcome::Ambiguous { .. })) => None,
        Ok(HarnessMessageOutcome::Codex(
            CodexMessageOutcome::NeedsTerminalFallback { .. } | CodexMessageOutcome::Failed { .. },
        )) => Some(false),
        Ok(HarnessMessageOutcome::OpenCode(_) | HarnessMessageOutcome::Pi(_)) => None,
    }
}

/// Marks a send that failed before any harness adapter was asked to deliver.
/// A caller that must never resend a possibly submitted prompt can retry one
/// that carries this.
#[derive(Clone, Copy, Debug)]
pub struct NotSent;

impl std::fmt::Display for NotSent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("nothing was sent")
    }
}

impl std::error::Error for NotSent {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTarget {
    pub slug: String,
    pub kind: AgentTargetKind,
    pub branch: Option<String>,
    pub cwd: PathBuf,
    pub managed_name: Option<String>,
    pub remote: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentTargetKind {
    Worktree,
    Special,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessChoice {
    pub selected: Option<HarnessId>,
    pub source: SelectionSource,
    pub live: Option<Vec<HarnessId>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionSource {
    Live,
    Primary,
    /// Configuration pins this slot's harness (`[harness] wt_session`); it
    /// wins over the primary, a persisted Tab selection and live sessions of
    /// other harnesses, which are left running and still listed in `live`.
    Pinned,
    Unavailable,
    RemoteUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentRoute {
    pub target: AgentTarget,
    pub choice: HarnessChoice,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedSession {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

/// Session keys keep the same worktree while the board is suspended. The
/// worktree is resolved once for the handoff and the selected agent identity
/// is retained; each key then needs only the normal tmux resume checks.
pub struct SessionNavigation<'a> {
    context: &'a AppContext,
    app: AppHarness,
    key: Option<String>,
    worktree: Option<AgentTarget>,
    agent: Option<wt_tui::SessionSelection>,
    /// The attached agent slot and the tmux listing taken at attach, kept
    /// until the worktree is known and `agent` can be built from them.
    attached: Option<(String, Vec<SessionInfo>)>,
}

impl<'a> SessionNavigation<'a> {
    /// Navigation for an attach prepared elsewhere. One tmux listing records
    /// which slot was attached; the worktree is read from Git only if a
    /// session key is pressed. [`enter_session`] and
    /// [`enter_selected_session`] need neither read: they build it from the
    /// request's own.
    pub async fn new(
        context: &'a AppContext,
        key: Option<String>,
        target: SessionTarget,
        prepared: &PreparedSession,
    ) -> Result<Self> {
        let mut navigation = Self {
            context,
            app: AppHarness::new(context),
            key,
            worktree: None,
            agent: None,
            attached: None,
        };
        if target == SessionTarget::Harness && navigation.key.is_some() {
            match navigation.attached_slot(prepared).await {
                Ok(attached) => navigation.attached = Some(attached),
                Err(error) => navigation.disable(&error),
            }
        }
        Ok(navigation)
    }

    /// Navigation from what the session request already resolved: the target
    /// and, for an agent, the attached slot with the tmux listing it was in.
    fn begin(
        context: &'a AppContext,
        app: AppHarness,
        key: Option<String>,
        target: SessionTarget,
        agent: AgentTarget,
        slot: Option<(String, Vec<SessionInfo>)>,
    ) -> Self {
        let worktree = matches!(
            target,
            SessionTarget::Harness | SessionTarget::Shell | SessionTarget::Diff
        )
        .then_some(agent);
        let mut navigation = Self {
            context,
            app,
            key,
            worktree,
            agent: None,
            attached: None,
        };
        if target == SessionTarget::Harness && navigation.key.is_some() {
            let retained = match (&navigation.worktree, &slot) {
                (Some(worktree), Some((slot, live))) => {
                    retained_agent(worktree, navigation.key.clone(), live, slot)
                }
                _ => Err(anyhow::anyhow!("agent session ended before attachment")),
            };
            match retained {
                Ok(agent) => navigation.agent = Some(agent),
                Err(error) => navigation.disable(&error),
            }
        }
        navigation
    }

    // Switching is a convenience: the first attach is already prepared and
    // must not be refused because its identity cannot be retained. Without a
    // retained agent the session keys return to the board, so a later F12
    // can never land on a different agent. Re-entry through `next` stays
    // fail-closed.
    fn disable(&mut self, error: &anyhow::Error) {
        tracing::warn!(error = %format!("{error:#}"), "session switching disabled for this attach");
        self.agent = None;
        self.attached = None;
        self.key = None;
    }

    async fn attached_slot(
        &self,
        prepared: &PreparedSession,
    ) -> Result<(String, Vec<SessionInfo>)> {
        let attached = prepared
            .args
            .last()
            .and_then(|arg| arg.to_str())
            .and_then(|arg| arg.strip_prefix('='))
            .context("agent attachment has no exact tmux target")?;
        let live = self.app.session_inventory(self.context).await?;
        let slot = live
            .iter()
            .find(|slot| slot.id == attached || slot.name == attached)
            .context("agent session ended before attachment")?
            .id
            .clone();
        Ok((slot, live))
    }

    /// The handoff's worktree, read from Git the first time it is needed.
    async fn worktree(&mut self) -> Result<AgentTarget> {
        if let Some(worktree) = &self.worktree {
            return Ok(worktree.clone());
        }
        let worktree = session_commands::resolve_target(
            self.context,
            self.key.as_deref(),
            SessionTarget::Harness,
        )
        .await?;
        self.worktree = Some(worktree.clone());
        Ok(worktree)
    }

    pub async fn next(&mut self, code: Option<i32>) -> Result<Option<PreparedSession>> {
        let target = match code {
            Some(110) => SessionTarget::Shell,
            Some(111) => SessionTarget::Diff,
            Some(112) => SessionTarget::Harness,
            _ => return Ok(None),
        };
        // Special slots have no worktree key. Keep their board-return behavior.
        let Some(key) = self.key.clone() else {
            return Ok(None);
        };
        let worktree = self.worktree().await?;
        // The board cannot remove this checkout during its own handoff, but
        // `wt rm` from another process can, and tmux would start a session
        // for a missing directory somewhere else.
        if !worktree.cwd.is_dir() {
            bail!("selected worktree no longer exists");
        }
        if target != SessionTarget::Harness {
            return terminal_session(self.context, &self.app, &worktree, target)
                .await
                .map(Some);
        }
        // The identity comes from the listing taken at attach, so a slot
        // replaced since then fails the witness check below.
        if self.agent.is_none()
            && let Some((slot, live)) = self.attached.take()
        {
            match retained_agent(&worktree, Some(key.clone()), &live, &slot) {
                Ok(agent) => self.agent = Some(agent),
                // Without the attached agent's identity a later key must
                // not pick an agent afresh.
                Err(error) => {
                    self.key = None;
                    return Err(error);
                }
            }
        }
        if let Some(agent) = &self.agent {
            let attached =
                session_commands::prepare_selected(self.context, &self.app, worktree, agent, None)
                    .await?;
            return Ok(Some(attached.prepared));
        }
        let attached = agent_session(
            self.context,
            &self.app,
            &worktree,
            Some(key.clone()),
            target,
            None,
            None,
        )
        .await?;
        let slot = attached
            .slot
            .as_deref()
            .context("agent session ended before attachment")?;
        self.agent = Some(retained_agent(&worktree, Some(key), &attached.live, slot)?);
        Ok(Some(attached.prepared))
    }
}

/// The picker selection for the agent slot a handoff attached, found by its
/// tmux id or name in `live`. It carries the slot's identity witness, so a
/// later switch back refuses a slot that was replaced meanwhile.
fn retained_agent(
    worktree: &AgentTarget,
    key: Option<String>,
    live: &[SessionInfo],
    attached: &str,
) -> Result<wt_tui::SessionSelection> {
    let slot = live
        .iter()
        .find(|slot| slot.id == attached || slot.name == attached)
        .context("agent session ended before attachment")?;
    session_commands::picker_entries(
        &HarnessId::ALL,
        worktree,
        key,
        SessionTarget::Harness,
        &[],
        live,
    )
    .into_iter()
    .find(|entry| {
        entry
            .selection
            .live_slot
            .as_ref()
            .is_some_and(|identity| identity.id == slot.id)
    })
    .map(|entry| entry.selection)
    .context("agent attachment does not belong to the selected worktree")
}

fn harness_session_environment(
    home: &Path,
    cwd: &Path,
    repository_config: Option<&Path>,
    executable: Option<&Path>,
    inherited: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::new();
    environment.insert("HOME".into(), absolute_session_path(home, cwd, home));

    let xdg_config = inherited
        .get("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(|value| absolute_session_path(Path::new(value), cwd, home))
        .unwrap_or_else(|| absolute_session_path(&home.join(".config"), cwd, home));
    environment.insert("XDG_CONFIG_HOME".into(), xdg_config.clone());

    let config = inherited
        .get("WT_CONFIG")
        .filter(|value| !value.is_empty())
        .map(|value| absolute_session_path(Path::new(value), cwd, home))
        .unwrap_or_else(|| {
            Path::new(&xdg_config)
                .join("wt/config.toml")
                .display()
                .to_string()
        });
    environment.insert("WT_CONFIG".into(), config);
    environment.insert(
        "WT_REPO_CONFIG".into(),
        repository_config
            .map(|path| absolute_session_path(path, cwd, home))
            .unwrap_or_default(),
    );

    let install_root = inherited
        .get("WT_INSTALL_ROOT")
        .filter(|value| !value.is_empty())
        .map(|root| absolute_session_path(Path::new(root), cwd, home))
        .unwrap_or_else(|| absolute_session_path(&home.join(".local/share/wt"), cwd, home));
    environment.insert("WT_INSTALL_ROOT".into(), install_root);

    let inherited_path = inherited
        .get("PATH")
        .map(String::as_str)
        .unwrap_or_default();
    let mut paths = executable
        .and_then(Path::parent)
        .into_iter()
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    paths.extend(std::env::split_paths(inherited_path));
    let mut seen = BTreeSet::new();
    paths.retain(|path| seen.insert(path.clone()));
    let path = std::env::join_paths(paths)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| inherited_path.to_owned());
    environment.insert("PATH".into(), path);
    // A managed worker names the variables its sessions must keep, such as
    // CODEX_HOME and its credentials, so agents use the managed setup.
    if let Some(names) = inherited
        .get("WT_SESSION_ENV")
        .filter(|names| !names.is_empty())
    {
        for name in names.split(':') {
            if environment.contains_key(name) {
                continue;
            }
            if let Some(value) = inherited.get(name) {
                environment.insert(name.to_owned(), value.clone());
            }
        }
        environment.insert("WT_SESSION_ENV".into(), names.clone());
    }
    // Managed secrets are sourced inside the session, never passed as values.
    if let Some(root) = inherited
        .get("WT_MANAGED_ROOT")
        .filter(|root| !root.is_empty())
    {
        environment.insert(
            wt_tmux::SESSION_SOURCE.to_owned(),
            format!("{root}/secrets/env.sh"),
        );
    }
    environment
}

/// The `wt` special session's account selector. It names the configured
/// `paths.wt_source` checkout, so a worktree that shares the slug never
/// matches, and it applies to whichever path starts Claude there.
pub fn claude_launch_auth(context: &AppContext) -> Option<wt_harness::ClaudeLaunchAuth> {
    let command = context
        .config
        .harness
        .wt_session_claude_token_command
        .clone()?;
    let wt_source = context.config.paths.wt_source.as_ref()?;
    Some(wt_harness::ClaudeLaunchAuth::token_command(
        "wt",
        wt_source,
        command,
        context.processes.clone(),
    ))
}

/// Extra `claude` arguments for the `wt` special session, scoped like
/// [`claude_launch_auth`].
pub fn claude_launch_args(context: &AppContext) -> Option<wt_harness::ClaudeLaunchArgs> {
    let args = &context.config.harness.wt_session_claude_args;
    let wt_source = context.config.paths.wt_source.as_ref()?;
    (!args.is_empty()).then(|| wt_harness::ClaudeLaunchArgs::new("wt", wt_source, args.clone()))
}

/// Codex keeps sessions and its control socket under `$CODEX_HOME`. Managed
/// workers point it at their own directory; otherwise it is `~/.codex`.
pub fn codex_paths(home: &Path, cache_root: &Path) -> CodexPaths {
    let paths = CodexPaths::new(home, cache_root);
    match std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        Some(codex_home) => paths.with_codex_home(PathBuf::from(codex_home)),
        None => paths,
    }
}

/// Resolve Pi's native session-directory overrides at app composition only.
/// The adapter's path constructor stays independent of ambient user settings.
pub fn pi_paths(home: &Path, cache_root: &Path) -> wt_harness::PiPaths {
    pi_paths_with_overrides(
        home,
        cache_root,
        std::env::var_os("PI_CODING_AGENT_SESSION_DIR"),
        std::env::var_os("PI_CODING_AGENT_DIR"),
    )
}

fn pi_paths_with_overrides(
    home: &Path,
    cache_root: &Path,
    session_dir: Option<OsString>,
    agent_dir: Option<OsString>,
) -> wt_harness::PiPaths {
    let paths = wt_harness::PiPaths::new(home, cache_root);
    let sessions = session_dir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            agent_dir
                .filter(|value| !value.is_empty())
                .map(|path| PathBuf::from(path).join("sessions"))
        });
    match sessions {
        Some(sessions) => paths.with_sessions(sessions),
        None => paths,
    }
}

fn absolute_session_path(path: &Path, cwd: &Path, home: &Path) -> String {
    let raw = path.to_string_lossy();
    let expanded = if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        path.to_path_buf()
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    absolute.display().to_string()
}

#[derive(Clone)]
pub struct AppHarness {
    service: HarnessService,
    tmux: TmuxClient,
    claude_sessions: ClaudeSessionManager,
    claude_injector: ClaudeInjector,
    cache_root: PathBuf,
    primary_fallback: HarnessId,
    harness_config: wt_config::HarnessConfig,
    codex_config: PathBuf,
    main_clone: PathBuf,
    dotfiles: PathBuf,
    wt_source: Option<PathBuf>,
}

impl AppHarness {
    async fn validate_live_pi(
        &self,
        request: &HarnessSpawnRequest,
        context: &AppContext,
    ) -> Result<()> {
        wt_harness::PiHarness::new(
            pi_paths(&context.home, &context.config.paths.cache_root),
            context.processes.clone(),
            self.tmux.clone(),
        )
        .validate_live(request, &context.cancellation)
        .await?;
        Ok(())
    }

    pub fn new(context: &AppContext) -> Self {
        let cache_root = context.config.paths.cache_root.clone();
        let codex_paths = codex_paths(&context.home, &cache_root);
        let codex_config = codex_paths.codex_home.join("config.toml");
        let inherited_environment = std::env::vars_os()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let session_environment = harness_session_environment(
            &context.home,
            &context.cwd,
            context.config.repository_config.as_deref(),
            std::env::current_exe().ok().as_deref(),
            &inherited_environment,
        );
        let spawn_path = session_environment.get("PATH").cloned().unwrap_or_default();
        let mut server =
            TmuxServer::named(context.config.tmux.socket.clone()).with_cwd(context.home.clone());
        if let Ok(config_path) = write_terminal_config(
            &cache_root,
            &context.home,
            context.config.tmux.terminal_config.as_deref(),
        ) {
            server = server.with_config_file(config_path);
        }
        let tmux = TmuxClient::new(context.processes.clone(), server)
            .with_command_prefix(crate::workload::prefix(context))
            .with_failure_retention()
            .with_session_environment(
                session_environment
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        let claude_paths = ClaudePaths::new(&context.home, &cache_root);
        let launch_auth = claude_launch_auth(context);
        let launch_args = claude_launch_args(context);
        let claude_sessions =
            ClaudeSessionManager::new(ClaudeHarness::new(claude_paths.clone()), tmux.clone())
                .with_spawn_path(spawn_path.clone())
                .with_launch_auth(launch_auth.clone())
                .with_launch_args(launch_args.clone())
                .with_launch_env(context.config.harness.claude_env.clone());
        let claude_injector = ClaudeInjector::new(cache_root.clone());
        let service = HarnessService::new(
            claude_paths,
            codex_paths,
            OpenCodePaths::new(&context.home, &cache_root),
            pi_paths(&context.home, &cache_root),
            context.processes.clone(),
            tmux.clone(),
        )
        .with_spawn_path(spawn_path)
        .with_claude_launch_auth(launch_auth)
        .with_claude_launch_args(launch_args)
        .with_claude_launch_env(context.config.harness.claude_env.clone());
        Self {
            service,
            tmux,
            claude_sessions,
            claude_injector,
            cache_root,
            primary_fallback: context.config.harness.primary,
            harness_config: context.config.harness.clone(),
            codex_config,
            main_clone: context.config.paths.main_clone.clone(),
            dotfiles: context.config.paths.dotfiles.clone(),
            wt_source: context.config.paths.wt_source.clone(),
        }
    }

    /// Start `target`'s Claude session on `model` if this harness is what
    /// creates it. A session already running keeps the model it has.
    pub fn with_claude_model(mut self, target: &AgentTarget, model: &str) -> Self {
        let args = wt_harness::ClaudeLaunchArgs::new(
            target.slug.clone(),
            &target.cwd,
            vec!["--model".into(), model.to_owned()],
        );
        self.claude_sessions = self.claude_sessions.with_more_launch_args(args.clone());
        self.service = self.service.with_more_claude_launch_args(args);
        self
    }

    /// Bound a Claude send from this harness, for a caller that cannot wait
    /// out a person: `human` is how long the session may keep waiting on
    /// one, `queue` how long the send queues behind another delivery.
    pub fn with_claude_turn_limits(
        mut self,
        human: std::time::Duration,
        queue: std::time::Duration,
    ) -> Self {
        self.service = self.service.with_claude_turn_limits(human, queue);
        self
    }

    pub async fn session_inventory(&self, context: &AppContext) -> Result<Vec<SessionInfo>> {
        self.tmux
            .list_sessions(&context.cancellation)
            .await
            .context("inspect wt tmux sessions")
    }

    pub async fn stop_claude(
        &self,
        slug: &str,
        cwd: &Path,
        managed_name: Option<String>,
        context: &AppContext,
    ) -> Result<()> {
        self.claude_sessions
            .stop(
                &ClaudeSessionTarget {
                    slug: slug.into(),
                    cwd: cwd.to_owned(),
                    managed_name,
                },
                &context.cancellation,
            )
            .await
            .context("stop Claude session")
    }

    pub async fn claude_selftest(
        &self,
        tmux_name: &str,
        context: &AppContext,
    ) -> wt_harness::ClaudeSelftestOutcome {
        self.claude_injector
            .selftest(tmux_name, &context.cancellation)
            .await
    }

    pub async fn codex_app_server_info(
        &self,
        context: &AppContext,
    ) -> Result<Option<wt_harness::CodexAppServerInfo>> {
        self.service
            .codex_app_server_info(&context.cancellation)
            .await
            .context("inspect Codex app-server")
    }

    /// Attach to an agent session.
    pub fn attach_command(&self, session: &str, cwd: &Path) -> PreparedSession {
        self.attach_command_as(session, cwd, SessionShortcut::Harness)
    }

    /// Attach to a session whose switch-key kind the caller knows; the kind
    /// is never derived from the session name.
    pub fn attach_command_as(
        &self,
        session: &str,
        cwd: &Path,
        shortcut: SessionShortcut,
    ) -> PreparedSession {
        PreparedSession {
            program: "tmux".into(),
            args: self.tmux.attach_session_args(session, shortcut),
            cwd: cwd.to_owned(),
        }
    }

    pub fn primary(&self) -> HarnessId {
        let path = self.cache_root.join("harness.json");
        let Ok(text) = fs::read_to_string(path) else {
            return self.primary_fallback;
        };
        serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value.get("primary")?.as_str().map(str::to_owned))
            .and_then(|value| serde_json::from_value(Value::String(value)).ok())
            .unwrap_or(self.primary_fallback)
    }

    /// The harness configuration pins for this target, if any.
    pub fn pinned(&self, slug: &str, kind: AgentTargetKind) -> Option<HarnessId> {
        pinned_harness(&self.harness_config, slug, kind)
    }

    pub async fn routes(&self, context: &AppContext) -> Result<Vec<AgentRoute>> {
        let records = context.repository.inventory(&context.cancellation).await?;
        let sessions = self.tmux.list_sessions(&context.cancellation).await.ok();
        Ok(self.routes_from(&records, sessions.as_deref()))
    }

    /// Routes for an inventory and a tmux listing the caller already holds.
    /// `None` is a listing that could not be read, never an empty server.
    fn routes_from(
        &self,
        records: &[wt_vcs::WorktreeRecord],
        sessions: Option<&[SessionInfo]>,
    ) -> Vec<AgentRoute> {
        let targets = self.targets_from_records(records.iter());
        let names = sessions.map(|sessions| {
            sessions
                .iter()
                .map(|s| s.name.clone())
                .collect::<BTreeSet<_>>()
        });
        let ids = sessions.unwrap_or_default();
        let known = targets.iter().map(|target| target.slug.clone()).collect();
        let primary = self.primary();
        targets
            .into_iter()
            .map(|target| {
                let choice = match &names {
                    _ if target.remote => HarnessChoice {
                        selected: None,
                        source: SelectionSource::RemoteUnavailable,
                        live: None,
                    },
                    None => HarnessChoice {
                        selected: None,
                        source: SelectionSource::Unavailable,
                        live: None,
                    },
                    Some(names) => {
                        let live = live_harnesses(&target.slug, names, &known, ids);
                        select_harness(self.pinned(&target.slug, target.kind), primary, live)
                    }
                };
                AgentRoute { target, choice }
            })
            .collect()
    }

    /// The slot agents a session picker can name, with the same checks as
    /// [`Self::slot_agent`]. Reads the checkout directories, never Git.
    pub fn slot_agents(&self) -> Vec<(wt_tui::SessionTarget, AgentTarget)> {
        [
            wt_tui::SessionTarget::Manager,
            wt_tui::SessionTarget::Main,
            wt_tui::SessionTarget::WtSource,
            wt_tui::SessionTarget::Dotfiles,
        ]
        .into_iter()
        .filter_map(|target| Some((target, self.slot_agent(target)?)))
        .collect()
    }

    /// The agent for one slot target, or `None` when its checkout is absent.
    pub fn slot_agent(&self, target: wt_tui::SessionTarget) -> Option<AgentTarget> {
        slot_agent_at(
            &self.main_clone,
            &self.dotfiles,
            self.wt_source.as_deref(),
            target,
        )
    }

    pub fn targets(&self, snapshots: &[wt_vcs::WorktreeSnapshot]) -> Vec<AgentTarget> {
        self.targets_from_records(snapshots.iter().map(|snapshot| &snapshot.worktree))
    }

    fn targets_from_records<'a>(
        &self,
        records: impl Iterator<Item = &'a wt_vcs::WorktreeRecord>,
    ) -> Vec<AgentTarget> {
        let mut targets = Vec::new();
        if let Some(path) = self.wt_source.as_ref().filter(|path| path.is_dir()) {
            targets.push(special_target("wt", path.clone(), None));
        }
        targets.push(special_target("main", self.main_clone.clone(), None));
        if self.dotfiles.is_dir() {
            targets.push(special_target("dotfiles", self.dotfiles.clone(), None));
        }
        targets.push(special_target(
            "manager",
            self.main_clone.clone(),
            Some("manager".into()),
        ));
        let reserved = targets
            .iter()
            .map(|target| target.slug.clone())
            .collect::<BTreeSet<_>>();
        targets.extend(
            records
                .filter(|record| !record.is_main)
                .filter(|record| !reserved.contains(record.target.slug()))
                .map(|record| AgentTarget {
                    slug: record.target.slug().to_owned(),
                    kind: AgentTargetKind::Worktree,
                    branch: Some(record.target.branch.clone()),
                    cwd: PathBuf::from(&record.target.path),
                    managed_name: None,
                    remote: matches!(
                        record.target.location(),
                        wt_core::WorktreeLocation::Remote { .. }
                    ),
                }),
        );
        targets
    }

    /// Resolve a slug, branch or `prefix/slug` name. An exact slug or branch
    /// match anywhere wins over the last-segment fallback, so `team/foo`
    /// never reaches worktree `foo` while a worktree has branch `team/foo`.
    pub fn target_for<'a>(requested: &str, routes: &'a [AgentRoute]) -> Option<&'a AgentRoute> {
        routes
            .iter()
            .find(|route| route.target.slug == requested)
            .or_else(|| {
                routes
                    .iter()
                    .find(|route| route.target.branch.as_deref() == Some(requested))
            })
            .or_else(|| {
                let slug = requested.rsplit('/').next()?;
                routes.iter().find(|route| route.target.slug == slug)
            })
    }

    pub async fn send(
        &self,
        route: &AgentRoute,
        text: &str,
        sender: Option<&str>,
        context: &AppContext,
    ) -> Result<HarnessMessageOutcome> {
        self.send_with(route, text, sender, false, context).await
    }

    /// `steer` delivers into a running Codex turn instead of queueing.
    pub async fn send_with(
        &self,
        route: &AgentRoute,
        text: &str,
        sender: Option<&str>,
        steer: bool,
        context: &AppContext,
    ) -> Result<HarnessMessageOutcome> {
        let record = MessageRecord {
            from: sender,
            text: None,
        };
        self.send_recorded(route, text, sender, steer, record, context)
            .await
    }

    /// Deliver a message and record it in the target worktree's history.
    /// Every send passes through here, so the history has one writer.
    /// Special sessions (the manager, the main checkout) have no row.
    pub async fn send_recorded(
        &self,
        route: &AgentRoute,
        text: &str,
        sender: Option<&str>,
        steer: bool,
        record: MessageRecord<'_>,
        context: &AppContext,
    ) -> Result<HarnessMessageOutcome> {
        let outcome = self.deliver(route, text, sender, steer, context).await;
        record_message(route, text, record, &outcome, context).await;
        outcome
    }

    /// Send a prompt from wt whose sender writes its own history: a watch's
    /// wake-up is one line there, not a message beside it.
    pub async fn send_unrecorded(
        &self,
        route: &AgentRoute,
        text: &str,
        context: &AppContext,
    ) -> Result<HarnessMessageOutcome> {
        self.deliver(route, text, None, false, context).await
    }

    async fn deliver(
        &self,
        route: &AgentRoute,
        text: &str,
        sender: Option<&str>,
        steer: bool,
        context: &AppContext,
    ) -> Result<HarnessMessageOutcome> {
        let id = route.choice.selected.ok_or_else(|| {
            anyhow::Error::new(NotSent).context(
                "could not inspect wt's tmux session registry; no harness was selected or started",
            )
        })?;
        self.prepare_start(id, &route.target.slug, &route.target.cwd, context)
            .await
            .map_err(|error| error.context(NotSent))?;
        self.service
            .send(
                &HarnessTarget {
                    id,
                    slug: route.target.slug.clone(),
                    cwd: route.target.cwd.clone(),
                    managed_name: route.target.managed_name.clone(),
                    sender: sender.map(str::to_owned),
                    text: text.to_owned(),
                    steer,
                },
                &context.cancellation,
            )
            .await
            .context("deliver agent message")
    }

    /// A live Claude `wt` slot keeps the account it started with. Name the
    /// restart that would apply the configured selection, if one is needed.
    pub async fn claude_auth_restart_notice(
        &self,
        route: &AgentRoute,
        context: &AppContext,
    ) -> Option<String> {
        if route.target.remote || route.choice.selected != Some(HarnessId::Claude) {
            return None;
        }
        self.service
            .claude_launch_auth_restart_notice(
                &HarnessSpawnRequest {
                    worktree_path: route.target.cwd.clone(),
                    slug: route.target.slug.clone(),
                    managed_name: route.target.managed_name.clone(),
                    resume_session_id: None,
                    display_label: None,
                },
                &context.cancellation,
            )
            .await
            .ok()
            .flatten()
    }

    pub async fn ensure_started(
        &self,
        id: HarnessId,
        request: &HarnessSpawnRequest,
        context: &AppContext,
    ) -> Result<bool> {
        self.prepare_start(id, &request.slug, &request.worktree_path, context)
            .await?;
        Ok(self
            .service
            .ensure_started(id, request, &context.cancellation)
            .await?)
    }

    /// Every app-owned launch and message wake passes through this boundary.
    /// Authorize the exact checkout from current local inventory or an explicit
    /// configured special target before updating Codex's selected home.
    async fn prepare_start(
        &self,
        id: HarnessId,
        slug: &str,
        cwd: &Path,
        context: &AppContext,
    ) -> Result<()> {
        if id != HarnessId::Codex {
            return Ok(());
        }
        let authorized = match slug {
            "main" | "manager" => Some(self.main_clone.clone()),
            "wt" => self.wt_source.clone(),
            "dotfiles" => Some(self.dotfiles.clone()),
            _ => context
                .repository
                .inventory(&context.cancellation)
                .await?
                .into_iter()
                .find(|record| {
                    record.target.slug() == slug
                        && matches!(record.target.location(), wt_core::WorktreeLocation::Local)
                })
                .map(|record| PathBuf::from(record.target.path)),
        }
        .with_context(|| {
            format!(
                "Codex target {slug} is not a current local worktree or configured special root"
            )
        })?;
        let cwd = cwd.to_path_buf();
        let config = self.codex_config.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let root = fs::canonicalize(&cwd).context("resolve Codex target root")?;
            let authorized =
                fs::canonicalize(&authorized).context("resolve authorized Codex root")?;
            if root != authorized {
                bail!("Codex target path no longer matches its authorized checkout");
            }
            match wt_harness::ensure_trusted_project(&config, &root) {
                Err(error) if error.is_access_denied() => {
                    warn_trust_denied(&root, &error);
                    Ok(())
                }
                result => Ok(result?),
            }
        })
        .await
        .context("prepare Codex target trust")??;
        Ok(())
    }

    pub async fn discover(
        &self,
        route: &AgentRoute,
        context: &AppContext,
    ) -> Result<Vec<wt_harness::HarnessSession>> {
        let id = route
            .choice
            .selected
            .ok_or_else(|| anyhow::anyhow!("could not inspect wt's tmux session registry"))?;
        let sessions = self.tmux.list_sessions(&context.cancellation).await?;
        let live_id = sessions
            .iter()
            .find(|session| session.name == session_name(&route.target, id))
            .and_then(|session| session.harness_session_id.clone());
        self.service
            .discover(
                id,
                &DiscoveryRequest {
                    slug: route.target.slug.clone(),
                    worktree_path: route.target.cwd.clone(),
                    live_session_id: live_id,
                },
                &sessions
                    .into_iter()
                    .map(|session| session.name)
                    .collect::<Vec<_>>(),
                &context.cancellation,
            )
            .await
            .context("discover harness sessions")
    }
}

/// A denied trust write must not fail the send, status or hold it rides on.
/// Codex may then show its folder-trust gate, which message delivery already
/// detects and reports without typing into it. Warn once per process; only an
/// agent sandbox's CLI process gets the stderr line, since the board owns its
/// terminal.
fn warn_trust_denied(root: &Path, error: &wt_harness::CodexTrustError) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(root = %root.display(), error = %error, "Codex trust not recorded");
        if std::env::var_os("CODEX_SANDBOX").is_some() {
            eprintln!(
                "wt: warning: the Codex sandbox denied recording trust for {} ({error}); \
                 continuing. If Codex asks to trust that folder, rerun with escalation.",
                root.display()
            );
        }
    });
}

/// The one rule for configuration-pinned slot harnesses: only the `wt`
/// special session can be pinned, by `[harness] wt_session`. A worktree that
/// happens to be named `wt` is never pinned.
pub fn pinned_harness(
    config: &wt_config::HarnessConfig,
    slug: &str,
    kind: AgentTargetKind,
) -> Option<HarnessId> {
    match (kind, slug) {
        (AgentTargetKind::Special, "wt") => config.wt_session,
        _ => None,
    }
}

/// [`pinned_harness`] for a slot surface, for callers holding a
/// [`SessionTarget`] rather than an [`AgentTarget`].
pub fn pinned_slot_harness(
    config: &wt_config::HarnessConfig,
    target: SessionTarget,
) -> Option<HarnessId> {
    match target {
        SessionTarget::WtSource => pinned_harness(config, "wt", AgentTargetKind::Special),
        _ => None,
    }
}

fn special_target(slug: &str, cwd: PathBuf, managed_name: Option<String>) -> AgentTarget {
    AgentTarget {
        slug: slug.into(),
        kind: AgentTargetKind::Special,
        branch: None,
        cwd,
        managed_name,
        remote: false,
    }
}
fn slot_agent_at(
    main_clone: &Path,
    dotfiles: &Path,
    wt_source: Option<&Path>,
    target: SessionTarget,
) -> Option<AgentTarget> {
    use SessionTarget as Slot;
    match target {
        Slot::Manager => Some(special_target(
            "manager",
            main_clone.to_owned(),
            Some("manager".into()),
        )),
        Slot::Main => Some(special_target("main", main_clone.to_owned(), None)),
        Slot::WtSource => wt_source
            .filter(|path| path.is_dir())
            .map(|path| special_target("wt", path.to_owned(), None)),
        Slot::Dotfiles => dotfiles
            .is_dir()
            .then(|| special_target("dotfiles", dotfiles.to_owned(), None)),
        Slot::Harness | Slot::Shell | Slot::Diff => None,
    }
}

/// Prepare a tmux client ticket for the UI controller. This function may
/// create/resume the detached session but never takes terminal ownership.
pub async fn ui_session(
    context: &AppContext,
    key: Option<String>,
    target: SessionTarget,
) -> Result<PreparedSession> {
    ui_session_with_harness(context, key, target, None).await
}

/// Prepare a session with an explicit harness choice. Remote session handoff
/// uses this when the controller's selected harness differs from the worker's
/// local primary; ordinary local UI sessions keep using `ui_session`.
pub async fn ui_session_with_harness(
    context: &AppContext,
    key: Option<String>,
    target: SessionTarget,
    selected_harness: Option<HarnessId>,
) -> Result<PreparedSession> {
    let app = AppHarness::new(context);
    Ok(enter(context, &app, key, target, selected_harness, None)
        .await?
        .prepared)
}

/// Prepare a session together with the switch-key state for its handoff.
/// Both come from one resolution of the target: one worktree inventory and
/// one tmux listing decide what is attached and whose identity is retained.
pub async fn enter_session<'a>(
    context: &'a AppContext,
    key: Option<String>,
    target: SessionTarget,
    selected_harness: Option<HarnessId>,
) -> Result<(PreparedSession, SessionNavigation<'a>)> {
    let app = AppHarness::new(context);
    let entered = enter(context, &app, key.clone(), target, selected_harness, None).await?;
    let navigation =
        SessionNavigation::begin(context, app, key, target, entered.agent, entered.slot);
    Ok((entered.prepared, navigation))
}

/// [`enter_session`] for a caller that names a worktree by slug, as the
/// remote worker's command line does. The slug is found in the same inventory
/// the session is prepared from. Slot targets have no slug and ignore it.
pub async fn enter_session_by_slug<'a>(
    context: &'a AppContext,
    slug: &str,
    target: SessionTarget,
    selected_harness: Option<HarnessId>,
) -> Result<(PreparedSession, SessionNavigation<'a>)> {
    if !matches!(
        target,
        SessionTarget::Harness | SessionTarget::Shell | SessionTarget::Diff
    ) {
        return enter_session(context, None, target, selected_harness).await;
    }
    let records = context.repository.inventory(&context.cancellation).await?;
    let key = records
        .iter()
        .find(|record| !record.is_main && record.target.slug() == slug)
        .map(|record| wt_core::worktree_target_key(&record.target))
        .with_context(|| format!("worktree not found on this host: {slug}"))?;
    let app = AppHarness::new(context);
    let entered = enter(
        context,
        &app,
        Some(key.clone()),
        target,
        selected_harness,
        Some(records),
    )
    .await?;
    let navigation =
        SessionNavigation::begin(context, app, Some(key), target, entered.agent, entered.slot);
    Ok((entered.prepared, navigation))
}

/// [`prepare_session`] together with the switch-key state for its handoff,
/// from the same single resolution as [`enter_session`].
pub async fn enter_selected_session<'a>(
    context: &'a AppContext,
    selection: &wt_tui::SessionSelection,
) -> Result<(PreparedSession, SessionNavigation<'a>)> {
    session_commands::ensure_harness_selection(selection.target)?;
    let app = AppHarness::new(context);
    let agent =
        session_commands::resolve_target(context, selection.key.as_deref(), selection.target)
            .await?;
    let attached =
        session_commands::prepare_selected(context, &app, agent.clone(), selection, None).await?;
    let slot = attached.slot.map(|slot| (slot, attached.live));
    let navigation = SessionNavigation::begin(
        context,
        app,
        selection.key.clone(),
        selection.target,
        agent,
        slot,
    );
    Ok((attached.prepared, navigation))
}

/// What one session request resolved and attached.
struct Entered {
    prepared: PreparedSession,
    agent: AgentTarget,
    /// The agent slot's tmux id and the listing it was found in.
    slot: Option<(String, Vec<SessionInfo>)>,
}

async fn enter(
    context: &AppContext,
    app: &AppHarness,
    key: Option<String>,
    target: SessionTarget,
    selected_harness: Option<HarnessId>,
    records: Option<Vec<wt_vcs::WorktreeRecord>>,
) -> Result<Entered> {
    let (agent, records) = match target {
        SessionTarget::Harness | SessionTarget::Shell | SessionTarget::Diff => {
            let key = key
                .as_deref()
                .context("worktree session requires a worktree key")?;
            let records = match records {
                Some(records) => records,
                None => context.repository.inventory(&context.cancellation).await?,
            };
            let agent = session_commands::worktree_target(&records, key)?;
            (agent, Some(records))
        }
        SessionTarget::Manager => (
            special_target(
                "manager",
                context.config.paths.main_clone.clone(),
                Some("manager".into()),
            ),
            None,
        ),
        SessionTarget::Main => (
            special_target("main", context.config.paths.main_clone.clone(), None),
            None,
        ),
        SessionTarget::WtSource => {
            let path = context
                .config
                .paths
                .wt_source
                .as_ref()
                .filter(|path| path.is_dir())
                .context(unavailable_source_message())?;
            (special_target("wt", path.clone(), None), None)
        }
        SessionTarget::Dotfiles => {
            let path = &context.config.paths.dotfiles;
            if !path.is_dir() {
                bail!(
                    "dotfiles session is unavailable: configured path {} does not exist",
                    path.display()
                );
            }
            (special_target("dotfiles", path.clone(), None), None)
        }
    };
    if matches!(target, SessionTarget::Shell | SessionTarget::Diff) {
        let prepared = terminal_session(context, app, &agent, target).await?;
        return Ok(Entered {
            prepared,
            agent,
            slot: None,
        });
    }
    let attached =
        agent_session(context, app, &agent, key, target, selected_harness, records).await?;
    Ok(Entered {
        prepared: attached.prepared,
        agent,
        slot: attached.slot.map(|slot| (slot, attached.live)),
    })
}

/// Attach to a worktree's shell or diff session, creating it when absent.
async fn terminal_session(
    context: &AppContext,
    app: &AppHarness,
    agent: &AgentTarget,
    target: SessionTarget,
) -> Result<PreparedSession> {
    let (name, shortcut) = match target {
        SessionTarget::Shell => (format!("{}-shell", agent.slug), SessionShortcut::Shell),
        SessionTarget::Diff => (format!("{}-diff", agent.slug), SessionShortcut::Diff),
        _ => bail!("not a shell or diff session target"),
    };
    if !app
        .tmux
        .session_exists(&name, &context.cancellation)
        .await?
    {
        let command = if target == SessionTarget::Shell {
            let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
            vec![shell.to_string_lossy().into_owned()]
        } else {
            let slug = agent.slug.clone();
            let state = context
                .database
                .call(|store| Ok(store.read_wt_state()?))
                .await?;
            let base = state
                .get("slugs")
                .and_then(|slugs| slugs.get(&slug))
                .and_then(|entry| entry.get("baseBranch"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&context.config.branch.base);
            let command =
                resolve_diff_command(&context.config.diff.command, &format!("origin/{base}"));
            vec!["/bin/sh".into(), "-lc".into(), command]
        };
        app.tmux
            .create_session(
                &CreateSession {
                    name: name.clone(),
                    cwd: agent.cwd.clone(),
                    command,
                    width: None,
                    height: None,
                },
                &context.cancellation,
            )
            .await?;
    }
    Ok(app.attach_command_as(&name, &agent.cwd, shortcut))
}

/// Attach to the target's agent, resuming its live slot or starting one.
/// One tmux listing decides the harness, the slot and its identity witness;
/// `records` is the request's worktree inventory when it already took one.
async fn agent_session(
    context: &AppContext,
    app: &AppHarness,
    agent: &AgentTarget,
    key: Option<String>,
    target: SessionTarget,
    selected_harness: Option<HarnessId>,
    records: Option<Vec<wt_vcs::WorktreeRecord>>,
) -> Result<session_commands::Attached> {
    let live = app
        .tmux
        .list_sessions(&context.cancellation)
        .await
        .context("could not inspect wt's tmux sessions; no session was started")?;
    let harness_id = match selected_harness {
        Some(id) => id,
        None => {
            // Another worktree's slug can equal one of this target's slot
            // names, so the choice needs every slug, not only this one.
            let records = match records {
                Some(records) => records,
                None => context.repository.inventory(&context.cancellation).await?,
            };
            app.routes_from(&records, Some(&live))
                .into_iter()
                .find(|route| route.target.slug == agent.slug)
                .context("selected session target is unavailable")?
                .choice
                .selected
                .context("could not inspect wt's tmux sessions; no session was started")?
        }
    };
    if selected_harness.is_none()
        && let Some(attached) = session_commands::prepare_live_session(
            context, app, agent, key, target, harness_id, &live,
        )
        .await?
    {
        return Ok(attached);
    }
    let request = HarnessSpawnRequest {
        worktree_path: agent.cwd.clone(),
        slug: agent.slug.clone(),
        managed_name: agent.managed_name.clone(),
        resume_session_id: None,
        display_label: None,
    };
    let tmux_name = slot_name(&agent.slug, agent.managed_name.as_deref(), harness_id);
    let prepared = app.attach_command(&tmux_name, &agent.cwd);
    if let Some(slot) = live.iter().find(|slot| slot.name == tmux_name) {
        if harness_id == HarnessId::Pi {
            app.validate_live_pi(&request, context).await?;
        }
        return Ok(session_commands::Attached {
            prepared,
            slot: Some(slot.id.clone()),
            live,
        });
    }
    // Claude's slot is its checkout and name; only the single-slot harnesses
    // pick a conversation to resume from their history.
    let resume = if harness_id == HarnessId::Claude {
        None
    } else {
        let discovered = app
            .service
            .discover(
                harness_id,
                &DiscoveryRequest {
                    slug: agent.slug.clone(),
                    worktree_path: agent.cwd.clone(),
                    live_session_id: None,
                },
                &[],
                &context.cancellation,
            )
            .await?;
        // Special targets can share a checkout (manager and main).
        // History's directory match cannot establish slot ownership.
        let owned: BTreeSet<String> = live
            .iter()
            .filter(|slot| slot.name != tmux_name)
            .filter_map(|slot| slot.harness_session_id.clone())
            .collect();
        primary_single_slot_session(&discovered, &owned).map(|entry| entry.session_id.clone())
    };
    app.ensure_started(
        harness_id,
        &HarnessSpawnRequest {
            resume_session_id: resume,
            ..request
        },
        context,
    )
    .await?;
    let live = app.session_inventory(context).await?;
    let slot = live
        .iter()
        .find(|slot| slot.name == tmux_name)
        .map(|slot| slot.id.clone());
    Ok(session_commands::Attached {
        prepared,
        slot,
        live,
    })
}

fn primary_single_slot_session<'a>(
    sessions: &'a [wt_harness::HarnessSession],
    owned: &BTreeSet<String>,
) -> Option<&'a wt_harness::HarnessSession> {
    sessions
        .iter()
        .filter(|session| !owned.contains(&session.session_id))
        .find(|session| session.extras.managed_name.as_deref() == Some("primary"))
        .or_else(|| {
            sessions
                .iter()
                .filter(|session| !owned.contains(&session.session_id))
                .max_by_key(|session| session.last_active_ms.unwrap_or_default())
        })
}

fn resolve_diff_command(template: &str, base: &str) -> String {
    if !template.contains("{{base}}") {
        return template.to_owned();
    }
    let quoted = format!("\"{}\"", base.replace('"', "\\\""));
    template.replace("{{base}}", &quoted)
}

/// Route selection for one target. A configured pin decides outright and
/// never touches a live session of another harness; otherwise a live session
/// wins, preferring the primary when several are live.
pub fn select_harness(
    pinned: Option<HarnessId>,
    primary: HarnessId,
    live: Vec<HarnessId>,
) -> HarnessChoice {
    match pinned {
        Some(id) => HarnessChoice {
            selected: Some(id),
            source: SelectionSource::Pinned,
            live: Some(live),
        },
        None => choose_live_harness(primary, live),
    }
}

fn choose_live_harness(primary: HarnessId, live: Vec<HarnessId>) -> HarnessChoice {
    let selected = if live.contains(&primary) {
        primary
    } else {
        live.first().copied().unwrap_or(primary)
    };
    HarnessChoice {
        selected: Some(selected),
        source: if live.is_empty() {
            SelectionSource::Primary
        } else {
            SelectionSource::Live
        },
        live: Some(live),
    }
}

fn live_harnesses(
    slug: &str,
    names: &BTreeSet<String>,
    known_slugs: &BTreeSet<String>,
    sessions: &[SessionInfo],
) -> Vec<HarnessId> {
    let mut live = Vec::new();
    for id in HarnessId::ALL {
        let name = slot_name(slug, None, id);
        // An auxiliary harness slot can have the same name as another
        // worktree's Claude primary. The real slug owns that exact name.
        if (id == HarnessId::Claude || !known_slugs.contains(&name)) && names.contains(&name) {
            live.push(id);
            continue;
        }
        if id == HarnessId::Claude
            && (names.contains(&format!("{slug}~manager"))
                || sessions
                    .iter()
                    .any(|session| session.name.starts_with(&format!("{slug}~"))))
        {
            live.push(id);
        }
    }
    live
}

/// The persisted primary harness, read without blocking a runtime worker.
/// A missing or unreadable selection falls back to configuration.
pub async fn read_primary(path: &Path, fallback: HarnessId) -> HarnessId {
    let Ok(text) = tokio::fs::read_to_string(path).await else {
        return fallback;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value.get("primary")?.as_str().map(str::to_owned))
        .and_then(|value| serde_json::from_value(Value::String(value)).ok())
        .unwrap_or(fallback)
}

pub fn persist_primary(cache_root: &Path, id: HarnessId) -> Result<()> {
    let path = cache_root.join("harness.json");
    let mut root = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .context("read primary harness selection")?
            .as_object()
            .cloned()
            .context("primary harness selection must be an object")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Default::default(),
        Err(error) => return Err(error).context("read primary harness selection"),
    };
    root.insert("primary".into(), Value::String(id.as_str().into()));
    let text = serde_json::to_vec_pretty(&Value::Object(root))?;
    let parent = path.parent().context("harness cache path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut tmp, &text)?;
    tmp.as_file().sync_all()?;
    tmp.persist(&path)
        .context("replace primary harness selection")?;
    Ok(())
}

pub fn unavailable_source_message() -> &'static str {
    "the wt source session is unavailable in a native install; configure paths.wt_source to an existing checkout to enable it"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_wt_special_session_is_pinned_and_unset_keeps_live_selection() {
        let mut config = wt_config::HarnessConfig::default();
        assert_eq!(
            pinned_harness(&config, "wt", AgentTargetKind::Special),
            None
        );
        assert_eq!(
            select_harness(None, HarnessId::Codex, vec![HarnessId::Claude]),
            choose_live_harness(HarnessId::Codex, vec![HarnessId::Claude])
        );
        config.wt_session = Some(HarnessId::Claude);
        assert_eq!(
            pinned_harness(&config, "wt", AgentTargetKind::Special),
            Some(HarnessId::Claude)
        );
        assert_eq!(
            pinned_harness(&config, "wt", AgentTargetKind::Worktree),
            None
        );
        for slug in ["main", "manager", "dotfiles", "one"] {
            assert_eq!(
                pinned_harness(&config, slug, AgentTargetKind::Special),
                None
            );
        }
        assert_eq!(
            pinned_slot_harness(&config, SessionTarget::WtSource),
            Some(HarnessId::Claude)
        );
        assert_eq!(pinned_slot_harness(&config, SessionTarget::Main), None);
        // A live session of another harness is reported, never selected.
        let choice = select_harness(
            Some(HarnessId::Claude),
            HarnessId::Codex,
            vec![HarnessId::Codex],
        );
        assert_eq!(choice.selected, Some(HarnessId::Claude));
        assert_eq!(choice.source, SelectionSource::Pinned);
        assert_eq!(choice.live, Some(vec![HarnessId::Codex]));
    }

    #[tokio::test]
    async fn pinned_wt_session_wins_over_saved_primary_and_a_foreign_live_slot() {
        let mut fixture = crate::test_support::CommandFixture::new().await.unwrap();
        let wt_source = fixture._root.path().join("wt-source");
        fs::create_dir_all(&wt_source).unwrap();
        {
            let config = std::sync::Arc::make_mut(&mut fixture.ctx.config);
            config.tmux.socket = format!(
                "wt-pin-{}",
                fixture._root.path().file_name().unwrap().to_string_lossy()
            );
            config.paths.wt_source = Some(wt_source.clone());
            config.harness.primary = HarnessId::Claude;
            config.harness.wt_session = Some(HarnessId::Opencode);
        }
        persist_primary(&fixture.ctx.config.paths.cache_root, HarnessId::Codex).unwrap();
        let app = AppHarness::new(&fixture.ctx);
        // A live Codex session already owns the wt slot's Codex name.
        app.tmux
            .create_session(
                &CreateSession {
                    name: "wt-codex".into(),
                    cwd: wt_source.clone(),
                    command: vec!["sleep".into(), "30".into()],
                    width: None,
                    height: None,
                },
                &fixture.ctx.cancellation,
            )
            .await
            .unwrap();
        let result: Result<()> = async {
            let routes = app.routes(&fixture.ctx).await?;
            let wt = AppHarness::target_for("wt", &routes).context("wt route")?;
            assert_eq!(wt.target.kind, AgentTargetKind::Special);
            assert_eq!(wt.choice.selected, Some(HarnessId::Opencode));
            assert_eq!(wt.choice.source, SelectionSource::Pinned);
            assert_eq!(wt.choice.live, Some(vec![HarnessId::Codex]));
            // Every other slot keeps the saved Tab selection.
            let main = AppHarness::target_for("main", &routes).context("main route")?;
            assert_eq!(main.choice.selected, Some(HarnessId::Codex));
            assert_eq!(main.choice.source, SelectionSource::Primary);
            // Selection never stops the other harness's session.
            assert!(
                app.tmux
                    .session_exists("wt-codex", &fixture.ctx.cancellation)
                    .await?
            );
            Ok(())
        }
        .await;
        app.tmux
            .kill_server(&fixture.ctx.cancellation)
            .await
            .unwrap();
        fixture.close().await.unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn an_unretainable_first_attach_still_attaches_without_switching() {
        let mut fixture = crate::test_support::CommandFixture::new().await.unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config)
            .tmux
            .socket = format!(
            "wt-navigation-best-effort-{}",
            fixture._root.path().file_name().unwrap().to_string_lossy(),
        );
        let app = AppHarness::new(&fixture.ctx);
        let cwd = fixture.ctx.cwd.clone();
        // No such live session: retention fails, the attach proceeds.
        let missing = app.attach_command("one-codex", &cwd);
        let no_target = PreparedSession {
            program: "tmux".into(),
            args: Vec::new(),
            cwd: cwd.clone(),
        };
        for prepared in [missing, no_target] {
            let mut navigation = SessionNavigation::new(
                &fixture.ctx,
                Some("one".into()),
                SessionTarget::Harness,
                &prepared,
            )
            .await
            .unwrap();
            assert!(navigation.agent.is_none());
            assert!(navigation.attached.is_none());
            assert!(navigation.key.is_none());
            // Every session key returns to the board.
            for code in [110, 111, 112] {
                assert!(navigation.next(Some(code)).await.unwrap().is_none());
            }
        }
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn session_keys_keep_the_agent_identity_and_reject_a_replacement() {
        let mut fixture = crate::test_support::CommandFixture::new().await.unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config)
            .tmux
            .socket = format!(
            "wt-navigation-{}",
            fixture._root.path().file_name().unwrap().to_string_lossy(),
        );
        let app = AppHarness::new(&fixture.ctx);
        let agent =
            session_commands::resolve_target(&fixture.ctx, Some("one"), SessionTarget::Harness)
                .await
                .unwrap();
        let create = CreateSession {
            name: "one-codex".into(),
            cwd: agent.cwd.clone(),
            command: vec!["sleep".into(), "30".into()],
            width: None,
            height: None,
        };
        app.tmux
            .create_session(&create, &fixture.ctx.cancellation)
            .await
            .unwrap();
        let result: Result<()> = async {
            let original = app.session_inventory(&fixture.ctx).await?;
            let codex = original
                .iter()
                .find(|slot| slot.name == "one-codex")
                .unwrap();
            for primary in [HarnessId::Codex, HarnessId::Claude] {
                persist_primary(&fixture.ctx.config.paths.cache_root, primary)?;
                let prepared =
                    ui_session(&fixture.ctx, Some("one".into()), SessionTarget::Harness).await?;
                assert_eq!(
                    prepared.args.last().unwrap().to_string_lossy(),
                    format!("={}", codex.id)
                );
                assert_eq!(app.session_inventory(&fixture.ctx).await?, original);
            }
            let prepared = app.attach_command("one-codex", &agent.cwd);
            let mut navigation = SessionNavigation::new(
                &fixture.ctx,
                Some("one".into()),
                SessionTarget::Harness,
                &prepared,
            )
            .await?;
            assert!(navigation.next(Some(0)).await?.is_none());
            let shell = navigation.next(Some(110)).await?.unwrap();
            assert_eq!(shell.cwd, agent.cwd);
            assert_eq!(shell.args.last().unwrap(), "=one-shell");
            let resumed = navigation.next(Some(112)).await?.unwrap();
            assert_eq!(
                resumed.args.last().unwrap().to_string_lossy(),
                format!("={}", codex.id)
            );
            assert_eq!(
                navigation
                    .agent
                    .as_ref()
                    .and_then(|agent| agent.live_slot.clone()),
                Some(wt_tui::LiveSessionIdentity {
                    id: codex.id.clone(),
                    created_at: codex.created_at,
                })
            );
            app.tmux
                .kill_session("one-codex", &fixture.ctx.cancellation)
                .await?;
            app.tmux
                .create_session(&create, &fixture.ctx.cancellation)
                .await?;
            let error = navigation.next(Some(112)).await.unwrap_err();
            assert!(
                error.to_string().contains("live session changed"),
                "{error:#}"
            );
            Ok(())
        }
        .await;
        app.tmux
            .kill_server(&fixture.ctx.cancellation)
            .await
            .unwrap();
        fixture.close().await.unwrap();
        result.unwrap();
    }

    /// A handoff resolves its worktree and agent once. Its switch keys work
    /// from that resolution: they do not list worktrees again, and they do
    /// not follow a checkout that another process removed.
    #[tokio::test]
    async fn a_handoff_keeps_its_resolution_for_every_switch_key() {
        let mut fixture = crate::test_support::CommandFixture::new().await.unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config)
            .tmux
            .socket = format!(
            "wt-handoff-{}",
            fixture._root.path().file_name().unwrap().to_string_lossy(),
        );
        let app = AppHarness::new(&fixture.ctx);
        let agent =
            session_commands::resolve_target(&fixture.ctx, Some("one"), SessionTarget::Harness)
                .await
                .unwrap();
        app.tmux
            .create_session(
                &CreateSession {
                    name: "one-codex".into(),
                    cwd: agent.cwd.clone(),
                    command: vec!["sleep".into(), "30".into()],
                    width: None,
                    height: None,
                },
                &fixture.ctx.cancellation,
            )
            .await
            .unwrap();
        let result: Result<()> = async {
            let slot = app
                .session_inventory(&fixture.ctx)
                .await?
                .into_iter()
                .find(|slot| slot.name == "one-codex")
                .context("fixture slot")?;
            let (prepared, mut navigation) = enter_session(
                &fixture.ctx,
                Some("one".into()),
                SessionTarget::Harness,
                None,
            )
            .await?;
            assert_eq!(
                prepared.args.last().unwrap().to_string_lossy(),
                format!("={}", slot.id)
            );
            assert_eq!(
                navigation
                    .agent
                    .as_ref()
                    .and_then(|agent| agent.live_slot.clone()),
                Some(wt_tui::LiveSessionIdentity {
                    id: slot.id.clone(),
                    created_at: slot.created_at,
                })
            );
            // Git can no longer list this repository's worktrees.
            let git = fixture.ctx.config.paths.main_clone.join(".git");
            let moved = fixture.ctx.config.paths.main_clone.join("git-moved");
            fs::rename(&git, &moved)?;
            let lost =
                session_commands::resolve_target(&fixture.ctx, Some("one"), SessionTarget::Harness)
                    .await;
            let shell = navigation.next(Some(110)).await;
            let back = navigation.next(Some(112)).await;
            fs::rename(&moved, &git)?;
            assert!(lost.is_err(), "the fixture still lists worktrees");
            let shell = shell?.context("shell key returned to the board")?;
            assert_eq!(shell.cwd, agent.cwd);
            assert_eq!(shell.args.last().unwrap(), "=one-shell");
            let back = back?.context("agent key returned to the board")?;
            assert_eq!(
                back.args.last().unwrap().to_string_lossy(),
                format!("={}", slot.id)
            );
            let gone = agent.cwd.with_extension("gone");
            fs::rename(&agent.cwd, &gone)?;
            let removed = navigation.next(Some(111)).await;
            fs::rename(&gone, &agent.cwd)?;
            let error = removed.err().context("a removed checkout got a session")?;
            assert!(error.to_string().contains("no longer exists"), "{error:#}");
            assert!(
                !app.tmux
                    .session_exists("one-diff", &fixture.ctx.cancellation)
                    .await?
            );
            Ok(())
        }
        .await;
        app.tmux
            .kill_server(&fixture.ctx.cancellation)
            .await
            .unwrap();
        fixture.close().await.unwrap();
        result.unwrap();
    }

    /// The remote worker names its worktree by slug. Entry resolves it in the
    /// one inventory the session is prepared from and keeps it for the keys.
    #[tokio::test]
    async fn a_worker_enters_by_slug_and_switches_without_listing_worktrees_again() {
        let mut fixture = crate::test_support::CommandFixture::new().await.unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config)
            .tmux
            .socket = format!(
            "wt-by-slug-{}",
            fixture._root.path().file_name().unwrap().to_string_lossy(),
        );
        let app = AppHarness::new(&fixture.ctx);
        let agent =
            session_commands::resolve_target(&fixture.ctx, Some("one"), SessionTarget::Harness)
                .await
                .unwrap();
        app.tmux
            .create_session(
                &CreateSession {
                    name: "one-codex".into(),
                    cwd: agent.cwd.clone(),
                    command: vec!["sleep".into(), "30".into()],
                    width: None,
                    height: None,
                },
                &fixture.ctx.cancellation,
            )
            .await
            .unwrap();
        let result: Result<()> = async {
            let slot = app
                .session_inventory(&fixture.ctx)
                .await?
                .into_iter()
                .find(|slot| slot.name == "one-codex")
                .context("fixture slot")?;
            let missing =
                enter_session_by_slug(&fixture.ctx, "absent", SessionTarget::Harness, None).await;
            assert!(
                missing
                    .err()
                    .is_some_and(|error| error.to_string().contains("absent"))
            );
            let (prepared, mut navigation) =
                enter_session_by_slug(&fixture.ctx, "one", SessionTarget::Harness, None).await?;
            assert_eq!(
                prepared.args.last().unwrap().to_string_lossy(),
                format!("={}", slot.id)
            );
            let git = fixture.ctx.config.paths.main_clone.join(".git");
            let moved = fixture.ctx.config.paths.main_clone.join("git-moved");
            fs::rename(&git, &moved)?;
            let shell = navigation.next(Some(110)).await;
            let back = navigation.next(Some(112)).await;
            fs::rename(&moved, &git)?;
            assert_eq!(
                shell?.context("shell key")?.args.last().unwrap(),
                "=one-shell"
            );
            assert_eq!(
                back?
                    .context("agent key")?
                    .args
                    .last()
                    .unwrap()
                    .to_string_lossy(),
                format!("={}", slot.id)
            );
            Ok(())
        }
        .await;
        app.tmux
            .kill_server(&fixture.ctx.cancellation)
            .await
            .unwrap();
        fixture.close().await.unwrap();
        result.unwrap();
    }

    /// Entering a cold Claude session returns once its tmux slot exists. A
    /// Claude stopped at a startup prompt never registers, and the person
    /// attached to the pane is the one who can answer it.
    #[cfg(unix)]
    #[tokio::test]
    async fn entering_a_cold_claude_session_does_not_wait_for_it_to_register() {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = crate::test_support::CommandFixture::new().await.unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config)
            .tmux
            .socket = format!(
            "wt-cold-claude-{}",
            fixture._root.path().file_name().unwrap().to_string_lossy(),
        );
        let bin = fixture._root.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("claude"), "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        fs::set_permissions(bin.join("claude"), fs::Permissions::from_mode(0o700)).unwrap();
        let mut app = AppHarness::new(&fixture.ctx);
        app.service = app
            .service
            .clone()
            .with_spawn_path(format!("{}:/usr/bin:/bin", bin.display()));
        let agent =
            session_commands::resolve_target(&fixture.ctx, Some("one"), SessionTarget::Harness)
                .await
                .unwrap();
        let result: Result<()> = async {
            let attached = agent_session(
                &fixture.ctx,
                &app,
                &agent,
                Some("one".into()),
                SessionTarget::Harness,
                Some(HarnessId::Claude),
                None,
            )
            .await?;
            assert_eq!(attached.prepared.args.last().unwrap(), "=one");
            let slot = attached.slot.clone().context("the started slot")?;
            // The unregistered slot is still the one the session keys return to.
            let navigation = SessionNavigation::begin(
                &fixture.ctx,
                app.clone(),
                Some("one".into()),
                SessionTarget::Harness,
                agent.clone(),
                Some((slot.clone(), attached.live)),
            );
            assert_eq!(
                navigation
                    .agent
                    .as_ref()
                    .and_then(|agent| agent.live_slot.as_ref())
                    .map(|identity| identity.id.clone()),
                Some(slot)
            );
            Ok(())
        }
        .await;
        app.tmux
            .kill_server(&fixture.ctx.cancellation)
            .await
            .unwrap();
        fixture.close().await.unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn pi_existing_attach_paths_reject_an_unowned_same_name_slot() {
        let mut fixture = crate::test_support::CommandFixture::new().await.unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config)
            .tmux
            .socket = format!(
            "wt-pi-adoption-{}",
            fixture._root.path().file_name().unwrap().to_string_lossy()
        );
        let app = AppHarness::new(&fixture.ctx);
        app.tmux
            .create_session(
                &CreateSession {
                    name: "one-pi".into(),
                    cwd: fixture.ctx.cwd.clone(),
                    command: vec!["sleep".into(), "30".into()],
                    width: None,
                    height: None,
                },
                &fixture.ctx.cancellation,
            )
            .await
            .unwrap();
        app.tmux
            .set_option(
                &wt_tmux::OptionScope::Session("one-pi".into()),
                "@wt-harness-session-id",
                Some("foreign-thread"),
                &fixture.ctx.cancellation,
            )
            .await
            .unwrap();
        let direct = ui_session_with_harness(
            &fixture.ctx,
            Some("one".into()),
            SessionTarget::Harness,
            Some(HarnessId::Pi),
        )
        .await;
        let selection = wt_tui::SessionSelection {
            key: Some("one".into()),
            target: SessionTarget::Harness,
            harness: HarnessId::Pi,
            session_id: Some("foreign-thread".into()),
            live_slot: None,
            managed_name: None,
            mode: wt_tui::SessionMode::Resume,
            live: true,
            seen_live: None,
        };
        let picked = prepare_session(&fixture.ctx, &selection).await;
        let slot = app
            .session_inventory(&fixture.ctx)
            .await
            .unwrap()
            .into_iter()
            .find(|slot| slot.name == "one-pi")
            .unwrap();
        let unknown = prepare_session(
            &fixture.ctx,
            &wt_tui::SessionSelection {
                session_id: None,
                live_slot: Some(wt_tui::LiveSessionIdentity {
                    id: slot.id,
                    created_at: slot.created_at,
                }),
                ..selection
            },
        )
        .await;
        let retained = app
            .tmux
            .session_exists("one-pi", &fixture.ctx.cancellation)
            .await
            .unwrap();
        app.tmux
            .kill_server(&fixture.ctx.cancellation)
            .await
            .unwrap();
        fixture.close().await.unwrap();
        assert!(
            direct.is_err(),
            "F12 must validate the live Pi endpoint before attach"
        );
        assert!(
            picked.is_err(),
            "picker attach must validate the live Pi endpoint"
        );
        assert!(
            unknown.is_err(),
            "unknown live-slot attach must also validate ownership"
        );
        assert!(
            retained,
            "an unowned session must not be replaced or stopped"
        );
    }

    #[test]
    fn pi_session_paths_follow_native_override_precedence() {
        let home = Path::new("/isolated-home");
        let cache = Path::new("/isolated-cache");
        let paths = |sessions, agent| pi_paths_with_overrides(home, cache, sessions, agent);
        assert_eq!(paths(None, None).sessions, home.join(".pi/agent/sessions"));
        assert_eq!(
            paths(None, Some("/managed/pi".into())).sessions,
            Path::new("/managed/pi/sessions")
        );
        assert_eq!(
            paths(Some("/explicit".into()), Some("/managed/pi".into())).sessions,
            Path::new("/explicit")
        );
        assert_eq!(
            paths(Some("".into()), Some("".into())).sessions,
            home.join(".pi/agent/sessions")
        );
    }

    #[tokio::test]
    async fn message_wakes_prepare_exact_worktree_and_special_root_trust_before_dispatch() {
        let fixture = crate::test_support::CommandFixture::new().await.unwrap();
        let mut app = AppHarness::new(&fixture.ctx);
        // Do not inherit the test runner's selected CODEX_HOME.
        app.codex_config = fixture.ctx.home.join("selected-codex/config.toml");
        for (slug, cwd) in [
            ("one", fixture.ctx.cwd.clone()),
            ("manager", fixture.ctx.config.paths.main_clone.clone()),
        ] {
            let route = AgentRoute {
                target: special_target(slug, cwd.clone(), None),
                choice: HarnessChoice {
                    selected: Some(HarnessId::Codex),
                    source: SelectionSource::Primary,
                    live: Some(Vec::new()),
                },
            };
            // Empty text stops at the adapter dispatch boundary. It exercises
            // the formerly bypassed send preparation without starting a real
            // agent or sending a message to any account.
            let error = app.send(&route, "", None, &fixture.ctx).await.unwrap_err();
            assert!(format!("{error:#}").contains("empty"));
            let config: toml::Value =
                toml::from_str(&fs::read_to_string(&app.codex_config).unwrap()).unwrap();
            let canonical = fs::canonicalize(cwd).unwrap();
            assert_eq!(
                config["projects"][canonical.to_str().unwrap()]["trust_level"].as_str(),
                Some("trusted")
            );
        }
        assert!(!fixture.ctx.home.join(".codex/config.toml").exists());
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_send_is_recorded_in_the_worktree_history_and_never_for_a_special_session() {
        let fixture = crate::test_support::CommandFixture::new().await.unwrap();
        let mut app = AppHarness::new(&fixture.ctx);
        app.codex_config = fixture.ctx.home.join("selected-codex/config.toml");
        let route = |slug: &str, kind, cwd: PathBuf| AgentRoute {
            target: AgentTarget {
                kind,
                ..special_target(slug, cwd, None)
            },
            choice: HarnessChoice {
                selected: Some(HarnessId::Codex),
                source: SelectionSource::Primary,
                live: Some(Vec::new()),
            },
        };
        let worktree = route("one", AgentTargetKind::Worktree, fixture.ctx.cwd.clone());
        let manager = route(
            "manager",
            AgentTargetKind::Special,
            fixture.ctx.config.paths.main_clone.clone(),
        );
        // Empty text fails at the adapter boundary without starting an agent.
        let record = MessageRecord {
            from: Some("two"),
            text: Some("start"),
        };
        app.send_recorded(&worktree, "", Some("two"), false, record, &fixture.ctx)
            .await
            .unwrap_err();
        app.send(&manager, "", Some("two"), &fixture.ctx)
            .await
            .unwrap_err();
        let (one, special) = fixture
            .ctx
            .database
            .call(|store| {
                Ok((
                    store.read_worktree_events("one", 80)?,
                    store.read_worktree_events("manager", 80)?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(
            one.into_iter().map(|event| event.event).collect::<Vec<_>>(),
            [wt_core::WorktreeEventKind::Message {
                from: Some("two".into()),
                text: "start".into(),
                delivered: Some(false),
            }]
        );
        assert!(special.is_empty());
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn each_delivery_outcome_is_recorded_as_what_is_known_about_arrival() {
        use wt_harness::{ClaudeMessageOutcome, CodexMessageOutcome, PiSendOutcome};
        let fixture = crate::test_support::CommandFixture::new().await.unwrap();
        let route = AgentRoute {
            target: AgentTarget {
                kind: AgentTargetKind::Worktree,
                ..special_target("one", fixture.ctx.cwd.clone(), None)
            },
            choice: HarnessChoice {
                selected: Some(HarnessId::Codex),
                source: SelectionSource::Primary,
                live: Some(Vec::new()),
            },
        };
        let outcomes: [(Result<HarnessMessageOutcome>, Option<bool>); 5] = [
            // Codex owns the message: it arrived.
            (
                Ok(HarnessMessageOutcome::Codex(
                    CodexMessageOutcome::CliQueued {
                        thread_id: "thread".into(),
                    },
                )),
                Some(true),
            ),
            // Submitted to a harness that gives no receipt.
            (
                Ok(HarnessMessageOutcome::Pi(PiSendOutcome {
                    cold_started: false,
                })),
                None,
            ),
            (
                Ok(HarnessMessageOutcome::Codex(
                    CodexMessageOutcome::Ambiguous {
                        reason: "reply lost".into(),
                    },
                )),
                None,
            ),
            (
                Ok(HarnessMessageOutcome::Claude(
                    ClaudeMessageOutcome::Failed {
                        reason: "pane closed".into(),
                        maybe_submitted: true,
                    },
                )),
                None,
            ),
            (
                Ok(HarnessMessageOutcome::Claude(
                    ClaudeMessageOutcome::Failed {
                        reason: "no session".into(),
                        maybe_submitted: false,
                    },
                )),
                Some(false),
            ),
        ];
        let expected: Vec<_> = outcomes.iter().map(|(_, delivered)| *delivered).collect();
        for (outcome, _) in &outcomes {
            let record = MessageRecord {
                from: Some("manager"),
                text: None,
            };
            record_message(
                &route,
                "please rebase\nand more",
                record,
                outcome,
                &fixture.ctx,
            )
            .await;
        }
        let recorded = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_worktree_events("one", 80)?))
            .await
            .unwrap();
        let delivered: Vec<_> = recorded
            .iter()
            .map(|event| match &event.event {
                wt_core::WorktreeEventKind::Message {
                    from,
                    text,
                    delivered,
                } => {
                    assert_eq!(from.as_deref(), Some("manager"));
                    assert_eq!(text, "please rebase");
                    *delivered
                }
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(delivered, expected);
        fixture.close().await.unwrap();
    }

    /// A send refused before any adapter is asked says so in its type, so a
    /// wake is retried instead of being recorded as possibly delivered.
    #[tokio::test]
    async fn a_send_refused_before_delivery_is_marked_not_sent() {
        let fixture = crate::test_support::CommandFixture::new().await.unwrap();
        let mut app = AppHarness::new(&fixture.ctx);
        app.codex_config = fixture.ctx.home.join("selected-codex/config.toml");
        let route = |slug: &str, selected| AgentRoute {
            target: special_target(slug, fixture.ctx.cwd.clone(), None),
            choice: HarnessChoice {
                selected,
                source: SelectionSource::Primary,
                live: None,
            },
        };
        for refused in [
            route("one", None),
            // Not this worktree's checkout: Codex preparation refuses it.
            route("unregistered", Some(HarnessId::Codex)),
        ] {
            let error = app
                .send_unrecorded(&refused, "hello", &fixture.ctx)
                .await
                .unwrap_err();
            assert!(error.is::<NotSent>(), "{error:#}");
        }
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn codex_preparation_rejects_unowned_and_mismatched_roots() {
        let fixture = crate::test_support::CommandFixture::new().await.unwrap();
        let mut app = AppHarness::new(&fixture.ctx);
        app.codex_config = fixture.ctx.home.join("selected-codex/config.toml");
        for (slug, cwd) in [
            ("unregistered", fixture.ctx.cwd.clone()),
            ("one", fixture.ctx.config.paths.main_clone.clone()),
            ("manager", fixture.ctx.cwd.clone()),
        ] {
            assert!(
                app.prepare_start(HarnessId::Codex, slug, &cwd, &fixture.ctx)
                    .await
                    .is_err()
            );
            assert!(!app.codex_config.exists());
        }
        fixture.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_preparation_continues_when_trust_config_is_denied() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = crate::test_support::CommandFixture::new().await.unwrap();
        let mut app = AppHarness::new(&fixture.ctx);
        let dir = fixture.ctx.home.join("sandboxed-codex");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), "model = 'x'\n").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        app.codex_config = dir.join("config.toml");
        let result = app
            .prepare_start(HarnessId::Codex, "one", &fixture.ctx.cwd, &fixture.ctx)
            .await;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("config.toml")).unwrap(),
            "model = 'x'\n"
        );
        // Authorization still fails closed; only the trust write degrades.
        assert!(
            app.prepare_start(
                HarnessId::Codex,
                "unregistered",
                &fixture.ctx.cwd,
                &fixture.ctx
            )
            .await
            .is_err()
        );
        fixture.close().await.unwrap();
    }

    #[test]
    fn managed_session_variables_follow_the_worker_into_agent_sessions() {
        let inherited = BTreeMap::from([
            ("PATH".into(), "/usr/bin".into()),
            ("CODEX_HOME".into(), "/m/codex".into()),
            ("WT_MANAGED_ROOT".into(), "/m".into()),
            ("CLOUDFLARE_API_TOKEN".into(), "secret".into()),
            ("UNLISTED".into(), "x".into()),
            ("WT_SESSION_ENV".into(), "CODEX_HOME:PATH:MISSING".into()),
        ]);
        let env = harness_session_environment(
            Path::new("/users/test"),
            Path::new("/repo/worktree"),
            None,
            Some(Path::new("/opt/wt/bin/wt")),
            &inherited,
        );
        assert_eq!(env["CODEX_HOME"], "/m/codex");
        assert_eq!(env["PATH"], "/opt/wt/bin:/usr/bin");
        assert!(!env.contains_key("CLOUDFLARE_API_TOKEN"));
        assert!(!env.contains_key("UNLISTED"));
        assert!(!env.contains_key("MISSING"));
        assert_eq!(env["WT_SESSION_ENV"], "CODEX_HOME:PATH:MISSING");
        assert_eq!(env[wt_tmux::SESSION_SOURCE], "/m/secrets/env.sh");
    }

    #[test]
    fn repeated_harness_environment_keeps_runtime_first_without_path_growth() {
        let mut inherited = BTreeMap::from([(
            "PATH".into(),
            "/shim:/runtime/bin:/usr/bin:/shim::/bin:".into(),
        )]);
        for _ in 0..25 {
            inherited = harness_session_environment(
                Path::new("/users/test"),
                Path::new("/repo"),
                None,
                Some(Path::new("/runtime/bin/wt")),
                &inherited,
            );
            assert_eq!(inherited["PATH"], "/runtime/bin:/shim:/usr/bin::/bin");
        }
    }

    #[test]
    fn harness_environment_pins_selectors_and_prepends_running_binary_path() {
        let inherited = BTreeMap::from([
            ("PATH".into(), "/shim:/usr/bin".into()),
            ("WT_CONFIG".into(), "~/profiles/one.toml".into()),
            ("WT_REPO_CONFIG".into(), "/stale/other.wt.toml".into()),
            ("XDG_CONFIG_HOME".into(), "/stale/xdg".into()),
            ("WT_INSTALL_ROOT".into(), "/release/wt".into()),
        ]);
        let env = harness_session_environment(
            Path::new("/users/test"),
            Path::new("/repo/worktree"),
            Some(Path::new("/repo/main/.wt.toml")),
            Some(Path::new("/release/wt/versions/abc/bin/wt")),
            &inherited,
        );
        assert_eq!(env["PATH"], "/release/wt/versions/abc/bin:/shim:/usr/bin");
        assert_eq!(env["WT_CONFIG"], "/users/test/profiles/one.toml");
        assert_eq!(env["WT_REPO_CONFIG"], "/repo/main/.wt.toml");
        assert_eq!(env["XDG_CONFIG_HOME"], "/stale/xdg");
        assert_eq!(env["WT_INSTALL_ROOT"], "/release/wt");
        assert_eq!(env["HOME"], "/users/test");

        let defaulted = harness_session_environment(
            Path::new("/users/test"),
            Path::new("/repo/worktree"),
            None,
            Some(Path::new("/opt/wt/bin/wt")),
            &BTreeMap::from([("PATH".into(), "/usr/bin".into())]),
        );
        assert_eq!(defaulted["WT_CONFIG"], "/users/test/.config/wt/config.toml");
        assert_eq!(defaulted["WT_REPO_CONFIG"], "");
        assert_eq!(defaulted["XDG_CONFIG_HOME"], "/users/test/.config");
        assert_eq!(defaulted["WT_INSTALL_ROOT"], "/users/test/.local/share/wt");
    }

    #[test]
    fn live_target_matches_typescript_identity_fixture() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../test/compat/target-identity.json"))
                .unwrap();
        let input = &fixture["input"];
        let names = serde_json::from_value(input["names"].clone()).unwrap();
        let known = serde_json::from_value(input["knownSlugs"].clone()).unwrap();
        let primary = serde_json::from_value(input["primary"].clone()).unwrap();
        let choice = choose_live_harness(
            primary,
            live_harnesses(input["slug"].as_str().unwrap(), &names, &known, &[]),
        );
        assert_eq!(choice.source, SelectionSource::Live);
        assert_eq!(
            serde_json::json!({
                "harnessId": choice.selected, "source": "live", "liveHarnesses": choice.live,
            }),
            fixture["expected"]
        );
    }

    #[test]
    fn exact_branch_match_wins_over_an_earlier_last_segment_slug() {
        let route = |slug: &str, branch: &str| AgentRoute {
            target: AgentTarget {
                slug: slug.into(),
                kind: AgentTargetKind::Worktree,
                branch: Some(branch.into()),
                cwd: PathBuf::from("/w").join(slug),
                managed_name: None,
                remote: false,
            },
            choice: HarnessChoice {
                selected: Some(HarnessId::Claude),
                source: SelectionSource::Primary,
                live: Some(Vec::new()),
            },
        };
        let routes = [route("foo", "foo"), route("team-foo", "team/foo")];
        let target = |name| AppHarness::target_for(name, &routes).map(|r| r.target.slug.as_str());
        assert_eq!(target("team/foo"), Some("team-foo"));
        assert_eq!(target("foo"), Some("foo"));
        assert_eq!(target("team-foo"), Some("team-foo"));
        assert_eq!(target("other/foo"), Some("foo"));
        assert_eq!(target("missing"), None);
    }

    #[test]
    fn automatic_resume_never_borrows_another_slots_conversation() {
        let session = |id: &str, active: i64, primary: bool| wt_harness::HarnessSession {
            display_name: id.into(),
            session_id: id.into(),
            tmux_session_name: "manager-opencode".into(),
            last_active_ms: Some(active),
            is_live: false,
            extras: wt_harness::HarnessExtras {
                managed_name: primary.then(|| "primary".into()),
                ..Default::default()
            },
        };
        let sessions = [
            session("main-live", 30, true),
            session("another-live", 20, false),
            session("available", 10, false),
        ];
        let mut owned = BTreeSet::from(["main-live".into(), "another-live".into()]);
        assert_eq!(
            primary_single_slot_session(&sessions, &owned)
                .unwrap()
                .session_id,
            "available"
        );
        owned.insert("available".into());
        assert!(primary_single_slot_session(&sessions, &owned).is_none());
        owned.clear();
        assert_eq!(
            primary_single_slot_session(&sessions, &owned)
                .unwrap()
                .session_id,
            "main-live"
        );
    }

    #[test]
    fn manager_claude_slot_has_distinct_identity() {
        assert_eq!(
            session_name(
                &special_target("manager", PathBuf::from("/main"), Some("manager".into())),
                HarnessId::Claude
            ),
            "manager~manager"
        );
    }

    #[test]
    fn routing_prefers_live_non_primary_and_guards_codex_name_collisions() {
        let names = ["branch-codex".to_owned()].into_iter().collect();
        let known = ["branch".to_owned(), "branch-codex".to_owned()]
            .into_iter()
            .collect();
        let live = live_harnesses("branch", &names, &known, &[]);
        assert!(live.is_empty());
        let names = ["branch-opencode".to_owned()].into_iter().collect();
        assert_eq!(
            live_harnesses("branch", &names, &known, &[]),
            [HarnessId::Opencode]
        );
        let mut known = known;
        known.insert("branch-opencode".into());
        assert!(live_harnesses("branch", &names, &known, &[]).is_empty());
        let names = ["branch-pi".to_owned()].into_iter().collect();
        assert_eq!(
            live_harnesses("branch", &names, &known, &[]),
            [HarnessId::Pi]
        );
        known.insert("branch-pi".into());
        assert!(live_harnesses("branch", &names, &known, &[]).is_empty());
    }

    /// Every cold-start entry for the `wt` slot runs the configured helper
    /// before creating anything, and a failing helper refuses the start
    /// rather than launching Claude on the normal login. The helper fails, so
    /// no real Claude process or account is involved.
    #[cfg(unix)]
    #[tokio::test]
    async fn wt_slot_cold_starts_select_the_account_and_refuse_on_helper_failure() {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = crate::test_support::CommandFixture::new().await.unwrap();
        let root = fixture._root.path().to_owned();
        let wt_source = root.join("wt-source");
        fs::create_dir_all(&wt_source).unwrap();
        let runs = root.join("helper-runs");
        let helper = root.join("token-helper");
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\necho run >> '{}'\necho 'stderr sk-secret' >&2\nexit 9\n",
                runs.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        {
            let config = std::sync::Arc::make_mut(&mut fixture.ctx.config);
            config.tmux.socket = format!("wt-auth-{}", root.file_name().unwrap().to_string_lossy());
            config.paths.wt_source = Some(wt_source.clone());
            config.harness.wt_session = Some(HarnessId::Claude);
            config.harness.wt_session_claude_token_command =
                Some(vec![helper.to_string_lossy().into_owned()]);
        }
        let ctx = &fixture.ctx;
        let auth = claude_launch_auth(ctx).expect("configured selector");
        let target = |slug: &str, cwd: &Path| wt_harness::ClaudeSessionTarget {
            slug: slug.into(),
            cwd: cwd.to_owned(),
            managed_name: None,
        };
        assert!(auth.applies_to(&target("wt", &wt_source)));
        assert!(!auth.applies_to(&target("main", &ctx.config.paths.main_clone)));
        assert!(!auth.applies_to(&target("wt", &ctx.cwd)));
        assert!(!auth.applies_to(&target("one", &wt_source)));

        let app = AppHarness::new(ctx);
        let result: Result<()> = async {
            // TUI slot entry.
            let entry = ui_session_with_harness(ctx, None, SessionTarget::WtSource, None)
                .await
                .err()
                .context("slot entry started without a token")?;
            // `wt agent send wt`, prompt actions and automations all deliver
            // through `AppHarness::send`, which cold-starts via the messenger.
            let route = AgentRoute {
                target: special_target("wt", wt_source.clone(), None),
                choice: HarnessChoice {
                    selected: Some(HarnessId::Claude),
                    source: SelectionSource::Pinned,
                    live: Some(Vec::new()),
                },
            };
            // Refused before anything could be submitted, and reported so.
            let send = match app.send(&route, "hello", None, ctx).await? {
                HarnessMessageOutcome::Claude(wt_harness::ClaudeMessageOutcome::Failed {
                    reason,
                    maybe_submitted: false,
                }) => reason,
                other => bail!("message wake started without a token: {other:?}"),
            };
            for error in [format!("{entry:#}"), send] {
                assert!(error.contains("exited with status 9"), "{error}");
                assert!(error.contains("did not fall back"), "{error}");
                assert!(!error.contains("sk-secret"), "{error}");
            }
            assert_eq!(fs::read_to_string(&runs)?.lines().count(), 2);
            assert!(!app.tmux.session_exists("wt", &ctx.cancellation).await?);
            Ok(())
        }
        .await;
        let _ = app.tmux.kill_server(&ctx.cancellation).await;

        // Without the selector, or without a wt source, nothing is selected.
        {
            let config = std::sync::Arc::make_mut(&mut fixture.ctx.config);
            config.harness.wt_session_claude_token_command = None;
        }
        assert!(claude_launch_auth(&fixture.ctx).is_none());
        {
            let config = std::sync::Arc::make_mut(&mut fixture.ctx.config);
            config.harness.wt_session_claude_token_command = Some(vec!["helper".into()]);
            config.paths.wt_source = None;
        }
        assert!(claude_launch_auth(&fixture.ctx).is_none());
        fixture.close().await.unwrap();
        result.unwrap();
    }
}
