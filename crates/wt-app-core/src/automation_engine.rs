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
/// A deferred fire is checked again this soon while it is young (a restack
/// usually retargets stacked PRs within a minute or two)...
const DEFER_RETRY_MS: i64 = 30 * 1000;
/// ...and this often once it is older than `DEFER_SLOW_AFTER_MS`.
const DEFER_SLOW_RETRY_MS: i64 = 5 * 60 * 1000;
const DEFER_SLOW_AFTER_MS: i64 = 10 * 60 * 1000;
/// A fire still deferred this long is reported once on the attention feed.
pub const DEFER_REPORT_AFTER_MS: i64 = 60 * 60 * 1000;

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
    /// Slugs with a live headless action tmux session (all active slugs
    /// while the tmux inventory is unconfirmed).
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
    /// Positive evidence that nothing was written, and the fire must run
    /// again later: it is requeued as is (frozen inputs included), so it
    /// outlives its worktree. Exempt from the breaker by construction, since
    /// nothing is recorded as dispatched.
    Deferred {
        reason: String,
        /// Facts the run learned that a retry needs after its worktree is
        /// gone (the proven merged tip). Carried into the fire's frozen vars.
        frozen: BTreeMap<String, String>,
    },
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

/// One queued fire and why the latest pass did not dispatch it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QueuedFire {
    pub rule: String,
    pub target: String,
    pub detail: String,
    pub queued_at_ms: i64,
    /// When its settle window ends; it dispatches then if nothing else holds it.
    pub settle_at_ms: i64,
    pub waiting_for: String,
}

/// A fire the engine has claimed and is still delivering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningFire {
    pub rule: String,
    pub target: String,
    pub detail: String,
    pub since_ms: Option<i64>,
}

/// Recorded fires kept for display.
const RECENT_HISTORY: usize = 10;

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
    /// When its intent first queued; a deferral keeps the age.
    queued_at_ms: i64,
    settle_at_ms: i64,
}

/// Why a requeued fire is waiting, and whether its age was reported.
#[derive(Clone, Debug)]
struct Deferral {
    reason: String,
    reported: bool,
    frozen: BTreeMap<String, String>,
}

/// Queue and ledger coordinator. The caller drives `tick` on source changes
/// and a bounded settle heartbeat, then executes each returned claim exactly
/// once and reports its final delivery evidence with `finish`.
pub struct AutomationEngine {
    ledger: AutomationLedger,
    cancellation: CancellationToken,
    intents: BTreeMap<String, AutomationIntent>,
    active: BTreeMap<String, ActiveFire>,
    /// Why each still-queued intent waited in the latest pass.
    waiting: BTreeMap<String, String>,
    /// Intents held only by unmet row-backed requirements. They stay queued
    /// with their settle age but are not reported as queued.
    unmet: BTreeSet<String>,
    /// The newest ledger records as of the latest pass, newest first.
    recent: Vec<wt_automations::LedgerRecord>,
    /// Deferred fires by identity, while their intent is queued or running.
    deferrals: BTreeMap<String, Deferral>,
}

impl AutomationEngine {
    pub fn new(ledger: AutomationLedger, cancellation: CancellationToken) -> Self {
        Self {
            ledger,
            cancellation,
            intents: BTreeMap::new(),
            active: BTreeMap::new(),
            waiting: BTreeMap::new(),
            unmet: BTreeSet::new(),
            recent: Vec::new(),
            deferrals: BTreeMap::new(),
        }
    }

    /// Fires being delivered now, oldest first.
    pub fn running(&self) -> Vec<RunningFire> {
        let mut running = self
            .active
            .values()
            .map(|active| RunningFire {
                rule: active.fire.rule.id.clone(),
                target: pair_target(&active.fire),
                detail: active.fire.detail.clone(),
                since_ms: match &active.slot {
                    SlotState::Session {
                        dispatched_at_ms, ..
                    } => Some(*dispatched_at_ms),
                    _ => None,
                },
            })
            .collect::<Vec<_>>();
        running.sort_by(|a, b| (a.since_ms, &a.rule).cmp(&(b.since_ms, &b.rule)));
        running
    }

