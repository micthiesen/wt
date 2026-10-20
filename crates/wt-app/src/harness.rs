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
pub(crate) use session_commands::{
    cached_session_options, picker_entries, picker_slug, visible_harnesses,
};
use session_commands::{session_name, slot_name};

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

/// Session keys keep the same worktree while the board is suspended.
/// Retain the selected agent identity and use the normal resume checks.
pub(crate) struct SessionNavigation<'a> {
    context: &'a AppContext,
    key: Option<String>,
    agent: Option<wt_tui::SessionSelection>,
}

impl<'a> SessionNavigation<'a> {
    pub(crate) async fn new(
        context: &'a AppContext,
        key: Option<String>,
        target: SessionTarget,
        prepared: &PreparedSession,
    ) -> Result<Self> {
        let mut navigation = Self {
            context,
            key,
            agent: None,
        };
        // Switching is a convenience: the first attach is already prepared
        // and must not be refused because its identity cannot be retained.
        // Without a retained agent the session keys return to the board, so
        // a later F12 can never land on a different agent. Re-entry through
        // `next` stays fail-closed.
        if target == SessionTarget::Harness
            && navigation.key.is_some()
            && let Err(error) = navigation.retain_agent(prepared).await
        {
            tracing::warn!(error = %format!("{error:#}"), "session switching disabled for this attach");
            navigation.agent = None;
            navigation.key = None;
        }
        Ok(navigation)
    }

    async fn retain_agent(&mut self, prepared: &PreparedSession) -> Result<()> {
        let app = AppHarness::new(self.context);
        let agent = session_commands::resolve_target(
            self.context,
            self.key.as_deref(),
            SessionTarget::Harness,
        )
        .await?;
        let live = app.session_inventory(self.context).await?;
        let attached = prepared
            .args
            .last()
            .and_then(|arg| arg.to_str())
            .and_then(|arg| arg.strip_prefix('='))
            .context("agent attachment has no exact tmux target")?;
        let slot = live
            .iter()
            .find(|slot| slot.id == attached || slot.name == attached)
            .context("agent session ended before attachment")?;
        self.agent = session_commands::picker_entries(
            &HarnessId::ALL,
            &agent,
            self.key.clone(),
            SessionTarget::Harness,
            &[],
            &live,
        )
        .into_iter()
        .find(|entry| {
            entry
                .selection
                .live_slot
                .as_ref()
                .is_some_and(|identity| identity.id == slot.id)
        })
        .map(|entry| entry.selection);
        if self.agent.is_none() {
            bail!("agent attachment does not belong to the selected worktree");
        }
        Ok(())
    }

