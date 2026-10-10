//! Host-local tmux inventory and harness session discovery.
//!
//! This source deliberately consumes the already-batched local Git source. It
//! never scans Git per session and makes a single tmux inventory request per
//! refresh. Remote inventory is composed by the host boundary above this module.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wt_core::HarnessId;
use wt_harness::{
    ClaudePaths, CodexPaths, DiscoveryRequest, HarnessService, HarnessSession, OpenCodePaths,
};
use wt_runtime::{
    SourceHandle, SourcePublisher, SourceSnapshot, SourceState, TaskScope, source_channel,
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
    pub inventory: SourceHandle<SessionInventory>,
    pub discoveries: SourceHandle<SessionDiscoveries>,
    pub commands: SessionCommands,
    /// Shared host-local Git facts, passed through without rescanning.
    pub git: SourceHandle<Vec<WorktreeSnapshot>>,
}

pub fn start(
    scope: &TaskScope,
    context: &AppContext,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
) -> SessionSources {
    let app = AppHarness::new(context);
    let tmux = TmuxClient::new(
        context.processes.clone(),
        TmuxServer::named(context.config.tmux.socket.clone()).with_cwd(context.home.clone()),
    );
    let (inventory, inventory_publisher) = source_channel();
    spawn_inventory(scope, tmux, inventory_publisher);

    let (discoveries, publisher) = source_channel();
    let (requests, request_rx) = mpsc::channel(32);
    let service = HarnessService::new(
        ClaudePaths::new(&context.home, &context.config.paths.cache_root),
        CodexPaths::new(&context.home, &context.config.paths.cache_root),
        OpenCodePaths::new(&context.home, &context.config.paths.cache_root),
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
        git,
    }
}

fn spawn_inventory(
    scope: &TaskScope,
    tmux: TmuxClient,
    mut publisher: SourcePublisher<SessionInventory>,
) {
    let cancel = scope.token();
    scope.spawn(async move {
        let mut previous: Option<SessionInventory> = None;
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
            let fetched = tmux.list_sessions(&cancel).await;
            if cancel.is_cancelled() {
                break;
            }
            match fetched {
                Ok(sessions) => {
                    let inventory = classify_sessions(sessions);
                    if previous.as_ref() != Some(&inventory) || !healthy {
                        previous = Some(inventory.clone());
                        publisher.publish(SourceSnapshot {
                            data: Some(Arc::new(inventory)),
                            state: SourceState::Ready,
                            updated_at: Some(tokio::time::Instant::now()),
                            revision: 0,
                        });
                    }
                    healthy = true;
                }
                Err(error) => {
                    healthy = false;
                    // Preserve last-known sessions while making uncertainty
                    // visible to consumers that make lifecycle decisions.
                    publisher.publish(SourceSnapshot {
                        data: previous.clone().map(Arc::new),
                        state: SourceState::Failed(
                            format!("tmux session inventory: {error}").into(),
                        ),
                        updated_at: None,
                        revision: 0,
                    });
                }
            }
        }
    });
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
    ["-opencode", "-codex", "-action", "-shell", "-diff", "-dev"]
        .into_iter()
        .find_map(|suffix| {
            name.strip_suffix(suffix)
                .filter(|slug| !slug.is_empty())
                .map(|slug| (slug, suffix))
        })
}

struct DiscoveryInputs {
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    inventory: SourceHandle<SessionInventory>,
}

