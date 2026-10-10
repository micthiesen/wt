//! Application boundary for harness selection, routing and messaging.
//!
//! The adapters own session formats; this module owns the app's paths, tmux
//! server and target selection. Tmux inventory failure is represented as
//! unknown and never treated as an empty server.

use std::ffi::OsString;
use std::{
    collections::BTreeSet,
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
use wt_tmux::{CreateSession, SessionInfo, TmuxClient, TmuxServer};
use wt_tui::SessionTarget;

use crate::context::AppContext;

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

#[derive(Clone)]
pub struct AppHarness {
    service: HarnessService,
    tmux: TmuxClient,
    claude_sessions: ClaudeSessionManager,
    claude_injector: ClaudeInjector,
    cache_root: PathBuf,
    primary_fallback: HarnessId,
    main_clone: PathBuf,
    dotfiles: PathBuf,
    wt_source: Option<PathBuf>,
}

impl AppHarness {
    pub fn new(context: &AppContext) -> Self {
        let tmux = TmuxClient::new(
            context.processes.clone(),
            TmuxServer::named(context.config.tmux.socket.clone()).with_cwd(context.home.clone()),
        );
        let cache_root = context.config.paths.cache_root.clone();
        let claude_paths = ClaudePaths::new(&context.home, &cache_root);
        let claude_sessions =
            ClaudeSessionManager::new(ClaudeHarness::new(claude_paths.clone()), tmux.clone());
        let claude_injector = ClaudeInjector::new(cache_root.clone());
        let service = HarnessService::new(
            claude_paths,
            CodexPaths::new(&context.home, &cache_root),
            OpenCodePaths::new(&context.home, &cache_root),
            context.processes.clone(),
            tmux.clone(),
        );
        Self {
            service,
            tmux,
            claude_sessions,
            claude_injector,
            cache_root,
            primary_fallback: context.config.harness.primary,
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

    pub fn attach_command(&self, session: &str, cwd: &Path) -> PreparedSession {
        PreparedSession {
            program: "tmux".into(),
            args: self.tmux.attach_session_args(session),
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

    pub async fn routes(&self, context: &AppContext) -> Result<Vec<AgentRoute>> {
        let snapshots = context
            .repository
            .inventory_status(&context.cancellation)
            .await?;
        let targets = self.targets(&snapshots);
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
                };
                AgentRoute { target, choice }
            })
            .collect())
    }

    pub fn targets(&self, snapshots: &[wt_vcs::WorktreeSnapshot]) -> Vec<AgentTarget> {
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
            snapshots
                .iter()
                .filter(|snapshot| !snapshot.worktree.is_main)
                .filter(|snapshot| !reserved.contains(snapshot.worktree.target.slug()))
                .map(|snapshot| AgentTarget {
                    slug: snapshot.worktree.target.slug().to_owned(),
                    kind: AgentTargetKind::Worktree,
                    branch: Some(snapshot.worktree.target.branch.clone()),
                    cwd: PathBuf::from(&snapshot.worktree.target.path),
                    managed_name: None,
                    remote: matches!(
                        snapshot.worktree.target.location(),
                        wt_core::WorktreeLocation::Remote { .. }
                    ),
                }),
        );
        targets
    }

    pub fn target_for<'a>(requested: &str, routes: &'a [AgentRoute]) -> Option<&'a AgentRoute> {
        routes.iter().find(|route| {
            route.target.slug == requested
                || route.target.branch.as_deref() == Some(requested)
                || requested
                    .rsplit('/')
                    .next()
                    .is_some_and(|slug| route.target.slug == slug)
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

fn session_name(target: &AgentTarget, id: HarnessId) -> String {
    match id {
        HarnessId::Claude if target.managed_name.as_deref() == Some("manager") => {
            format!("{}~manager", target.slug)
        }
        HarnessId::Claude => target.slug.clone(),
        HarnessId::Codex => format!("{}-codex", target.slug),
        HarnessId::Opencode => format!("{}-opencode", target.slug),
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
            let rows = context
                .repository
                .inventory_status(&context.cancellation)
                .await?;
            let row = rows
                .into_iter()
                .find(|row| wt_core::worktree_target_key(&row.worktree.target) == key)
                .context("selected worktree no longer exists")?;
            if row.worktree.is_main {
                bail!("main clone uses the dedicated Main session target");
            }
            if matches!(
                row.worktree.target.location(),
                wt_core::WorktreeLocation::Remote { .. }
            ) {
                bail!(
                    "remote session handoff requires the remote runtime; refusing to start a local session for this target"
                );
            }
            let slug = row.worktree.target.slug().to_owned();
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
            (
                slug,
                PathBuf::from(&row.worktree.target.path),
                None,
                target,
                diff_base,
            )
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
            let harness_id = selected_harness.unwrap_or_else(|| app.primary());
            let request = HarnessSpawnRequest {
                worktree_path: cwd.clone(),
                slug: slug.clone(),
                managed_name: managed_name.clone(),
                resume_session_id: None,
                display_label: None,
            };
            let tmux_name = match harness_id {
                HarnessId::Claude if managed_name.as_deref() == Some("manager") => {
                    format!("{slug}~manager")
                }
                HarnessId::Claude => slug.clone(),
                HarnessId::Codex => format!("{slug}-codex"),
                HarnessId::Opencode => format!("{slug}-opencode"),
            };
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
                    let desired = managed_name.as_deref().unwrap_or("primary");
                    discovered
                        .iter()
                        .find(|entry| entry.extras.managed_name.as_deref() == Some(desired))
                        .or_else(|| discovered.first())
                        .map(|entry| entry.session_id.clone())
                };
                app.service
                    .ensure_started(
                        harness_id,
                        &HarnessSpawnRequest {
                            resume_session_id: resume,
                            ..request
                        },
                        &context.cancellation,
                    )
                    .await?;
            }
            tmux_name
        }
    };
    Ok(app.attach_command(&session, &cwd))
}

fn resolve_diff_command(template: &str, base: &str) -> String {
    if !template.contains("{{base}}") {
        return template.to_owned();
    }
    let quoted = format!("\"{}\"", base.replace('"', "\\\""));
    template.replace("{{base}}", &quoted)
}

fn live_harnesses(
    slug: &str,
    names: &BTreeSet<String>,
    known_slugs: &BTreeSet<String>,
    sessions: &[SessionInfo],
) -> Vec<HarnessId> {
    let mut live = Vec::new();
    for id in HarnessId::ALL {
        let name = match id {
            HarnessId::Claude => slug.to_owned(),
            HarnessId::Codex => format!("{slug}-codex"),
            HarnessId::Opencode => format!("{slug}-opencode"),
        };
        // A Codex slot named exactly like another target belongs to that
        // target's Claude primary, not to this slug's Codex process.
        if (id != HarnessId::Codex || name == slug || !known_slugs.contains(&name))
            && names.contains(&name)
        {
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
    }
}