    /// The newest recorded fires as of the latest pass, newest first.
    pub fn recent(&self) -> &[wt_automations::LedgerRecord] {
        &self.recent
    }

    /// Every retained intent, including requirement-held ones.
    #[cfg(test)]
    pub fn pending_count(&self) -> usize {
        self.intents.len()
    }

    /// The fires every retained intent carries, including frozen post-merge
    /// intents whose worktree has gone. Their requirements are decided from
    /// these, since the current evaluation no longer produces them.
    pub fn retained_fires(&self) -> Vec<AutomationFire> {
        self.intents
            .values()
            .map(|intent| intent.fire.clone())
            .collect()
    }

    /// The queue as of the latest pass, oldest first, without intents held
    /// only by unmet requirements (docs/automations.md does not count those).
    pub fn queued(&self) -> Vec<QueuedFire> {
        let mut queued = self
            .intents
            .iter()
            .filter(|(id, _)| !self.unmet.contains(*id))
            .map(|(id, intent)| QueuedFire {
                rule: intent.fire.rule.id.clone(),
                target: pair_target(&intent.fire),
                detail: intent.fire.detail.clone(),
                queued_at_ms: intent.queued_at_ms,
                settle_at_ms: intent.settle_at_ms,
                waiting_for: self
                    .waiting
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| "its turn in the queue".into()),
            })
            .collect::<Vec<_>>();
        queued.sort_by(|a, b| (a.queued_at_ms, &a.rule).cmp(&(b.queued_at_ms, &b.rule)));
        queued
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

    /// Requeue the fires a previous process deferred. Their worktrees may be
    /// gone, so evaluation alone would never produce them again. A record
    /// this build cannot read is left for a build that can.
    pub async fn restore_deferred(&mut self, now_ms: i64) -> Result<usize> {
        let records = self
            .ledger
            .deferred()
            .await
            .context("read deferred automation fires")?;
        let mut restored = 0;
        for (id, record) in records {
            let Ok(fire) = serde_json::from_value::<AutomationFire>(record["fire"].clone()) else {
                tracing::warn!(%id, "skipping an unreadable deferred automation fire");
                continue;
            };
            let queued_at_ms = record["queuedAtMs"].as_i64().unwrap_or(now_ms);
            let frozen = serde_json::from_value(record["frozen"].clone()).unwrap_or_default();
            self.deferrals.insert(
                id.clone(),
                Deferral {
                    reason: record["reason"].as_str().unwrap_or_default().to_owned(),
                    reported: false,
                    frozen,
                },
            );
            self.intents.insert(
                id,
                AutomationIntent {
                    fire,
                    queued_at_ms,
                    settle_at_ms: now_ms.saturating_add(DEFER_RETRY_MS),
                },
            );
            restored += 1;
        }
        Ok(restored)
    }

    pub async fn tick(&mut self, input: AutomationPassInput<'_>) -> Result<AutomationTick> {
        let mut report = AutomationTick::default();
        match self.pass(input, &mut report).await {
            Ok(()) => Ok(report),
            Err(error) => {
                self.release_unstarted(report.claims).await;
                Err(error)
            }
        }
    }

    /// Give back the claims a failed pass made. The caller never received
    /// them, so no dispatch began and a release cannot replay one. Left
    /// `dispatched`, they would hold their slots until restart and then
    /// become `ambiguous`, which is never retried.
    async fn release_unstarted(&mut self, claims: Vec<ClaimedFire>) {
        // The pass usually failed because its own token was cancelled at quit.
        let cancellation = CancellationToken::new();
        for claim in claims {
            let Some(active) = self.active.remove(&claim.id) else {
                continue;
            };
            if let Err(error) = self
                .ledger
                .release_not_started(&active.claim, &cancellation)
                .await
            {
                tracing::warn!(rule = %active.fire.rule.id, %error, "cannot release an automation claim that never started");
            }
            self.intents.insert(
                claim.id,
                AutomationIntent {
                    fire: active.fire,
                    queued_at_ms: active.queued_at_ms,
                    settle_at_ms: active.settle_at_ms,
                },
            );
        }
    }