    pub(crate) async fn next(&mut self, code: Option<i32>) -> Result<Option<PreparedSession>> {
        let target = match code {
            Some(110) => SessionTarget::Shell,
            Some(111) => SessionTarget::Diff,
            Some(112) => SessionTarget::Harness,
            _ => return Ok(None),
        };
        // Special slots have no worktree key. Keep their board-return behavior.
        if self.key.is_none() {
            return Ok(None);
        }
        if target == SessionTarget::Harness
            && let Some(agent) = &self.agent
        {
            return prepare_session(self.context, agent).await.map(Some);
        }
        let prepared = ui_session(self.context, self.key.clone(), target).await?;
        if target == SessionTarget::Harness {
            self.retain_agent(&prepared).await?;
        }
        Ok(Some(prepared))
    }
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
pub(crate) fn claude_launch_auth(context: &AppContext) -> Option<wt_harness::ClaudeLaunchAuth> {
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
pub(crate) fn claude_launch_args(context: &AppContext) -> Option<wt_harness::ClaudeLaunchArgs> {
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
                .with_launch_args(launch_args.clone());
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
        .with_claude_launch_args(launch_args);
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
    pub(crate) fn attach_command_as(
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
    pub(crate) fn pinned(&self, slug: &str, kind: AgentTargetKind) -> Option<HarnessId> {
        pinned_harness(&self.harness_config, slug, kind)
    }

    /// The harness a new start or prompt for this target uses when no live
    /// session decides it: the configured pin, else the effective primary.
    pub(crate) fn default_for(&self, slug: &str, kind: AgentTargetKind) -> HarnessId {
        self.pinned(slug, kind).unwrap_or_else(|| self.primary())
    }

    pub async fn routes(&self, context: &AppContext) -> Result<Vec<AgentRoute>> {
        let records = context.repository.inventory(&context.cancellation).await?;
        let targets = self.targets_from_records(records.iter());
        let sessions = self.tmux.list_sessions(&context.cancellation).await;
        let (names, ids) = match sessions {
            Ok(sessions) => (
                Some(
                    sessions
                        .iter()
                        .map(|s| s.name.clone())
                        .collect::<BTreeSet<_>>(),
                ),
                sessions,
            ),
            Err(_) => (None, Vec::new()),
        };
        let known = targets.iter().map(|target| target.slug.clone()).collect();
        let primary = self.primary();
        Ok(targets
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
                        let live = live_harnesses(&target.slug, names, &known, &ids);
                        select_harness(self.pinned(&target.slug, target.kind), primary, live)
                    }
                };
                AgentRoute { target, choice }
            })
            .collect())
    }

    /// The slot agents a session picker can name, with the same checks as
    /// [`Self::slot_agent`]. Reads the checkout directories, never Git.
    pub(crate) fn slot_agents(&self) -> Vec<(wt_tui::SessionTarget, AgentTarget)> {
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
    pub(crate) fn slot_agent(&self, target: wt_tui::SessionTarget) -> Option<AgentTarget> {
        use wt_tui::SessionTarget as Slot;
        match target {
            Slot::Manager => Some(special_target(
                "manager",
                self.main_clone.clone(),
                Some("manager".into()),
            )),
            Slot::Main => Some(special_target("main", self.main_clone.clone(), None)),
            Slot::WtSource => self
                .wt_source
                .as_ref()
                .filter(|path| path.is_dir())
                .map(|path| special_target("wt", path.clone(), None)),
            Slot::Dotfiles => self
                .dotfiles
                .is_dir()
                .then(|| special_target("dotfiles", self.dotfiles.clone(), None)),
            Slot::Harness | Slot::Shell | Slot::Diff => None,
        }
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
        let id = route.choice.selected.ok_or_else(|| {
            anyhow::anyhow!(
                "could not inspect wt's tmux session registry; no harness was selected or started"
            )
        })?;
        self.prepare_start(id, &route.target.slug, &route.target.cwd, context)
            .await?;
        self.service
            .send(
                &HarnessTarget {
                    id,
                    slug: route.target.slug.clone(),
                    cwd: route.target.cwd.clone(),
                    managed_name: route.target.managed_name.clone(),
                    sender: sender.map(str::to_owned),
                    text: text.to_owned(),
                },
                &context.cancellation,
            )
            .await
            .context("deliver agent message")
    }

    /// A live Claude `wt` slot keeps the account it started with. Name the
    /// restart that would apply the configured selection, if one is needed.
    pub(crate) async fn claude_auth_restart_notice(
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

    pub(crate) async fn ensure_started(
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
            wt_harness::ensure_trusted_project(&config, &root)?;
            Ok(())
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

/// The one rule for configuration-pinned slot harnesses: only the `wt`
/// special session can be pinned, by `[harness] wt_session`. A worktree that
/// happens to be named `wt` is never pinned.
pub(crate) fn pinned_harness(
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
pub(crate) fn pinned_slot_harness(
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
    let default_diff_base = format!("origin/{}", context.config.branch.base);
    let (slug, cwd, managed_name, kind, diff_base) = match target {
        SessionTarget::Harness | SessionTarget::Shell | SessionTarget::Diff => {
            let key = key.context("worktree session requires a worktree key")?;
            let selected = session_commands::resolve_target(context, Some(&key), target).await?;
            let slug = selected.slug;
            let saved_base = if target == SessionTarget::Diff {
                let state = context
                    .database
                    .call(|store| Ok(store.read_wt_state()?))
                    .await?;
                state
                    .get("slugs")
                    .and_then(|slugs| slugs.get(&slug))
                    .and_then(|entry| entry.get("baseBranch"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            } else {
                None
            };
            let diff_base = saved_base
                .map(|base| format!("origin/{base}"))
                .unwrap_or_else(|| default_diff_base.clone());
            (slug, selected.cwd, None, target, diff_base)
        }
        SessionTarget::Manager => (
            "manager".into(),
            context.config.paths.main_clone.clone(),
            Some("manager".into()),
            target,
            default_diff_base.clone(),
        ),
        SessionTarget::Main => (
            "main".into(),
            context.config.paths.main_clone.clone(),
            None,
            target,
            default_diff_base.clone(),
        ),
        SessionTarget::WtSource => {
            let path = context
                .config
                .paths
                .wt_source
                .as_ref()
                .filter(|path| path.is_dir())
                .context(unavailable_source_message())?;
            (
                "wt".into(),
                path.clone(),
                None,
                target,
                default_diff_base.clone(),
            )
        }
        SessionTarget::Dotfiles => {
            let path = &context.config.paths.dotfiles;
            if !path.is_dir() {
                bail!(
                    "dotfiles session is unavailable: configured path {} does not exist",
                    path.display()
                );
            }
            (
                "dotfiles".into(),
                path.clone(),
                None,
                target,
                default_diff_base.clone(),
            )
        }
    };
    let session = match kind {
        SessionTarget::Shell => {
            let name = format!("{slug}-shell");
            if !app
                .tmux
                .session_exists(&name, &context.cancellation)
                .await?
            {
                let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
                app.tmux
                    .create_session(
                        &CreateSession {
                            name: name.clone(),
                            cwd: cwd.clone(),
                            command: vec![shell.to_string_lossy().into_owned()],
                            width: None,
                            height: None,
                        },
                        &context.cancellation,
                    )
                    .await?;
            }
            name
        }
        SessionTarget::Diff => {
            let name = format!("{slug}-diff");
            if !app
                .tmux
                .session_exists(&name, &context.cancellation)
                .await?
            {
                let command = resolve_diff_command(&context.config.diff.command, &diff_base);
                app.tmux
                    .create_session(
                        &CreateSession {
                            name: name.clone(),
                            cwd: cwd.clone(),
                            command: vec!["/bin/sh".into(), "-lc".into(), command],
                            width: None,
                            height: None,
                        },
                        &context.cancellation,
                    )
                    .await?;
            }
            name
        }
        SessionTarget::Harness
        | SessionTarget::Manager
        | SessionTarget::Main
        | SessionTarget::WtSource
        | SessionTarget::Dotfiles => {
            let harness_id = match selected_harness {
                Some(id) => id,
                None => app
                    .routes(context)
                    .await?
                    .into_iter()
                    .find(|route| route.target.slug == slug)
                    .and_then(|route| route.choice.selected)
                    .unwrap_or_else(|| {
                        let kind = if target == SessionTarget::Harness {
                            AgentTargetKind::Worktree
                        } else {
                            AgentTargetKind::Special
                        };
                        app.default_for(&slug, kind)
                    }),
            };
            let request = HarnessSpawnRequest {
                worktree_path: cwd.clone(),
                slug: slug.clone(),
                managed_name: managed_name.clone(),
                resume_session_id: None,
                display_label: None,
            };
            let tmux_name = slot_name(&slug, managed_name.as_deref(), harness_id);
            if !app
                .tmux
                .session_exists(&tmux_name, &context.cancellation)
                .await?
            {
                let discovered = app
                    .service
                    .discover(
                        harness_id,
                        &DiscoveryRequest {
                            slug: slug.clone(),
                            worktree_path: cwd.clone(),
                            live_session_id: None,
                        },
                        &[],
                        &context.cancellation,
                    )
                    .await?;
                let resume = if harness_id == HarnessId::Claude {
                    None
                } else {
                    // Special targets can share a checkout (manager and main).
                    // History's directory match cannot establish slot ownership.
                    let owned: BTreeSet<String> = app
                        .session_inventory(context)
                        .await?
                        .into_iter()
                        .filter(|slot| slot.name != tmux_name)
                        .filter_map(|slot| slot.harness_session_id)
                        .collect();
                    primary_single_slot_session(&discovered, &owned)
                        .map(|entry| entry.session_id.clone())
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
            } else if harness_id == HarnessId::Pi {
                app.validate_live_pi(&request, context).await?;
            }
            tmux_name
        }
    };
    let shortcut = match kind {
        SessionTarget::Shell => SessionShortcut::Shell,
        SessionTarget::Diff => SessionShortcut::Diff,
        _ => SessionShortcut::Harness,
    };
    Ok(app.attach_command_as(&session, &cwd, shortcut))
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
pub(crate) fn select_harness(
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
        let mut fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
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
            assert_eq!(
                app.default_for("wt", AgentTargetKind::Special),
                HarnessId::Opencode
            );
            assert_eq!(
                app.default_for("main", AgentTargetKind::Special),
                HarnessId::Codex
            );
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
        let mut fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
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
        let mut fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
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
            let selected = navigation
                .agent
                .as_ref()
                .unwrap()
                .live_slot
                .as_ref()
                .unwrap()
                .id
                .clone();
            let resumed = navigation.next(Some(112)).await?.unwrap();
            assert_eq!(
                resumed.args.last().unwrap().to_string_lossy(),
                format!("={selected}")
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

    #[tokio::test]
    async fn pi_existing_attach_paths_reject_an_unowned_same_name_slot() {
        let mut fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
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
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
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
    async fn codex_preparation_rejects_unowned_and_mismatched_roots() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
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
        let mut fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
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
            let send = app
                .send(&route, "hello", None, ctx)
                .await
                .err()
                .context("message wake started without a token")?;
            for error in [format!("{entry:#}"), format!("{send:#}")] {
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
