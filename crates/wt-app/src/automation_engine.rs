//! Host-local automation scheduling over one caller-prepared source snapshot.
//!
//! This module owns only queue, ledger and dispatch ordering. It does not read
//! Git, GitHub, sessions, metadata or presentation rows; the caller supplies
//! typed facts from a coherent pass and executes the claimed fires.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;
use wt_automations::{
    AutomationEvalContext, AutomationFire, AutomationIntent, AutomationLedger, AutomationRow,
    CancellableFiresInput, DispatchClaim, DispatchClaimResult, DispatchKind, DispatchPolicy,
    DispatchRequest, QueueIntentsInput, cancellable_fires, evaluate, evaluate_breaker_resets,
    fire_identity, queue_intents,
};
use wt_config::{AutomationBusyPolicy, AutomationDef};

const MAX_CONCURRENT: usize = 2;
const SESSION_SLOT_MAX_MS: i64 = 10 * 60 * 1000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorktreeQuiescence {
    pub locked: bool,
    pub action_running: bool,
    pub session_busy: bool,
    /// Wall-clock time of the latest edit in the worktree, if known.
    pub last_edit_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeActivity {
    /// Slugs with a durable headless action run still in the running state.
    pub headless_running: BTreeSet<String>,
    /// Slugs whose live session is working, asking, or otherwise unsafe to
    /// inject into. Include manager only if the caller wants manager-targeted
    /// session work to remain serialized after enqueue.
    pub busy_sessions: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequirementState {
    Ready,
    /// A row-backed prerequisite may become true later. Do not occupy queue.
    Waiting,
    /// Frozen input cannot change; consume this fire as a terminal skip.
    FrozenUnmet(String),
}

#[derive(Clone, Debug)]
pub struct AutomationPassInput<'a> {
    pub rules: &'a [AutomationDef],
    pub rows: &'a [AutomationRow],
    pub context: &'a AutomationEvalContext,
    pub globally_paused: bool,
    pub state_ready: bool,
    pub enabled: bool,
    pub now_ms: i64,
    /// Per-fire requirement results, keyed by `fire_identity`. Missing
    /// non-builtin rules fail closed and are held until the adapter supplies
    /// a decision; builtins need no action requirements.
    pub requirements: &'a BTreeMap<String, RequirementState>,
    pub quiescence: &'a BTreeMap<String, WorktreeQuiescence>,
    pub activity: &'a RuntimeActivity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliverySlot {
    /// The command completed synchronously; release its queue slot.
    None,
    /// A launched headless action occupies its slug while it remains running.
    Headless,
    /// A prompt was delivered into a live session; hold while it is busy,
    /// with the same ten-minute cap as the TypeScript engine.
    Session,
    /// A manager prompt occupies the singleton only until injection returns.
    Manager,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutomationOutcome {
    /// Positive evidence that the dispatch path performed no durable write.
    NotStarted,
    /// The action launched or completed. A reported action failure is still
    /// delivered and must not be automatically replayed.
    Delivered { slot: DeliverySlot },
    /// A prompt may have been delivered but its acknowledgement was lost.
    Ambiguous { reason: String, slot: DeliverySlot },
}

#[derive(Clone, Debug)]
pub struct ClaimedFire {
    id: String,
    pub fire: AutomationFire,
    claim: DispatchClaim,
}

impl ClaimedFire {
    pub fn fire(&self) -> &AutomationFire {
        &self.fire
    }
}

#[derive(Clone, Debug, Default)]
pub struct AutomationTick {
    pub pending: usize,
    pub claims: Vec<ClaimedFire>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SlotState {
    Dispatching,
    Headless { slug: String },
    Session { slug: String, dispatched_at_ms: i64 },
}

#[derive(Clone, Debug)]
struct ActiveFire {
    claim: DispatchClaim,
    fire: AutomationFire,
    slot: SlotState,
}

/// Queue and ledger coordinator. The caller drives `tick` on source changes
/// and a bounded settle heartbeat, then executes each returned claim exactly
/// once and reports its final delivery evidence with `finish`.
pub struct AutomationEngine {
    ledger: AutomationLedger,
    cancellation: CancellationToken,
    intents: BTreeMap<String, AutomationIntent>,
    active: BTreeMap<String, ActiveFire>,
}

impl AutomationEngine {
    pub fn new(ledger: AutomationLedger, cancellation: CancellationToken) -> Self {
        Self {
            ledger,
            cancellation,
            intents: BTreeMap::new(),
            active: BTreeMap::new(),
        }
    }

    pub fn pending_count(&self) -> usize {
        self.intents.len()
    }

    /// Mark a persisted `dispatched` entry delivered only when a durable
    /// action-run record proves the complete fire-key batch. Every unmatched
    /// attempt remains ambiguous and handled, never replayed.
    pub async fn reconcile_boot(
        &self,
        delivered_keys: &BTreeSet<String>,
        now_ms: i64,
    ) -> Result<(usize, usize)> {
        self.ledger
            .reconcile_dispatched(delivered_keys, now_ms, &self.cancellation)
            .await
            .context("reconcile interrupted automation dispatches")
    }

    pub async fn tick(&mut self, input: AutomationPassInput<'_>) -> Result<AutomationTick> {
        self.release_finished_slots(input.now_ms, input.activity);
        if !input.state_ready {
            // A transient store read failure must pause dispatch, not silently
            // discard the in-memory queue or pretend its cancellation persisted.
            return Ok(AutomationTick::default());
        }
        if input.globally_paused || !input.enabled {
            self.cancel_pending(input).await?;
            return Ok(AutomationTick::default());
        }

        for (rule_id, target) in evaluate_breaker_resets(input.rules, input.rows, input.context) {
            self.ledger
                .reset_breaker(&rule_id, &target, &self.cancellation)
                .await
                .with_context(|| format!("reset automation breaker for {rule_id}/{target}"))?;
        }

        let current = evaluate(input.rules, input.rows, input.context);
        let existing = self.intents.values().cloned().collect::<Vec<_>>();
        let candidates = current
            .iter()
            .flat_map(|fire| fire.fire_keys.iter())
            .chain(
                existing
                    .iter()
                    .flat_map(|intent| intent.fire.fire_keys.iter()),
            )
            .cloned()
            .collect::<BTreeSet<_>>();
        let ledger = self
            .ledger
            .snapshot()
            .await
            .context("read automation ledger")?;
        let handled = candidates
            .into_iter()
            .filter(|key| ledger.has_handled(key, input.now_ms))
            .collect::<BTreeSet<_>>();
        let executing = self.active.keys().cloned().collect::<BTreeSet<_>>();
        let intents = queue_intents(QueueIntentsInput {
            existing: &existing,
            current: &current,
            rows: input.rows,
            context: input.context,
            now_ms: input.now_ms,
            paused: false,
            state_ready: true,
            handled: &handled,
            executing: &executing,
        });
        self.intents = intents
            .into_iter()
            .map(|intent| (fire_identity(&intent.fire), intent))
            .collect();

        let mut queue = self.intents.values().cloned().collect::<Vec<_>>();
        queue.sort_by_key(|intent| (intent.queued_at_ms, fire_identity(&intent.fire)));
        let mut occupied_slugs = BTreeSet::new();
        let mut restack_stacks = BTreeSet::new();
        let mut manager_in_flight = false;
        for active in self.active.values() {
            occupied_slugs.extend(active.fire.quiesce_slugs.iter().cloned());
            if active.fire.rule.run == "builtin:restack"
                && let Some(stack_id) = &active.fire.stack_id
            {
                restack_stacks.insert(stack_id.clone());
            }
            if matches!(active.slot, SlotState::Dispatching)
                && input
                    .context
                    .actions
                    .get(&active.fire.rule.run)
                    .is_some_and(|traits| {
                        traits.audience == wt_automations::ActionAudience::Manager
                    })
            {
                manager_in_flight = true;
            }
        }

        let mut report = AutomationTick::default();
        for intent in queue {
            if self.active.len() >= MAX_CONCURRENT {
                break;
            }
            let fire = intent.fire;
            let id = fire_identity(&fire);
            let rule = &fire.rule;
            let manager_run =
                input.context.actions.get(&rule.run).is_some_and(|traits| {
                    traits.audience == wt_automations::ActionAudience::Manager
                });
            let audience = input
                .context
                .actions
                .get(&rule.run)
                .map_or(wt_automations::ActionAudience::None, |traits| {
                    traits.audience
                });
            let breaker_exempt = breaker_exempt(&fire, manager_run);

            if fire
                .quiesce_slugs
                .iter()
                .any(|slug| occupied_slugs.contains(slug))
            {
                continue;
            }
            if rule.run == "builtin:restack"
                && fire
                    .stack_id
                    .as_ref()
                    .is_some_and(|id| restack_stacks.contains(id))
            {
                continue;
            }
            if manager_run && manager_in_flight {
                continue;
            }

            let requirements = input.requirements.get(&id);
            if matches!(requirements, Some(RequirementState::Waiting)) {
                // Preserve the original settle age while a row-backed
                // prerequisite changes. The trigger still has to remain true.
                continue;
            }
            if matches!(requirements, Some(RequirementState::FrozenUnmet(_))) {
                let keys = fire.fire_keys.clone();
                self.ledger
                    .mark_skipped(
                        &keys,
                        &rule.id,
                        &pair_target(&fire),
                        input.now_ms,
                        &self.cancellation,
                    )
                    .await
                    .context("persist terminal unmet automation requirement")?;
                self.intents.remove(&id);
                continue;
            }
            if requirements.is_none() && !rule.run.starts_with("builtin:") {
                // Unknown action data is never treated as satisfied. A source
                // adapter must explicitly publish a requirement decision.
                continue;
            }

            let pair = pair_target(&fire);
            let current_state = self
                .ledger
                .snapshot()
                .await
                .context("read automation ledger")?;
            let breaker = current_state.breaker_state(&rule.id, &pair, input.now_ms);
            if !breaker_exempt && breaker.tripped_at.is_some() {
                self.ledger
                    .mark_skipped(
                        &fire.fire_keys,
                        &rule.id,
                        &pair,
                        input.now_ms,
                        &self.cancellation,
                    )
                    .await
                    .context("persist breaker-suppressed automation")?;
                self.intents.remove(&id);
                continue;
            }

            let cooldown_ms = rule.cooldown_minutes.map(|minutes| {
                if !minutes.is_finite() || minutes <= 0.0 {
                    0
                } else {
                    (minutes * 60_000.0).min(i64::MAX as f64) as i64
                }
            });
            if cooldown_ms.is_some_and(|cooldown| {
                current_state
                    .last_dispatch(&rule.id, &pair)
                    .is_some_and(|last| input.now_ms < last.saturating_add(cooldown))
            }) {
                continue;
            }
            if input.now_ms < intent.settle_at_ms {
                continue;
            }

            let quiesce_exempt = breaker_exempt;
            if !quiesce_exempt && let Some(reason) = quiesce_block_reason(&fire, &input) {
                if rule.busy == AutomationBusyPolicy::Skip {
                    self.ledger
                        .mark_skipped(
                            &fire.fire_keys,
                            &rule.id,
                            &pair,
                            input.now_ms,
                            &self.cancellation,
                        )
                        .await
                        .context("persist busy-skipped automation")?;
                    self.intents.remove(&id);
                    tracing::info!(rule = %rule.id, %reason, "automation skipped while target busy");
                }
                continue;
            }

            let policy = DispatchPolicy {
                cooldown_ms,
                breaker_limit: (!breaker_exempt).then_some(wt_automations::BREAKER_LIMIT),
            };
            match self
                .ledger
                .claim_dispatch_checked(
                    DispatchRequest {
                        keys: &fire.fire_keys,
                        rule_id: &rule.id,
                        slug: &pair,
                        kind: dispatch_kind(&fire, audience),
                        policy,
                        now_ms: input.now_ms,
                    },
                    &self.cancellation,
                )
                .await
                .context("claim automation dispatch")?
            {
                DispatchClaimResult::AlreadyHandled => {
                    self.intents.remove(&id);
                }
                DispatchClaimResult::CoolingDown { .. }
                | DispatchClaimResult::BreakerReserved { .. } => {}
                DispatchClaimResult::BreakerOpen { .. } => {
                    self.ledger
                        .mark_skipped(
                            &fire.fire_keys,
                            &rule.id,
                            &pair,
                            input.now_ms,
                            &self.cancellation,
                        )
                        .await
                        .context("persist breaker-suppressed automation")?;
                    self.intents.remove(&id);
                }
                DispatchClaimResult::Claimed(claim) => {
                    self.intents.remove(&id);
                    occupied_slugs.extend(fire.quiesce_slugs.iter().cloned());
                    if rule.run == "builtin:restack"
                        && let Some(stack_id) = &fire.stack_id
                    {
                        restack_stacks.insert(stack_id.clone());
                    }
                    if manager_run {
                        manager_in_flight = true;
                    }
                    self.active.insert(
                        id.clone(),
                        ActiveFire {
                            claim: claim.clone(),
                            fire: fire.clone(),
                            slot: SlotState::Dispatching,
                        },
                    );
                    report.claims.push(ClaimedFire { id, fire, claim });
                }
            }
        }
        report.pending = self.intents.len();
        Ok(report)
    }

    /// Persist cancellation before dropping pending intents. Active claims
    /// are excluded and are never recalled from their delivery path.
    pub async fn cancel_pending(&mut self, input: AutomationPassInput<'_>) -> Result<usize> {
        if !input.state_ready {
            return Ok(0);
        }
        let pending = self
            .intents
            .values()
            .map(|intent| intent.fire.clone())
            .collect::<Vec<_>>();
        let executing = self.active.keys().cloned().collect::<BTreeSet<_>>();
        let snapshot = self
            .ledger
            .snapshot()
            .await
            .context("read automation ledger")?;
        let keys = pending
            .iter()
            .flat_map(|fire| fire.fire_keys.iter())
            .cloned()
            .collect::<BTreeSet<_>>();
        let handled = keys
            .into_iter()
            .filter(|key| snapshot.has_handled(key, input.now_ms))
            .collect::<BTreeSet<_>>();
        let fires = cancellable_fires(CancellableFiresInput {
            paused: input.globally_paused || !input.enabled,
            state_ready: input.state_ready,
            rules: input.rules,
            rows: input.rows,
            context: input.context,
            pending: &pending,
            executing: &executing,
            handled: &handled,
        });
        let cancelled = fires
            .iter()
            .flat_map(|fire| fire.fire_keys.iter().cloned())
            .collect::<BTreeSet<_>>();
        if !cancelled.is_empty() {
            self.ledger
                .cancel(
                    &cancelled.into_iter().collect::<Vec<_>>(),
                    input.now_ms,
                    &self.cancellation,
                )
                .await
                .context("persist automation queue cancellation")?;
        }
        let count = fires
            .iter()
            .map(fire_identity)
            .collect::<BTreeSet<_>>()
            .len();
        // Every remaining pending key is either now durably cancelled or was
        // already handled by another writer. Clear only after persistence
        // succeeds so a ledger error leaves the queue retryable.
        self.intents.clear();
        Ok(count)
    }

    /// Complete one claimed delivery. The same claim id scopes every ledger
    /// transition, so a stale completion cannot overwrite a later key state.
    pub async fn finish(
        &mut self,
        claim: &ClaimedFire,
        outcome: AutomationOutcome,
        now_ms: i64,
    ) -> Result<()> {
        let Some(active) = self.active.get(&claim.id) else {
            anyhow::bail!("automation dispatch {} is not active", claim.id);
        };
        if active.slot != SlotState::Dispatching {
            anyhow::bail!("automation dispatch {} has already finished", claim.id);
        }
        if active.claim != claim.claim || active.fire.fire_keys != claim.fire.fire_keys {
            anyhow::bail!("automation dispatch claim identity does not match active fire");
        }
        let ledger_claim = active.claim.clone();
        match outcome {
            AutomationOutcome::NotStarted => {
                self.ledger
                    .release_not_started(&ledger_claim, &self.cancellation)
                    .await
                    .context("release automation claim proven not started")?;
                self.active.remove(&claim.id);
            }
            AutomationOutcome::Delivered { slot } => {
                self.ledger
                    .mark_delivered(&ledger_claim, now_ms, &self.cancellation)
                    .await
                    .context("mark automation delivered")?;
                self.finish_slot(&claim.id, slot, now_ms);
            }
            AutomationOutcome::Ambiguous { reason, slot } => {
                self.ledger
                    .mark_ambiguous(&ledger_claim, now_ms, &reason, &self.cancellation)
                    .await
                    .context("mark automation delivery ambiguous")?;
                self.finish_slot(&claim.id, slot, now_ms);
            }
        }
        Ok(())
    }

    fn finish_slot(&mut self, id: &str, slot: DeliverySlot, now_ms: i64) {
        let Some(slug) = self.active.get(id).map(|active| active.fire.slug.clone()) else {
            return;
        };
        let state = match slot {
            DeliverySlot::None | DeliverySlot::Manager => None,
            DeliverySlot::Headless => Some(SlotState::Headless { slug }),
            DeliverySlot::Session => Some(SlotState::Session {
                slug,
                dispatched_at_ms: now_ms,
            }),
        };
        if let Some(state) = state {
            if let Some(active) = self.active.get_mut(id) {
                active.slot = state;
            }
        } else {
            self.active.remove(id);
        }
    }

    fn release_finished_slots(&mut self, now_ms: i64, activity: &RuntimeActivity) {
        self.active.retain(|_, active| match &active.slot {
            SlotState::Dispatching => true,
            SlotState::Headless { slug } => activity.headless_running.contains(slug),
            SlotState::Session {
                slug,
                dispatched_at_ms,
            } => {
                activity.busy_sessions.contains(slug)
                    && now_ms.saturating_sub(*dispatched_at_ms) <= SESSION_SLOT_MAX_MS
            }
        });
    }
}

fn pair_target(fire: &AutomationFire) -> String {
    fire.stack_id.clone().unwrap_or_else(|| fire.slug.clone())
}

fn breaker_exempt(fire: &AutomationFire, manager_run: bool) -> bool {
    fire.rule.on == wt_config::AutomationTrigger::BranchAdvanced
        || fire.rule.run == "builtin:notify"
        || fire.rule.run == "builtin:close-issue"
        || fire.rule.run == "builtin:delete-branch"
        || fire.frozen_vars.is_some()
        || manager_run
}

fn dispatch_kind(fire: &AutomationFire, audience: wt_automations::ActionAudience) -> DispatchKind {
    if fire.rule.run.starts_with("builtin:") || audience != wt_automations::ActionAudience::None {
        // Builtins and prompt sends have no durable headless action-run record
        // to prove delivery after a crash. Boot reconciliation keeps unmatched
        // entries ambiguous instead of replaying them.
        DispatchKind::ExternalSend
    } else {
        DispatchKind::HeadlessAction
    }
}

fn quiesce_block_reason(fire: &AutomationFire, input: &AutomationPassInput<'_>) -> Option<String> {
    let settle_ms = duration_ms(fire.rule.settle_seconds);
    for slug in &fire.quiesce_slugs {
        let Some(row) = input.rows.iter().find(|row| row.slug == *slug) else {
            continue;
        };
        if row.archived {
            return Some(format!("{slug} is being cleaned up"));
        }
        let state = input.quiescence.get(slug).copied().unwrap_or_default();
        if row.busy {
            return Some(format!("{slug} is busy"));
        }
        if state.locked {
            return Some(format!("{slug} is locked"));
        }
        if state.action_running {
            return Some(format!("action running on {slug}"));
        }
        if state.session_busy {
            return Some(format!("session busy on {slug}"));
        }
        if state
            .last_edit_at_ms
            .is_some_and(|at| input.now_ms.saturating_sub(at) < settle_ms)
        {
            return Some(format!("recent edits in {slug}"));
        }
    }
    None
}

fn duration_ms(seconds: f64) -> i64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        0
    } else {
        (seconds * 1000.0).min(i64::MAX as f64) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_config::AutomationTrigger;

    fn rule(run: &str, settle_seconds: f64) -> AutomationDef {
        AutomationDef {
            id: "test".into(),
            on: AutomationTrigger::WtCreated,
            run: run.into(),
            settle_seconds,
            ..AutomationDef::default()
        }
    }

    fn row() -> AutomationRow {
        AutomationRow {
            slug: "worktree".into(),
            branch: "worktree".into(),
            created_at: Some("2026-10-09T12:00:00Z".into()),
            ..AutomationRow::default()
        }
    }

    fn now_ms() -> i64 {
        (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
    }

    fn input<'a>(
        rules: &'a [AutomationDef],
        rows: &'a [AutomationRow],
        context: &'a AutomationEvalContext,
        requirements: &'a BTreeMap<String, RequirementState>,
        quiescence: &'a BTreeMap<String, WorktreeQuiescence>,
        activity: &'a RuntimeActivity,
        now_ms: i64,
    ) -> AutomationPassInput<'a> {
        AutomationPassInput {
            rules,
            rows,
            context,
            globally_paused: false,
            state_ready: true,
            enabled: true,
            now_ms,
            requirements,
            quiescence,
            activity,
        }
    }

    #[tokio::test]
    async fn pause_persists_pending_cancellation_before_dropping_queue() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let cancellation = CancellationToken::new();
        let mut engine = AutomationEngine::new(ledger.clone(), cancellation);
        let rules = [rule("builtin:clean", 600.0)];
        let rows = [row()];
        let context = AutomationEvalContext::default();
        let requirements = BTreeMap::new();
        let quiescence = BTreeMap::new();
        let activity = RuntimeActivity::default();
        let start = now_ms();

        let first = engine
            .tick(input(
                &rules,
                &rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                start,
            ))
            .await
            .unwrap();
        assert_eq!(first.pending, 1);
        assert!(first.claims.is_empty());

        let mut paused = input(
            &rules,
            &rows,
            &context,
            &requirements,
            &quiescence,
            &activity,
            start + 1,
        );
        paused.globally_paused = true;
        let result = engine.tick(paused).await.unwrap();
        assert_eq!(result.pending, 0);
        assert_eq!(engine.pending_count(), 0);
        assert!(
            ledger
                .snapshot()
                .await
                .unwrap()
                .has_handled("test:created:worktree:2026-10-09T12:00:00Z", start + 1)
        );
    }

    #[tokio::test]
    async fn unavailable_state_preserves_queue_and_waiting_requirement_keeps_settle_age() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let cancellation = CancellationToken::new();
        let mut engine = AutomationEngine::new(ledger, cancellation);
        let rules = [rule("custom-action", 1.0)];
        let rows = [row()];
        let context = AutomationEvalContext::default();
        let quiescence = BTreeMap::new();
        let activity = RuntimeActivity::default();
        let mut requirements =
            BTreeMap::from([("test|worktree".into(), RequirementState::Waiting)]);
        let start = now_ms();

        engine
            .tick(input(
                &rules,
                &rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                start,
            ))
            .await
            .unwrap();
        assert_eq!(engine.pending_count(), 1);

        let mut unavailable = input(
            &rules,
            &rows,
            &context,
            &requirements,
            &quiescence,
            &activity,
            start + 1_500,
        );
        unavailable.state_ready = false;
        engine.tick(unavailable).await.unwrap();
        assert_eq!(engine.pending_count(), 1);

        requirements.insert("test|worktree".into(), RequirementState::Ready);
        let ready = engine
            .tick(input(
                &rules,
                &rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                start + 1_500,
            ))
            .await
            .unwrap();
        assert_eq!(ready.claims.len(), 1);
    }

    #[tokio::test]
    async fn ambiguous_delivery_is_terminal_and_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let cancellation = CancellationToken::new();
        let mut engine = AutomationEngine::new(ledger, cancellation);
        let rules = [rule("builtin:notify", 0.0)];
        let rows = [row()];
        let context = AutomationEvalContext::default();
        let requirements = BTreeMap::new();
        let quiescence = BTreeMap::new();
        let activity = RuntimeActivity::default();
        let start = now_ms();
        let first = engine
            .tick(input(
                &rules,
                &rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                start,
            ))
            .await
            .unwrap();
        let claim = first.claims.first().unwrap();
        assert_eq!(claim.claim.kind(), DispatchKind::ExternalSend);
        engine
            .finish(
                claim,
                AutomationOutcome::Ambiguous {
                    reason: "notification acknowledgement lost".into(),
                    slot: DeliverySlot::None,
                },
                start + 1,
            )
            .await
            .unwrap();
        let next = engine
            .tick(input(
                &rules,
                &rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                start + 2,
            ))
            .await
            .unwrap();
        assert!(next.claims.is_empty());
    }
}