    async fn pass(
        &mut self,
        input: AutomationPassInput<'_>,
        report: &mut AutomationTick,
    ) -> Result<()> {
        self.release_finished_slots(input.now_ms, input.activity);
        if !input.state_ready {
            // A transient store read failure must pause dispatch, not silently
            // discard the in-memory queue or pretend its cancellation persisted.
            // Git and metadata refresh often, so the queue keeps the last
            // pass's reasons; clearing them would flash requirement-held
            // intents into the reported queue on every refresh.
            return Ok(());
        }
        // Reasons describe this pass only.
        self.waiting.clear();
        self.unmet.clear();
        if input.globally_paused || !input.enabled {
            self.cancel_pending(input).await?;
            return Ok(());
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
        self.recent = ledger.recent(RECENT_HISTORY);
        let handled = candidates
            .into_iter()
            .filter(|key| ledger.has_handled(key, input.now_ms))
            .collect::<BTreeSet<_>>();
        let executing = self.active.keys().cloned().collect::<BTreeSet<_>>();
        let deferred = self.deferrals.keys().cloned().collect::<BTreeSet<_>>();
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
            deferred: &deferred,
        });
        self.intents = intents
            .into_iter()
            .map(|intent| (fire_identity(&intent.fire), intent))
            .collect();
        self.deferrals
            .retain(|id, _| self.intents.contains_key(id) || self.active.contains_key(id));

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
                && manager_bound(&active.fire, input.context)
            {
                manager_in_flight = true;
            }
        }

        for intent in queue {
            if self.active.len() >= MAX_CONCURRENT {
                self.waiting.insert(
                    fire_identity(&intent.fire),
                    format!("{MAX_CONCURRENT} automations already running"),
                );
                continue;
            }
            let fire = intent.fire;
            let id = fire_identity(&fire);
            let rule = &fire.rule;
            let manager_run = manager_bound(&fire, input.context);
            let audience = input
                .context
                .actions
                .get(&rule.run)
                .map_or(wt_automations::ActionAudience::None, |traits| {
                    traits.audience
                });
            let breaker_exempt = breaker_exempt(&fire, manager_run);

            if self.deferrals.contains_key(&id) && input.context.pauses.slugs.contains(&fire.slug) {
                self.waiting.insert(id, "its worktree to be resumed".into());
                continue;
            }
            if fire
                .quiesce_slugs
                .iter()
                .any(|slug| occupied_slugs.contains(slug))
            {
                self.waiting
                    .insert(id, "another automation on the same worktree".into());
                continue;
            }
            if rule.run == "builtin:restack"
                && fire
                    .stack_id
                    .as_ref()
                    .is_some_and(|id| restack_stacks.contains(id))
            {
                self.waiting
                    .insert(id, "a restack of the same stack".into());
                continue;
            }
            if manager_run && manager_in_flight {
                self.waiting.insert(id, "another manager prompt".into());
                continue;
            }

            let requirements = input.requirements.get(&id);
            if matches!(requirements, Some(RequirementState::Waiting)) {
                // Preserve the original settle age while a row-backed
                // prerequisite changes. The trigger still has to remain true.
                self.unmet.insert(id);
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
                self.waiting
                    .insert(id, "action requirements to be evaluated".into());
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
                self.waiting.insert(id, "its cooldown".into());
                continue;
            }
            if input.now_ms < intent.settle_at_ms {
                let reason = match self.deferrals.get(&id) {
                    Some(deferral) => format!("a re-check: {}", deferral.reason),
                    None => "its settle delay".into(),
                };
                self.waiting.insert(id, reason);
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
                } else {
                    self.waiting.insert(id, reason);
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
                DispatchClaimResult::CoolingDown { .. } => {
                    self.waiting.insert(id, "its cooldown".into());
                }
                DispatchClaimResult::BreakerReserved { .. } => {
                    self.waiting
                        .insert(id, "an earlier attempt to be confirmed".into());
                }
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
                    let mut fire = fire;
                    if let Some(deferral) = self.deferrals.get(&id)
                        && !deferral.frozen.is_empty()
                    {
                        fire.frozen_vars
                            .get_or_insert_with(BTreeMap::new)
                            .extend(deferral.frozen.clone());
                    }
                    self.active.insert(
                        id.clone(),
                        ActiveFire {
                            claim: claim.clone(),
                            fire: fire.clone(),
                            slot: SlotState::Dispatching,
                            queued_at_ms: intent.queued_at_ms,
                            settle_at_ms: intent.settle_at_ms,
                        },
                    );
                    report.claims.push(ClaimedFire { id, fire, claim });
                }
            }
        }
        report.pending = self.intents.len();
        Ok(())
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
        // A deferred fire is work already decided on, not a queued trigger:
        // pausing or clearing the queue must not consume its key, or a
        // deferred remote branch delete would never run.
        let fires = fires
            .into_iter()
            .filter(|fire| !self.deferrals.contains_key(&fire_identity(fire)))
            .collect::<Vec<_>>();
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
        let deferrals = &self.deferrals;
        self.intents.retain(|id, _| deferrals.contains_key(id));
        Ok(count)
    }

    /// Complete one claimed delivery. The same claim id scopes every ledger
    /// transition, so a stale completion cannot overwrite a later key state.
    #[cfg(test)]
    pub async fn finish(
        &mut self,
        claim: &ClaimedFire,
        outcome: AutomationOutcome,
        now_ms: i64,
    ) -> Result<Option<String>> {
        self.finish_with_cancellation(claim, outcome, now_ms, &self.cancellation.clone())
            .await
    }

    /// An admitted dispatch must persist its outcome through the command
    /// drain even after the read/evaluation scope has been cancelled.
    /// Returns an attention line when a deferred fire has now waited longer
    /// than `DEFER_REPORT_AFTER_MS`, once per fire.
    pub async fn finish_with_cancellation(
        &mut self,
        claim: &ClaimedFire,
        outcome: AutomationOutcome,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>> {
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
        let slot = match &outcome {
            AutomationOutcome::Delivered { slot } | AutomationOutcome::Ambiguous { slot, .. } => {
                *slot
            }
            AutomationOutcome::NotStarted | AutomationOutcome::Deferred { .. } => {
                DeliverySlot::None
            }
        };
        let recorded = self
            .record_outcome(claim, ledger_claim, outcome, now_ms, cancellation)
            .await;
        // A failed ledger write must not leave the slot `Dispatching`, which
        // nothing else ever clears. The key stays handled in the ledger, so
        // freeing the slot cannot run the fire again.
        if recorded.is_err()
            && self
                .active
                .get(&claim.id)
                .is_some_and(|active| active.slot == SlotState::Dispatching)
        {
            self.finish_slot(&claim.id, slot, now_ms);
        }
        let notice = recorded?;
        // Display only: the overlay's history shows the outcome now rather
        // than at the next pass. A failed read keeps the older list.
        if let Ok(ledger) = self.ledger.snapshot().await {
            self.recent = ledger.recent(RECENT_HISTORY);
        }
        Ok(notice)
    }

    async fn record_outcome(
        &mut self,
        claim: &ClaimedFire,
        ledger_claim: DispatchClaim,
        outcome: AutomationOutcome,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>> {
        let mut notice = None;
        match outcome {
            AutomationOutcome::NotStarted => {
                self.ledger
                    .release_not_started(&ledger_claim, cancellation)
                    .await
                    .context("release automation claim proven not started")?;
                let active = self.active.remove(&claim.id);
                // A deferred fire may have no row left to re-evaluate it;
                // keep it queued (its ledger record stays) instead of losing it.
                if let Some(active) = active
                    && self.deferrals.contains_key(&claim.id)
                {
                    self.intents.insert(
                        claim.id.clone(),
                        AutomationIntent {
                            fire: active.fire,
                            queued_at_ms: active.queued_at_ms,
                            settle_at_ms: now_ms.saturating_add(DEFER_RETRY_MS),
                        },
                    );
                }
            }
            AutomationOutcome::Deferred { reason, frozen } => {
                let active = self
                    .active
                    .get(&claim.id)
                    .context("deferred automation dispatch is not active")?;
                let mut carried = self
                    .deferrals
                    .get(&claim.id)
                    .map(|deferral| deferral.frozen.clone())
                    .unwrap_or_default();
                carried.extend(frozen);
                let record = serde_json::json!({
                    "fire": serde_json::to_value(&active.fire).context("encode deferred fire")?,
                    "queuedAtMs": active.queued_at_ms,
                    "reason": reason,
                    "frozen": carried,
                });
                self.ledger
                    .release_deferred(&ledger_claim, &claim.id, record, cancellation)
                    .await
                    .context("record deferred automation fire")?;
                let active = self
                    .active
                    .remove(&claim.id)
                    .context("deferred automation dispatch is not active")?;
                let age = now_ms.saturating_sub(active.queued_at_ms);
                let delay = if age < DEFER_SLOW_AFTER_MS {
                    DEFER_RETRY_MS
                } else {
                    DEFER_SLOW_RETRY_MS
                };
                let reported = self
                    .deferrals
                    .get(&claim.id)
                    .is_some_and(|deferral| deferral.reported);
                let report = !reported && age >= DEFER_REPORT_AFTER_MS;
                if report {
                    notice = Some(format!(
                        "auto {}: still waiting after {} min: {reason}",
                        active.fire.rule.id,
                        age / 60_000
                    ));
                }
                self.deferrals.insert(
                    claim.id.clone(),
                    Deferral {
                        reason,
                        reported: reported || report,
                        frozen: carried,
                    },
                );
                self.intents.insert(
                    claim.id.clone(),
                    AutomationIntent {
                        fire: active.fire,
                        queued_at_ms: active.queued_at_ms,
                        settle_at_ms: now_ms.saturating_add(delay),
                    },
                );
            }
            AutomationOutcome::Delivered { slot } => {
                self.ledger
                    .mark_delivered(&ledger_claim, now_ms, cancellation)
                    .await
                    .context("mark automation delivered")?;
                self.forget_deferral(&claim.id, cancellation).await?;
                self.finish_slot(&claim.id, slot, now_ms);
            }
            AutomationOutcome::Ambiguous { reason, slot } => {
                self.ledger
                    .mark_ambiguous(&ledger_claim, now_ms, &reason, cancellation)
                    .await
                    .context("mark automation delivery ambiguous")?;
                self.forget_deferral(&claim.id, cancellation).await?;
                self.finish_slot(&claim.id, slot, now_ms);
            }
        }
        Ok(notice)
    }

    async fn forget_deferral(&mut self, id: &str, cancellation: &CancellationToken) -> Result<()> {
        if self.deferrals.remove(id).is_some() {
            self.ledger
                .clear_deferred(id, cancellation)
                .await
                .context("clear deferred automation fire")?;
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

/// Whether the fire is delivered to the manager: its action targets the
/// manager, or it is an MCP event no worktree owns, which goes there instead
/// of the session its action names.
fn manager_bound(fire: &AutomationFire, context: &wt_automations::AutomationEvalContext) -> bool {
    let audience = context
        .actions
        .get(&fire.rule.run)
        .map(|traits| traits.audience);
    audience == Some(wt_automations::ActionAudience::Manager)
        || (fire.mcp_event.is_some()
            && fire.slug == wt_automations::FLEET_SLUG
            && audience == Some(wt_automations::ActionAudience::Session))
}

fn breaker_exempt(fire: &AutomationFire, manager_run: bool) -> bool {
    fire.rule.on == wt_config::AutomationTrigger::BranchAdvanced
        // Each event is a distinct occurrence, and the server leaves out the
        // agents' own writes, so a reply cannot wake its sender in a loop.
        || fire.rule.on == wt_config::AutomationTrigger::McpEvent
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
    use std::future::Future as _;

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

    /// `wt automations` and the queue log name why each fire still waits.
    #[tokio::test]
    async fn queued_fires_report_what_they_wait_for() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger, CancellationToken::new());
        let rules = [rule("custom-action", 0.0)];
        let rows = [row()];
        let context = AutomationEvalContext::default();
        let activity = RuntimeActivity::default();
        let start = now_ms();
        let mut requirements =
            BTreeMap::from([("test|worktree".into(), RequirementState::Waiting)]);
        let busy = BTreeMap::from([(
            "worktree".to_owned(),
            WorktreeQuiescence {
                session_busy: true,
                ..WorktreeQuiescence::default()
            },
        )]);
        let mut waiting_for = Vec::new();
        for (step, quiescence) in [&BTreeMap::new(), &busy].into_iter().enumerate() {
            if step == 1 {
                requirements.insert("test|worktree".into(), RequirementState::Ready);
            }
            let tick = engine
                .tick(input(
                    &rules,
                    &rows,
                    &context,
                    &requirements,
                    quiescence,
                    &activity,
                    start + step as i64,
                ))
                .await
                .unwrap();
            assert!(tick.claims.is_empty());
            let queued = engine.queued();
            waiting_for.push(queued.first().map(|fire| {
                assert_eq!(
                    (fire.rule.as_str(), fire.target.as_str()),
                    ("test", "worktree")
                );
                fire.waiting_for.clone()
            }));
        }
        // An unmet requirement is not reported as queued (docs/automations.md);
        // once met, the fire waits visibly for the busy session.
        assert_eq!(
            waiting_for,
            [None, Some("session busy on worktree".to_owned())]
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
        // Held only by its requirement, so not reported as queued.
        assert!(engine.queued().is_empty());

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
        // A pass without state changes nothing the queue reports.
        assert!(engine.queued().is_empty());

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
    async fn accepted_dispatch_finishes_after_evaluation_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("automations.json");
        let ledger = AutomationLedger::new(ledger_path.clone());
        let evaluation = CancellationToken::new();
        let mut engine = AutomationEngine::new(ledger, evaluation.clone());
        let rules = [rule("builtin:notify", 0.0)];
        let rows = [row()];
        let context = AutomationEvalContext::default();
        let requirements = BTreeMap::new();
        let quiescence = BTreeMap::new();
        let activity = RuntimeActivity::default();
        let start = now_ms();
        let tick = engine
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
        let claim = tick.claims.first().unwrap();
        evaluation.cancel();
        let commands = CancellationToken::new();
        engine
            .finish_with_cancellation(
                claim,
                AutomationOutcome::Delivered {
                    slot: DeliverySlot::None,
                },
                start + 1,
                &commands,
            )
            .await
            .unwrap();
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(ledger_path).unwrap()).unwrap();
        assert!(
            persisted["fired"]
                .as_object()
                .unwrap()
                .values()
                .all(|entry| entry["state"] == "delivered"),
            "{persisted}"
        );
        assert!(!commands.is_cancelled());
    }

    /// A deferred remote branch delete (something is still stacked on the
    /// branch) outlives its worktree and the breaker, is re-checked on a
    /// backoff rather than immediately, and reports once when it lingers.
    #[tokio::test]
    async fn deferred_fire_is_requeued_past_row_death_until_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let rules = [AutomationDef {
            id: "delete-branch-on-merge".into(),
            on: AutomationTrigger::WtMerged,
            run: "builtin:delete-branch".into(),
            settle_seconds: 0.0,
            ..AutomationDef::default()
        }];
        let landed = [AutomationRow {
            local_merged: Some(true),
            clean_candidate: true,
            ..row()
        }];
        let context = AutomationEvalContext::default();
        let requirements = BTreeMap::new();
        let quiescence = BTreeMap::new();
        let activity = RuntimeActivity::default();
        let start = now_ms();
        let pass = |rows, at| {
            input(
                &rules,
                rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                at,
            )
        };

        let first = engine.tick(pass(&landed, start)).await.unwrap();
        let claim = first.claims.first().unwrap().clone();
        assert_eq!(claim.fire().delete_branch.as_deref(), Some("worktree"));
        let notice = engine
            .finish(
                &claim,
                AutomationOutcome::Deferred {
                    reason: "open PR(s) #7 still target worktree".into(),
                    frozen: BTreeMap::from([("delete_branch_tip".into(), "abc123".into())]),
                },
                start + 1,
            )
            .await
            .unwrap();
        assert_eq!(notice, None);

        // A restart keeps it: a fresh engine on the same ledger requeues it,
        // since nothing would evaluate a removed row again.
        drop(engine);
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        assert_eq!(engine.restore_deferred(start + 1).await.unwrap(), 1);
        // Pausing or clearing the queue does not consume it either.
        let mut paused = pass(&[], start + 1);
        paused.globally_paused = true;
        engine.cancel_pending(paused).await.unwrap();
        assert_eq!(engine.pending_count(), 1);

        // Still landed, so the condition re-fires, but the re-check waits.
        let early = engine.tick(pass(&landed, start + 2)).await.unwrap();
        assert!(early.claims.is_empty());
        assert!(
            engine.queued()[0].waiting_for.contains("#7"),
            "{:?}",
            engine.queued()
        );

        // The restack removed the worktree; the deferred fire stays queued.
        let mut at = start + 1 + DEFER_RETRY_MS;
        let gone = engine.tick(pass(&[], at - 1)).await.unwrap();
        assert!(gone.claims.is_empty());
        assert_eq!(engine.pending_count(), 1);

        // Deferred over and over well past the breaker limit, and reported
        // exactly once after an hour.
        let mut notices = Vec::new();
        let mut attempts = 0;
        while at < start + 2 * DEFER_REPORT_AFTER_MS {
            let tick = engine.tick(pass(&[], at)).await.unwrap();
            let Some(claim) = tick.claims.first() else {
                panic!("deferred fire was not re-dispatched at +{}ms", at - start);
            };
            // The tip proven before the worktree went is still carried.
            assert_eq!(
                claim
                    .fire()
                    .frozen_vars
                    .as_ref()
                    .and_then(|vars| vars.get("delete_branch_tip"))
                    .map(String::as_str),
                Some("abc123")
            );
            attempts += 1;
            notices.extend(
                engine
                    .finish(
                        claim,
                        AutomationOutcome::Deferred {
                            reason: "open PR(s) #7 still target worktree".into(),
                            frozen: BTreeMap::new(),
                        },
                        at,
                    )
                    .await
                    .unwrap(),
            );
            at += DEFER_SLOW_RETRY_MS;
        }
        assert!(attempts > wt_automations::BREAKER_LIMIT as usize);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("#7"), "{notices:?}");

        // Children retargeted: the delete runs and the fire is done.
        let last = engine.tick(pass(&[], at)).await.unwrap();
        let claim = last.claims.first().unwrap();
        engine
            .finish(
                claim,
                AutomationOutcome::Delivered {
                    slot: DeliverySlot::None,
                },
                at,
            )
            .await
            .unwrap();
        let after = engine
            .tick(pass(&landed, at + DEFER_SLOW_RETRY_MS))
            .await
            .unwrap();
        assert!(after.claims.is_empty());
        assert_eq!(engine.pending_count(), 0);
        assert!(ledger.deferred().await.unwrap().is_empty());
    }

    /// Quitting between two claims of one pass must not strand the first:
    /// nobody was handed it, so a restart would find it `dispatched` with no
    /// proof and keep it `ambiguous`, never retried.
    #[tokio::test]
    async fn claim_made_before_a_failed_pass_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("automations.json");
        let ledger = AutomationLedger::new(ledger_path.clone());
        let evaluation = CancellationToken::new();
        let mut engine = AutomationEngine::new(ledger.clone(), evaluation.clone());
        let rules = [rule("builtin:notify", 0.0)];
        let rows = ["first", "second"].map(|slug| AutomationRow {
            slug: slug.into(),
            branch: slug.into(),
            ..row()
        });
        let context = AutomationEvalContext::default();
        let requirements = BTreeMap::new();
        let quiescence = BTreeMap::new();
        let activity = RuntimeActivity::default();
        let start = now_ms();
        let pass = |at| {
            input(
                &rules,
                &rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                at,
            )
        };

        // Cancel evaluation as soon as the first claim is on disk, before
        // the pass reaches the second fire's ledger lock.
        let result = {
            let mut tick = std::pin::pin!(engine.tick(pass(start)));
            std::future::poll_fn(|cx| {
                let polled = tick.as_mut().poll(cx);
                if polled.is_pending()
                    && std::fs::read_to_string(&ledger_path)
                        .is_ok_and(|ledger| ledger.contains("dispatched"))
                {
                    evaluation.cancel();
                }
                polled
            })
            .await
        };
        assert!(result.is_err(), "the second claim should be cancelled");
        assert!(engine.running().is_empty(), "{:?}", engine.running());
        assert_eq!(engine.pending_count(), 2);

        let mut restarted = AutomationEngine::new(ledger, CancellationToken::new());
        assert_eq!(
            restarted
                .reconcile_boot(&BTreeSet::new(), start + 1)
                .await
                .unwrap(),
            (0, 0)
        );
        let tick = restarted.tick(pass(start + 2)).await.unwrap();
        assert_eq!(tick.claims.len(), 2);
    }

    /// A finish whose ledger write fails must still free its slot: the
    /// ledger keeps the key handled, but two stuck slots stop every rule.
    #[tokio::test]
    async fn failed_finish_frees_its_slot() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("automations.json");
        let ledger = AutomationLedger::new(ledger_path.clone());
        let mut engine = AutomationEngine::new(ledger, CancellationToken::new());
        let rules = [rule("builtin:notify", 0.0)];
        let context = AutomationEvalContext::default();
        let requirements = BTreeMap::new();
        let quiescence = BTreeMap::new();
        let activity = RuntimeActivity::default();
        let start = now_ms();
        let rows = [row()];
        let tick = engine
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
        let claim = tick.claims.first().unwrap();

        // A directory where the ledger file belongs fails every read.
        let aside = dir.path().join("aside.json");
        std::fs::rename(&ledger_path, &aside).unwrap();
        std::fs::create_dir(&ledger_path).unwrap();
        engine
            .finish(
                claim,
                AutomationOutcome::Delivered {
                    slot: DeliverySlot::None,
                },
                start + 1,
            )
            .await
            .unwrap_err();
        std::fs::remove_dir(&ledger_path).unwrap();
        std::fs::rename(&aside, &ledger_path).unwrap();
        assert!(engine.running().is_empty(), "{:?}", engine.running());

        // The same worktree can run its next fire.
        let recreated = [AutomationRow {
            created_at: Some("2026-10-10T12:00:00Z".into()),
            ..row()
        }];
        let next = engine
            .tick(input(
                &rules,
                &recreated,
                &context,
                &requirements,
                &quiescence,
                &activity,
                start + 2,
            ))
            .await
            .unwrap();
        assert_eq!(next.claims.len(), 1);
    }

    /// Pausing a removed worktree (Ctrl+A in the history) holds its deferred
    /// branch delete; nothing would evaluate that row again after a resume.
    #[tokio::test]
    async fn worktree_pause_holds_a_deferred_fire_until_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger, CancellationToken::new());
        let rules = [AutomationDef {
            id: "delete-branch-on-merge".into(),
            on: AutomationTrigger::WtMerged,
            run: "builtin:delete-branch".into(),
            settle_seconds: 0.0,
            ..AutomationDef::default()
        }];
        let landed = [AutomationRow {
            local_merged: Some(true),
            clean_candidate: true,
            ..row()
        }];
        let context = AutomationEvalContext::default();
        let mut paused = AutomationEvalContext::default();
        paused.pauses.slugs.insert("worktree".into());
        let requirements = BTreeMap::new();
        let quiescence = BTreeMap::new();
        let activity = RuntimeActivity::default();
        let start = now_ms();
        let pass = |rows, context, at| {
            input(
                &rules,
                rows,
                context,
                &requirements,
                &quiescence,
                &activity,
                at,
            )
        };

        let first = engine.tick(pass(&landed, &context, start)).await.unwrap();
        let claim = first.claims.first().unwrap().clone();
        engine
            .finish(
                &claim,
                AutomationOutcome::Deferred {
                    reason: "open PR(s) #7 still target worktree".into(),
                    frozen: BTreeMap::new(),
                },
                start + 1,
            )
            .await
            .unwrap();

        let due = start + 1 + DEFER_RETRY_MS;
        let held = engine.tick(pass(&[], &paused, due)).await.unwrap();
        assert!(held.claims.is_empty());
        assert_eq!(engine.pending_count(), 1);
        assert!(
            engine.queued()[0].waiting_for.contains("resumed"),
            "{:?}",
            engine.queued()
        );

        let resumed = engine.tick(pass(&[], &context, due + 1)).await.unwrap();
        assert_eq!(resumed.claims.len(), 1);
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
