//! Host-local tmux inventory and harness session discovery.
//!
//! This source deliberately consumes the already-batched local Git source. It
//! never scans Git per session and makes a single tmux inventory request per
//! refresh. Remote inventory is composed by the host boundary above this module.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wt_core::Domain;
use wt_core::HarnessId;
use wt_harness::{ClaudePaths, DiscoveryRequest, HarnessService, HarnessSession, OpenCodePaths};
use wt_runtime::{
    Confirmed, DiskCache, LeafHandle, LeafPublisher, Persistence, RefreshPolicy, SourceHandle,
    SourceSnapshot, SourceSpec, SourceState, Stamp, TaskScope, leaf_channel,
};
use wt_tmux::{SessionInfo, TmuxClient, TmuxServer};
use wt_vcs::WorktreeSnapshot;

use crate::{context::AppContext, harness::AppHarness};

const INVENTORY_INTERVAL: Duration = Duration::from_secs(5);
const MAX_DISCOVERY_WORKERS: usize = 4;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionKey {
    pub slug: String,
    pub harness: HarnessId,
    pub session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDescriptor {
    pub name: String,
    pub id: String,
    pub created_at: i64,
    pub attached_clients: u32,
    pub window_count: u32,
    pub harness_session_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSlot {
    pub slug: String,
    pub managed_name: Option<String>,
}

/// One tmux snapshot partitioned using the same suffix rules as the existing
/// session-name contract. `all` remains authoritative for exact liveness.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionKinds {
    pub claude: Vec<ClaudeSlot>,
    pub codex: BTreeSet<String>,
    pub opencode: BTreeSet<String>,
    #[serde(default)]
    pub pi: BTreeSet<String>,
    pub diff: BTreeSet<String>,
    pub shell: BTreeSet<String>,
    pub action: BTreeSet<String>,
    pub dev: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInventory {
    pub all: BTreeMap<String, SessionDescriptor>,
    pub kinds: SessionKinds,
    /// Exact resumed UUID by live tmux session name. This must win over a
    /// bounded recent-rollout scan for long-lived Codex sessions.
    pub harness_session_ids: BTreeMap<String, String>,
    /// Normalized terminal activity only; not transcript identity or idle proof.
    #[serde(default)]
    pub pane_activity: BTreeMap<String, String>,
}

impl SessionInventory {
    /// The running tmux sessions, with the identity fields a picker row
    /// witnesses. Built from the one inventory poll the board already holds.
    pub(crate) fn live_sessions(&self) -> Vec<wt_tmux::SessionInfo> {
        self.all
            .values()
            .map(|descriptor| wt_tmux::SessionInfo {
                name: descriptor.name.clone(),
                id: descriptor.id.clone(),
                created_at: descriptor.created_at,
                attached_clients: descriptor.attached_clients,
                window_count: descriptor.window_count,
                harness_session_id: descriptor.harness_session_id.clone(),
            })
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredSession {
    pub key: SessionKey,
    pub session: HarnessSession,
}

/// Vector DTO so the stable composite identity is representable in JSON/SSH
/// protocol messages. Consumers index by `key`, never by a display label.
pub type SessionDiscoveries = Vec<DiscoveredSession>;

#[derive(Clone)]
pub struct SessionCommands {
    discover: mpsc::Sender<(String, HarnessId)>,
}

impl SessionCommands {
    /// Request a history refresh for one host-local target. Returns false when
    /// the bounded command queue is full; discovery is read-only.
    pub fn discover(&self, slug: impl Into<String>, harness: HarnessId) -> bool {
        match self.discover.try_send((slug.into(), harness)) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

pub struct SessionSources {
    pub inventory: LeafHandle<SessionInventory>,
    pub discoveries: LeafHandle<SessionDiscoveries>,
    pub commands: SessionCommands,
    /// Shared host-local Git facts, passed through without rescanning.
    pub git: SourceHandle<Vec<WorktreeSnapshot>>,
}

pub fn start(
    scope: &TaskScope,
    context: &AppContext,
    git: LeafHandle<Vec<WorktreeSnapshot>>,
) -> SessionSources {
    let app = AppHarness::new(context);
    let tmux = TmuxClient::new(
        context.processes.clone(),
        TmuxServer::named(context.config.tmux.socket.clone()).with_cwd(context.home.clone()),
    );
    let (inventory, inventory_publisher) = leaf_channel(
        scope,
        SourceSpec {
            name: "session-inventory",
            domain: Domain::Sessions,
            persistence: Persistence::Never("live process state"),
            policy: RefreshPolicy::default(),
        },
    );
    spawn_inventory(scope, tmux, inventory_publisher);

    // Last known discoveries are shown at launch; automations still require a
    // live discovery pass (`confirmed()`) before treating a slug as idle.
    let (discoveries, publisher) = leaf_channel(
        scope,
        SourceSpec {
            name: "session-discoveries",
            domain: Domain::Sessions,
            persistence: Persistence::Disk(DiskCache::new(&context.caches)),
            policy: RefreshPolicy::default(),
        },
    );
    let (requests, request_rx) = mpsc::channel(32);
    let service = HarnessService::new(
        ClaudePaths::new(&context.home, &context.config.paths.cache_root),
        crate::harness::codex_paths(&context.home, &context.config.paths.cache_root),
        OpenCodePaths::new(&context.home, &context.config.paths.cache_root),
        crate::harness::pi_paths(&context.home, &context.config.paths.cache_root),
        context.processes.clone(),
        TmuxClient::new(
            context.processes.clone(),
            TmuxServer::named(context.config.tmux.socket.clone()).with_cwd(context.home.clone()),
        ),
    );
    spawn_discoveries(
        scope,
        app,
        service,
        context.cancellation.clone(),
        DiscoveryInputs {
            git: git.clone(),
            inventory: inventory.clone(),
        },
        publisher,
        request_rx,
    );

    SessionSources {
        inventory,
        discoveries,
        commands: SessionCommands { discover: requests },
        git: git.view(),
    }
}

fn spawn_inventory(
    scope: &TaskScope,
    tmux: TmuxClient,
    mut publisher: LeafPublisher<SessionInventory>,
) {
    let cancel = scope.token();
    scope.spawn(async move {
        let mut previous: Option<SessionInventory> = None;
        let mut failed_panes = BTreeSet::new();
        let mut healthy = false;
        let mut requests = tokio::time::interval(INVENTORY_INTERVAL);
        requests.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = requests.tick() => {},
                requested = publisher.requested() => if requested.is_none() { break; },
            }
            let observed = Stamp::now();
            let fetched = async {
                let sessions = tmux.list_sessions(&cancel).await?;
                let panes = if sessions.is_empty() { Vec::new() } else { tmux.list_all_panes(&cancel).await? };
                Ok::<_, wt_tmux::TmuxError>((sessions, panes))
            }.await;
            if cancel.is_cancelled() {
                break;
            }
            match fetched {
                Ok((sessions, panes)) => {
                    let now_failed = panes.iter().filter(|pane| pane.dead)
                        .map(|pane| (pane.id.clone(), pane.pid)).collect::<BTreeSet<_>>();
                    for pane in panes.iter().filter(|pane| pane.dead && !failed_panes.contains(&(pane.id.clone(), pane.pid))) {
                        tracing::warn!(target: "wt_attention",
                            event_channel = "attention",
                            event_source = %pane.session_name,
                            event_text = "Terminal command failed. Open the session to inspect its error; Enter retries, F12 returns to wt.",
                            "terminal command failed");
                    }
                    failed_panes = now_failed;
                    let inventory = running_session_inventory(sessions, &panes);
                    if previous.as_ref() != Some(&inventory) || !healthy {
                        previous = Some(inventory.clone());
                        publisher.publish_live(inventory, observed);
                    }
                    healthy = true;
                }
                Err(error) => {
                    healthy = false;
                    // Preserve last-known sessions while making uncertainty
                    // visible to consumers that make lifecycle decisions.
                    publisher.set_state(SourceState::Failed(
                        format!("tmux session inventory: {error}").into(),
                    ));
                }
            }
        }
    });
}

fn running_session_inventory(
    sessions: Vec<SessionInfo>,
    panes: &[wt_tmux::PaneInfo],
) -> SessionInventory {
    let live = panes
        .iter()
        .filter(|pane| !pane.dead)
        .map(|pane| pane.session_name.as_str())
        .collect::<BTreeSet<_>>();
    let mut inventory = classify_sessions(
        sessions
            .into_iter()
            .filter(|session| live.contains(session.name.as_str()))
            .collect(),
    );
    for name in inventory.all.keys() {
        let mut candidates = panes
            .iter()
            .filter(|pane| pane.session_name == *name && !pane.dead);
        if let Some(pane) = candidates.next()
            && candidates.next().is_none()
            && name.ends_with("-codex")
            && let Some(state) = codex_terminal_activity(&pane.title)
        {
            inventory.pane_activity.insert(name.clone(), state.into());
        }
    }
    inventory
}

/// Codex's explicit terminal activity prefixes (status_surfaces.rs). Absence
/// is not idle proof: users may disable/customize titles or animations.
fn codex_terminal_activity(title: &str) -> Option<&'static str> {
    if ["[ ! ] Action Required", "[ . ] Action Required"]
        .iter()
        .any(|prefix| {
            title == *prefix
                || title
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with(' '))
        })
    {
        return Some("asking");
    }
    let (first, rest) = title.split_once(' ')?;
    (!rest.is_empty() && ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"].contains(&first))
        .then_some("working")
}

fn classify_sessions(sessions: Vec<SessionInfo>) -> SessionInventory {
    let mut result = SessionInventory::default();
    for session in sessions {
        let name = session.name.clone();
        if let Some(id) = session.harness_session_id.as_ref() {
            result.harness_session_ids.insert(name.clone(), id.clone());
        }
        result.all.insert(
            name.clone(),
            SessionDescriptor {
                name: name.clone(),
                id: session.id,
                created_at: session.created_at,
                attached_clients: session.attached_clients,
                window_count: session.window_count,
                harness_session_id: session.harness_session_id,
            },
        );
        if let Some((slug, managed_name)) = name.rsplit_once('~')
            && !slug.is_empty()
        {
            result.kinds.claude.push(ClaudeSlot {
                slug: slug.to_owned(),
                managed_name: Some(managed_name.to_owned()),
            });
            continue;
        }
        if let Some((slug, _)) = strip_kind_suffix(&name) {
            match name.strip_prefix(slug).unwrap_or_default() {
                "-codex" => {
                    result.kinds.codex.insert(slug.to_owned());
                }
                "-pi" => {
                    result.kinds.pi.insert(slug.to_owned());
                }
                "-opencode" => {
                    result.kinds.opencode.insert(slug.to_owned());
                }
                "-diff" => {
                    result.kinds.diff.insert(slug.to_owned());
                }
                "-shell" => {
                    result.kinds.shell.insert(slug.to_owned());
                }
                "-action" => {
                    result.kinds.action.insert(slug.to_owned());
                }
                "-dev" => {
                    result.kinds.dev.insert(slug.to_owned());
                }
                _ => {
                    result.kinds.claude.push(ClaudeSlot {
                        slug: name,
                        managed_name: None,
                    });
                }
            }
        } else {
            result.kinds.claude.push(ClaudeSlot {
                slug: name,
                managed_name: None,
            });
        }
    }
    result
        .kinds
        .claude
        .sort_by(|a, b| (&a.slug, &a.managed_name).cmp(&(&b.slug, &b.managed_name)));
    result
}

fn strip_kind_suffix(name: &str) -> Option<(&str, &'static str)> {
    [
        "-opencode",
        "-codex",
        "-pi",
        "-action",
        "-shell",
        "-diff",
        "-dev",
    ]
    .into_iter()
    .find_map(|suffix| {
        name.strip_suffix(suffix)
            .filter(|slug| !slug.is_empty())
            .map(|slug| (slug, suffix))
    })
}

struct DiscoveryInputs {
    git: LeafHandle<Vec<WorktreeSnapshot>>,
    inventory: LeafHandle<SessionInventory>,
}

fn confirm_discovery_inputs(
    git: &LeafHandle<Vec<WorktreeSnapshot>>,
    inventory: &LeafHandle<SessionInventory>,
    git_snapshot: &SourceSnapshot<Vec<WorktreeSnapshot>>,
    inventory_snapshot: &SourceSnapshot<SessionInventory>,
) -> Option<(
    Confirmed<Vec<WorktreeSnapshot>>,
    Confirmed<SessionInventory>,
)> {
    if matches!(git_snapshot.state(), SourceState::Failed(_))
        || matches!(inventory_snapshot.state(), SourceState::Failed(_))
    {
        return None;
    }
    Some((
        git.confirm(git_snapshot)?,
        inventory.confirm(inventory_snapshot)?,
    ))
}

fn no_discovery_publication(
    git_observed: Stamp,
    inventory_observed: Stamp,
    previous_discovery: Option<(Stamp, SourceState)>,
) -> (Stamp, SourceState) {
    let inputs = git_observed.earliest(inventory_observed);
    previous_discovery.map_or((inputs, SourceState::Ready), |(previous, state)| {
        (previous.earliest(inputs), state)
    })
}

fn discovery_result_state(errors: &[String]) -> SourceState {
    if errors.is_empty() {
        SourceState::Ready
    } else {
        SourceState::Failed(errors.join("; ").into())
    }
}

fn spawn_discoveries(
    scope: &TaskScope,
    app: AppHarness,
    service: HarnessService,
    cancel: CancellationToken,
    inputs: DiscoveryInputs,
    mut publisher: LeafPublisher<SessionDiscoveries>,
    mut requests: mpsc::Receiver<(String, HarnessId)>,
) {
    let DiscoveryInputs { git, inventory } = inputs;
    let scope_cancel = scope.token();
    scope.spawn(async move {
        let mut git_updates = git.subscribe();
        let mut inventory_updates = inventory.subscribe();
        let mut prior_inventory = None::<SessionInventory>;
        let mut prior_targets = None;
        let mut data = SessionDiscoveries::new();
        let mut has_published = false;
        let mut force_refresh = false;
        let mut pending = BTreeSet::<(String, HarnessId)>::new();
        // Start of the discovery pass that produced `data`. A liveness-only
        // republish keeps it: liveness is not a newer discovery.
        let mut discovered_at = Stamp::now();
        let mut discovery_state = SourceState::Ready;
        let mut backstop = tokio::time::interval(Duration::from_secs(30));
        backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = scope_cancel.cancelled() => break,
                _ = cancel.cancelled() => break,
                _ = backstop.tick() => {
                    force_refresh = true;
                }
                changed = git_updates.changed() => {
                    if changed.is_err() { break; }
                    git_updates.borrow_and_update();
                }
                changed = inventory_updates.changed() => {
                    if changed.is_err() { break; }
                    inventory_updates.borrow_and_update();
                }
                request = publisher.requested() => match request {
                    Some(()) => force_refresh = true,
                    None => break,
                },
                request = requests.recv() => match request {
                    Some(request) => { pending.insert(request); }
                    None => break,
                }
            }

            let inv_snapshot = inventory_updates.borrow().clone();
            let git_snapshot = git_updates.borrow().clone();
            let Some((worktrees, inv)) =
                confirm_discovery_inputs(&git, &inventory, &git_snapshot, &inv_snapshot)
            else {
                continue;
            };

            let targets = app.targets(&worktrees);
            if force_refresh {
                pending.extend(live_discovery_requests(&inv));
            }
            let known_slugs = targets
                .iter()
                .map(|target| target.slug.clone())
                .collect::<BTreeSet<_>>();
            let inv_changed = prior_inventory.as_ref() != Some(&*inv);
            let git_changed = prior_targets.as_ref() != Some(&targets);
            if inv_changed || git_changed {
                prior_inventory = Some((*inv).clone());
                prior_targets = Some(targets.clone());
                pending.extend(live_discovery_requests(&inv));
            }
            // A worktree literally named `one-codex` owns that tmux
            // name as its Claude primary. Do not give it to `one` merely
            // because the suffix resembles another harness's slot.
            for name in inv.all.keys().filter(|name| known_slugs.contains(*name)) {
                pending.insert((name.clone(), HarnessId::Claude));
                if let Some(slug) = name.strip_suffix("-codex") {
                    pending.remove(&(slug.to_owned(), HarnessId::Codex));
                }
                if let Some(slug) = name.strip_suffix("-pi") {
                    pending.remove(&(slug.to_owned(), HarnessId::Pi));
                }
                if let Some(slug) = name.strip_suffix("-opencode") {
                    pending.remove(&(slug.to_owned(), HarnessId::Opencode));
                }
            }
            let mut liveness_changed = false;
            for entry in &mut data {
                let is_live = is_session_live(&inv, entry.key.harness, &entry.session);
                liveness_changed |= entry.session.is_live != is_live;
                entry.session.is_live = is_live;
            }
            let by_slug = targets
                .into_iter()
                .map(|target| (target.slug.clone(), target))
                .collect::<BTreeMap<_, _>>();
            let sessions = inv.all.keys().cloned().collect::<Vec<_>>();
            let batch = std::mem::take(&mut pending);
            let mut work = batch
                .into_iter()
                .filter_map(|(slug, harness)| {
                    by_slug
                        .get(&slug)
                        .filter(|target| !target.remote)
                        .cloned()
                        .map(|target| (target, harness))
                })
                .collect::<std::collections::VecDeque<_>>();
            let has_work = !work.is_empty();
            if !has_work {
                if force_refresh || !has_published || liveness_changed {
                    let (observed, state) = no_discovery_publication(
                        worktrees.observed(),
                        inv.observed(),
                        has_published.then(|| (discovered_at, discovery_state.clone())),
                    );
                    publisher.publish_live_with_state(data.clone(), observed, state.clone());
                    discovered_at = observed;
                    discovery_state = state;
                    has_published = true;
                }
                force_refresh = false;
                continue;
            }
            force_refresh = false;
            let observed = Stamp::now();
            let mut join_set = tokio::task::JoinSet::new();
            let mut errors = Vec::new();
            for _ in 0..MAX_DISCOVERY_WORKERS {
                if let Some((target, harness)) = work.pop_front() {
                    spawn_discovery(
                        &mut join_set,
                        service.clone(),
                        target,
                        harness,
                        (*inv).clone(),
                        sessions.clone(),
                        scope_cancel.child_token(),
                    );
                }
            }
            while let Some(joined) = join_set.join_next().await {
                if scope_cancel.is_cancelled() || cancel.is_cancelled() {
                    break;
                }
                if let Ok((slug, harness, Ok(found))) = &joined {
                    data.retain(|entry| entry.key.slug != *slug || entry.key.harness != *harness);
                    for mut session in found.clone() {
                        session.is_live = is_session_live(&inv, *harness, &session);
                        let key = SessionKey {
                            slug: slug.clone(),
                            harness: *harness,
                            session_id: session.session_id.clone(),
                        };
                        data.push(DiscoveredSession { key, session });
                    }
                } else {
                    errors.push(match joined {
                        Ok((slug, harness, Err(error))) => format!("{slug} {harness:?}: {error}"),
                        Err(error) => format!("session discovery task: {error}"),
                        _ => unreachable!(),
                    });
                }
                if let Some((target, harness)) = work.pop_front() {
                    spawn_discovery(
                        &mut join_set,
                        service.clone(),
                        target,
                        harness,
                        (*inv).clone(),
                        sessions.clone(),
                        scope_cancel.child_token(),
                    );
                }
            }
            if !scope_cancel.is_cancelled() && !cancel.is_cancelled() {
                data.sort_by(|a, b| a.key.cmp(&b.key));
                discovered_at = observed;
                discovery_state = discovery_result_state(&errors);
                publisher.publish_live_with_state(data.clone(), observed, discovery_state.clone());
                has_published = true;
            }
        }
    });
}

/// Codex/OpenCode reuse one tmux slot across historical UUIDs. A live slot
/// without its exact identity is still busy, but proves no transcript live.
/// Claude's managed session name identifies its individual history entry.
pub(crate) fn is_session_live(
    inventory: &SessionInventory,
    harness: HarnessId,
    session: &HarnessSession,
) -> bool {
    inventory.all.contains_key(&session.tmux_session_name)
        && inventory
            .harness_session_ids
            .get(&session.tmux_session_name)
            .map_or(harness == HarnessId::Claude, |id| id == &session.session_id)
}

type DiscoveryTaskOutput = (String, HarnessId, Result<Vec<HarnessSession>, String>);

fn spawn_discovery(
    set: &mut tokio::task::JoinSet<DiscoveryTaskOutput>,
    service: HarnessService,
    target: crate::harness::AgentTarget,
    harness: HarnessId,
    inventory: SessionInventory,
    names: Vec<String>,
    cancel: CancellationToken,
) {
    set.spawn(async move {
        let name = tmux_name(&target, harness);
        let live_id = inventory.harness_session_ids.get(&name).cloned();
        let result = discover_target(&service, &target, harness, live_id, &names, &cancel).await;
        (target.slug, harness, result)
    });
}

fn live_discovery_requests(inventory: &SessionInventory) -> BTreeSet<(String, HarnessId)> {
    let mut requests = BTreeSet::new();
    for entry in &inventory.kinds.claude {
        requests.insert((entry.slug.clone(), HarnessId::Claude));
    }
    requests.extend(
        inventory
            .kinds
            .codex
            .iter()
            .cloned()
            .map(|slug| (slug, HarnessId::Codex)),
    );
    requests.extend(
        inventory
            .kinds
            .opencode
            .iter()
            .cloned()
            .map(|slug| (slug, HarnessId::Opencode)),
    );
    requests.extend(
        inventory
            .kinds
            .pi
            .iter()
            .cloned()
            .map(|slug| (slug, HarnessId::Pi)),
    );
    requests
}

async fn discover_target(
    service: &HarnessService,
    target: &crate::harness::AgentTarget,
    harness: HarnessId,
    live_session_id: Option<String>,
    live_tmux_names: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<HarnessSession>, String> {
    service
        .discover(
            harness,
            &DiscoveryRequest {
                slug: target.slug.clone(),
                worktree_path: target.cwd.clone(),
                live_session_id,
            },
            live_tmux_names,
            cancel,
        )
        .await
        .map_err(|error| error.to_string())
}

fn tmux_name(target: &crate::harness::AgentTarget, harness: HarnessId) -> String {
    match harness {
        HarnessId::Claude => target.managed_name.as_ref().map_or_else(
            || target.slug.clone(),
            |name| format!("{}~{name}", target.slug),
        ),
        HarnessId::Codex => format!("{}-codex", target.slug),
        HarnessId::Opencode => format!("{}-opencode", target.slug),
        HarnessId::Pi => format!("{}-pi", target.slug),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str, uuid: Option<&str>) -> SessionInfo {
        SessionInfo {
            name: name.into(),
            id: "$1".into(),
            created_at: 1,
            attached_clients: 0,
            window_count: 1,
            harness_session_id: uuid.map(str::to_owned),
        }
    }

    #[test]
    fn discoveries_wait_for_confirmed_inputs_then_accept_identical_live_data() {
        let scope = TaskScope::new();
        let (git, _) = leaf_channel(
            &scope,
            SourceSpec {
                name: "test-session-git",
                domain: Domain::Git,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        let (inventory, _) = leaf_channel(
            &scope,
            SourceSpec {
                name: "test-session-inventory",
                domain: Domain::Sessions,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        let worktrees = Vec::<WorktreeSnapshot>::new();
        let sessions = SessionInventory::default();
        let restored_git = SourceSnapshot::test_restored(worktrees.clone(), Domain::Git, 1);
        let restored_inventory =
            SourceSnapshot::test_restored(sessions.clone(), Domain::Sessions, 1);

        assert!(
            confirm_discovery_inputs(&git, &inventory, &restored_git, &restored_inventory,)
                .is_none()
        );

        // Identical data becomes usable only after both leaves publish live.
        // This transition must start discovery even though the target values
        // themselves did not change while the cache was displayed.
        let live_git = SourceSnapshot::test_live(Some(worktrees), SourceState::Ready, Domain::Git);
        let live_inventory =
            SourceSnapshot::test_live(Some(sessions), SourceState::Ready, Domain::Sessions);
        assert!(confirm_discovery_inputs(&git, &inventory, &live_git, &live_inventory).is_some());
    }

    #[test]
    fn no_io_republish_does_not_advance_the_discovery_witness() {
        let previous = Stamp::now();
        let failed = discovery_result_state(&["discovery transport failed".to_owned()]);
        std::thread::sleep(Duration::from_millis(3));
        let (observed, state) =
            no_discovery_publication(Stamp::now(), Stamp::now(), Some((previous, failed.clone())));
        assert_eq!(observed, previous);
        assert_eq!(state, failed);

        let successful = discovery_result_state(&[]);
        let (observed, state) = no_discovery_publication(
            Stamp::now(),
            Stamp::now(),
            Some((previous, successful.clone())),
        );
        assert_eq!(observed, previous);
        assert_eq!(state, successful);
    }

    #[tokio::test]
    async fn scheduler_waits_for_identical_live_git_before_confirming_empty_discoveries() {
        let mut fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache_scope = "session-freshness-test";
        let cache = wt_runtime::CacheStore::new(cache_dir.path().to_path_buf(), cache_scope);
        fixture.ctx.caches = cache.clone();
        let saved_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        std::fs::write(
            cache_dir.path().join("test-session-git.json"),
            serde_json::to_vec(&serde_json::json!({
                "format": 1,
                "name": "test-session-git",
                "scope": cache_scope,
                "saved_at_ms": saved_at_ms,
                "data": [],
            }))
            .unwrap(),
        )
        .unwrap();

        let scope = TaskScope::new();
        let (git, git_publisher) = leaf_channel(
            &scope,
            SourceSpec {
                name: "test-session-git",
                domain: Domain::Git,
                persistence: Persistence::Disk(DiskCache::new(&cache)),
                policy: RefreshPolicy::default(),
            },
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            while !git.snapshot().is_restored() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fixture Git cache should restore");

        let (inventory, inventory_publisher) = leaf_channel(
            &scope,
            SourceSpec {
                name: "test-session-inventory",
                domain: Domain::Sessions,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        let (discoveries, discoveries_publisher) = leaf_channel(
            &scope,
            SourceSpec {
                name: "test-session-discoveries",
                domain: Domain::Sessions,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        let service = HarnessService::new(
            ClaudePaths::new(&fixture.ctx.home, &fixture.ctx.config.paths.cache_root),
            crate::harness::codex_paths(&fixture.ctx.home, &fixture.ctx.config.paths.cache_root),
            OpenCodePaths::new(&fixture.ctx.home, &fixture.ctx.config.paths.cache_root),
            wt_harness::PiPaths::new(&fixture.ctx.home, &fixture.ctx.config.paths.cache_root),
            fixture.ctx.processes.clone(),
            TmuxClient::new(
                fixture.ctx.processes.clone(),
                TmuxServer::named(fixture.ctx.config.tmux.socket.clone())
                    .with_cwd(fixture.ctx.home.clone()),
            ),
        );
        let (request_tx, request_rx) = mpsc::channel(4);
        spawn_discoveries(
            &scope,
            AppHarness::new(&fixture.ctx),
            service,
            fixture.ctx.cancellation.clone(),
            DiscoveryInputs {
                git: git.clone(),
                inventory: inventory.clone(),
            },
            discoveries_publisher,
            request_rx,
        );

        inventory_publisher.publish_live(SessionInventory::default(), Stamp::now());
        request_tx
            .send(("no-such-target".into(), HarnessId::Codex))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(git.snapshot().is_restored());
        assert!(discoveries.confirmed().is_none());

        // The live Git fetch returns byte-for-byte equivalent empty data. The
        // scheduler must still process the retained request and publish a
        // live empty result because the restored input never advanced its
        // remembered target projection.
        git_publisher.publish_live(Vec::new(), Stamp::now());
        tokio::time::timeout(Duration::from_secs(3), async {
            while discoveries.confirmed().is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("confirmed identical Git input should resume discovery");
        assert!(discoveries.confirmed().unwrap().is_empty());

        let previous_revision = discoveries.snapshot().revision();
        assert!(discoveries.refresh());
        tokio::time::timeout(Duration::from_secs(3), async {
            while discoveries.snapshot().revision() <= previous_revision {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("manual refresh should reach the discovery source");

        scope.shutdown(Duration::from_secs(1)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[test]
    fn codex_terminal_status_is_presentation_only_and_requires_explicit_markers() {
        for spinner in ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"] {
            assert_eq!(
                codex_terminal_activity(&format!("{spinner} task | project")),
                Some("working")
            );
        }
        for title in ["[ ! ] Action Required | task", "[ . ] Action Required"] {
            assert_eq!(codex_terminal_activity(title), Some("asking"));
        }
        for title in [
            "",
            "Ready",
            "Working on a task",
            "task | project",
            "_ task",
            "[ ! ] Action Requiredness",
            "⠋",
        ] {
            assert_eq!(codex_terminal_activity(title), None);
        }
    }

    #[test]
    fn retained_failed_panes_are_not_running_agents() {
        let pane = wt_tmux::PaneInfo {
            title: String::new(),
            id: "%1".into(),
            session_name: "branch-codex".into(),
            window_id: "@1".into(),
            window_index: 0,
            index: 0,
            active: true,
            pid: Some(42),
            current_path: "/tmp/branch".into(),
            width: 80,
            height: 24,
            dead: true,
        };
        let failed = running_session_inventory(
            vec![session("branch-codex", Some("old"))],
            std::slice::from_ref(&pane),
        );
        assert!(failed.all.is_empty());
        assert!(failed.kinds.codex.is_empty());
        assert!(failed.harness_session_ids.is_empty());
        let retried = running_session_inventory(
            vec![session("branch-codex", Some("new"))],
            &[wt_tmux::PaneInfo {
                dead: false,
                ..pane
            }],
        );
        assert!(retried.kinds.codex.contains("branch"));
        assert_eq!(retried.harness_session_ids["branch-codex"], "new");
    }

    #[test]
    fn stable_harness_slot_without_uuid_does_not_make_its_entire_history_live() {
        for harness in [HarnessId::Codex, HarnessId::Opencode, HarnessId::Pi] {
            let name = format!("branch-{}", harness.as_str());
            let mut inventory = classify_sessions(vec![session(&name, None)]);
            let history = ["old", "current"].map(|id| HarnessSession {
                display_name: id.into(),
                session_id: id.into(),
                tmux_session_name: name.clone(),
                last_active_ms: None,
                is_live: false,
                extras: Default::default(),
            });
            assert!(
                history
                    .iter()
                    .all(|entry| !is_session_live(&inventory, harness, entry))
            );
            assert!(
                inventory.all.contains_key(&name),
                "unknown slot must remain busy for safety"
            );
            inventory.harness_session_ids.insert(name, "current".into());
            assert!(!is_session_live(&inventory, harness, &history[0]));
            assert!(is_session_live(&inventory, harness, &history[1]));
        }
    }

    #[test]
    fn partitions_names_and_retains_exact_resumed_uuid() {
        let inventory = classify_sessions(vec![
            session("main", None),
            session("main-manager", None),
            session("branch-codex", Some("old-rollout-id")),
            session("branch-opencode", None),
            session("branch-pi", Some("pi-session-id")),
            session("branch~review", None),
            session("branch-shell", None),
            session("branch-action", None),
            session("branch-dev", None),
        ]);
        assert_eq!(
            inventory.kinds.claude,
            vec![
                ClaudeSlot {
                    slug: "branch".into(),
                    managed_name: Some("review".into())
                },
                ClaudeSlot {
                    slug: "main".into(),
                    managed_name: None
                },
                ClaudeSlot {
                    slug: "main-manager".into(),
                    managed_name: None
                }
            ]
        );
        assert!(inventory.kinds.codex.contains("branch"));
        assert!(inventory.kinds.opencode.contains("branch"));
        assert!(inventory.kinds.pi.contains("branch"));
        assert!(live_discovery_requests(&inventory).contains(&("branch".into(), HarnessId::Pi)));
        assert!(inventory.kinds.shell.contains("branch"));
        assert!(inventory.kinds.action.contains("branch"));
        assert!(inventory.kinds.dev.contains("branch"));
        assert_eq!(
            inventory
                .harness_session_ids
                .get("branch-codex")
                .map(String::as_str),
            Some("old-rollout-id")
        );
        let json = serde_json::to_string(&inventory).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionInventory>(&json).unwrap(),
            inventory
        );
    }

    #[test]
    fn named_claude_is_split_at_last_separator_and_suffix_priority_is_longest_first() {
        let inventory = classify_sessions(vec![
            session("feature-with-dashes~review-v2", None),
            session("slug-opencode", None),
        ]);
        assert_eq!(inventory.kinds.claude[0].slug, "feature-with-dashes");
        assert_eq!(
            inventory.kinds.claude[0].managed_name.as_deref(),
            Some("review-v2")
        );
        assert!(inventory.kinds.opencode.contains("slug"));
    }

    #[tokio::test(start_paused = true)]
    async fn session_discoveries_meet_the_persisted_source_contract() {
        let sample: SessionDiscoveries = vec![DiscoveredSession {
            key: SessionKey {
                slug: "feature-one".into(),
                harness: HarnessId::Claude,
                session_id: "0b5c6a8e-3f0d-4c1e-9d55-1f2a7c9e4b10".into(),
            },
            session: HarnessSession {
                display_name: "Fix the restack prompt".into(),
                session_id: "0b5c6a8e-3f0d-4c1e-9d55-1f2a7c9e4b10".into(),
                tmux_session_name: "feature-one".into(),
                last_active_ms: Some(1_760_000_000_000),
                is_live: true,
                extras: wt_harness::HarnessExtras {
                    derived_state: Some(wt_harness::DerivedState::Idle),
                    session_summary: Some("Restack prompt fixed".into()),
                    context_percent: Some(42),
                    ..Default::default()
                },
            },
        }];
        wt_runtime::conformance::persisted_source(
            "session-discoveries",
            wt_core::Domain::Sessions,
            sample,
        )
        .await;
    }
}