fn spawn_discoveries(
    scope: &TaskScope,
    app: AppHarness,
    service: HarnessService,
    cancel: CancellationToken,
    inputs: DiscoveryInputs,
    publisher: SourcePublisher<SessionDiscoveries>,
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
        let mut pending = BTreeSet::<(String, HarnessId)>::new();
        let mut backstop = tokio::time::interval(Duration::from_secs(30));
        backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = scope_cancel.cancelled() => break,
                _ = cancel.cancelled() => break,
                _ = backstop.tick() => {
                    if let Some(inventory) = inventory_updates.borrow().data.as_ref() {
                        pending.extend(live_discovery_requests(inventory));
                    }
                }
                changed = git_updates.changed() => {
                    if changed.is_err() { break; }
                    git_updates.borrow_and_update();
                }
                changed = inventory_updates.changed() => {
                    if changed.is_err() { break; }
                    inventory_updates.borrow_and_update();
                }
                request = requests.recv() => match request {
                    Some(request) => { pending.insert(request); }
                    None => break,
                }
            }

            let inv_snapshot = inventory_updates.borrow().clone();
            let Some(inv) = inv_snapshot.data.as_deref() else {
                continue;
            };
            if matches!(inv_snapshot.state, SourceState::Failed(_)) {
                continue;
            }
            let git_snapshot = git_updates.borrow().clone();
            let Some(worktrees) = git_snapshot.data.as_deref() else {
                continue;
            };
            if matches!(git_snapshot.state, SourceState::Failed(_)) {
                continue;
            }

            let targets = app.targets(worktrees);
            let known_slugs = targets
                .iter()
                .map(|target| target.slug.clone())
                .collect::<BTreeSet<_>>();
            let inv_changed = prior_inventory.as_ref() != Some(inv);
            let git_changed = prior_targets.as_ref() != Some(&targets);
            if inv_changed || git_changed {
                prior_inventory = Some(inv.clone());
                prior_targets = Some(targets.clone());
                pending.extend(live_discovery_requests(inv));
            }
            // A worktree literally named `one-codex` owns that tmux
            // name as its Claude primary. Do not give it to `one` merely
            // because the suffix resembles another harness's slot.
            for name in inv.all.keys().filter(|name| known_slugs.contains(*name)) {
                pending.insert((name.clone(), HarnessId::Claude));
                if let Some(slug) = name.strip_suffix("-codex") {
                    pending.remove(&(slug.to_owned(), HarnessId::Codex));
                }
                if let Some(slug) = name.strip_suffix("-opencode") {
                    pending.remove(&(slug.to_owned(), HarnessId::Opencode));
                }
            }
            let mut liveness_changed = false;
            for entry in &mut data {
                let is_live = is_session_live(inv, &entry.session);
                liveness_changed |= entry.session.is_live != is_live;
                entry.session.is_live = is_live;
            }
            if pending.is_empty() {
                if liveness_changed {
                    publisher.publish(SourceSnapshot {
                        data: Some(Arc::new(data.clone())),
                        state: SourceState::Ready,
                        updated_at: Some(tokio::time::Instant::now()),
                        revision: 0,
                    });
                }
                continue;
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
            let mut join_set = tokio::task::JoinSet::new();
            let mut errors = Vec::new();
            for _ in 0..MAX_DISCOVERY_WORKERS {
                if let Some((target, harness)) = work.pop_front() {
                    spawn_discovery(
                        &mut join_set,
                        service.clone(),
                        target,
                        harness,
                        inv.clone(),
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
                        session.is_live = is_session_live(inv, &session);
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
                        inv.clone(),
                        sessions.clone(),
                        scope_cancel.child_token(),
                    );
                }
            }
            if !scope_cancel.is_cancelled() && !cancel.is_cancelled() {
                data.sort_by(|a, b| a.key.cmp(&b.key));
                publisher.publish(SourceSnapshot {
                    data: Some(Arc::new(data.clone())),
                    state: if errors.is_empty() {
                        SourceState::Ready
                    } else {
                        SourceState::Failed(errors.join("; ").into())
                    },
                    updated_at: Some(tokio::time::Instant::now()),
                    revision: 0,
                });
            }
        }
    });
}

fn is_session_live(inventory: &SessionInventory, session: &HarnessSession) -> bool {
    inventory.all.contains_key(&session.tmux_session_name)
        && inventory
            .harness_session_ids
            .get(&session.tmux_session_name)
            .is_none_or(|id| id == &session.session_id)
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
    fn partitions_names_and_retains_exact_resumed_uuid() {
        let inventory = classify_sessions(vec![
            session("main", None),
            session("main-manager", None),
            session("branch-codex", Some("old-rollout-id")),
            session("branch-opencode", None),
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
}
