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
    DispatchRequest, LOCAL_SIGHTING_WINDOW_MS, QueueIntentsInput, cancellable_fires, evaluate,
    evaluate_breaker_resets, fire_identity, queue_intents,
};
use wt_config::{AutomationBusyPolicy, AutomationDef};

/// Fires that hold something for minutes at a time: a prompt into a
/// worktree's session, a headless agent run, or a restack or clean that
/// rewrites checkouts. Each occupies an agent or a stack, so few run at once.
const MAX_HEAVY: usize = 2;
/// Bookkeeping fires that end in seconds: a shell action, a notification, a
/// tracker or branch update, a manager prompt. They have their own slots so
/// none waits minutes behind an agent's turn. Each shell run is one command
/// in its own worker process, so this is also the most automation shell
/// commands running at once.
const MAX_LIGHT: usize = 4;
const SESSION_SLOT_MAX_MS: i64 = 10 * 60 * 1000;
/// A deferred fire is checked again this soon while it is young (a restack
/// usually retargets stacked PRs within a minute or two)...
const DEFER_RETRY_MS: i64 = 30 * 1000;
/// ...and this often once it is older than `DEFER_SLOW_AFTER_MS`.
const DEFER_SLOW_RETRY_MS: i64 = 5 * 60 * 1000;
const DEFER_SLOW_AFTER_MS: i64 = 10 * 60 * 1000;
/// How long a deferred or retried fire's backoff is kept while no pass
/// produces the fire. A condition read from GitHub holds only while that
/// data is fresh, which on a quiet fleet is a minute or two in every ten:
/// forgetting the backoff at once restarted the retry count each time the
/// fire came back, so a failing run never reached its last retry.
const DORMANT_MAX_MS: i64 = 60 * 60 * 1000;
/// A fire still deferred this long is reported once on the attention feed.
pub const DEFER_REPORT_AFTER_MS: i64 = 60 * 60 * 1000;
/// How long after each failed run of a shell action the next one waits. A
/// run that fails after the last of these ends the fire as failed.
const RUN_RETRY_DELAYS_MS: [i64; 3] = [60 * 1000, 5 * 60 * 1000, 30 * 60 * 1000];
/// A launched run whose end nobody reported for this long is no longer
/// waited for, by this process or one started later; its fire stays
/// delivered.
const AWAIT_RUN_MAX_MS: i64 = 24 * 60 * 60 * 1000;

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
    /// The action launched or completed. A failure a built-in or a session
    /// reports is still delivered and is not replayed; only a launched shell
    /// command's non-zero exit is, through [`AutomationEngine::run_ended`].
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

/// A fire the engine ended as a terminal skip, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedFire {
    pub slug: String,
    pub rule: String,
    pub reason: String,
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
    /// The fire's identity: with `queued_at_ms` it names one wait in the
    /// queue, so the log's queued and left records pair exactly.
    pub id: String,
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

/// A watch action the ledger shows ended while its watch still lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettledWatch {
    pub id: String,
    pub action: wt_core::watch::WatchAction,
    /// Whether the owner's work had landed, as this pass evaluated it: what
    /// decides whether a settled wake stays owed as a due step.
    pub landing: wt_core::OwnerLanding,
    /// The ledger does not show the fire delivered: it was sent without
    /// proof, or ended without being sent.
    pub unconfirmed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct AutomationTick {
    pub pending: usize,
    pub claims: Vec<ClaimedFire>,
    /// Watch actions the ledger shows ended while the watch still lists
    /// them: the process stopped between recording the fire and updating the
    /// watch. The caller removes each from its watch.
    pub settled_watches: Vec<SettledWatch>,
}

/// Which budget a fire's slot comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CostClass {
    Heavy,
    Light,
}

impl CostClass {
    const fn cap(self) -> usize {
        match self {
            Self::Heavy => MAX_HEAVY,
            Self::Light => MAX_LIGHT,
        }
    }

    const fn noun(self) -> &'static str {
        match self {
            Self::Heavy => "agent or stack",
            Self::Light => "shell or built-in",
        }
    }
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
    class: CostClass,
    /// When its intent first queued; a deferral keeps the age.
    queued_at_ms: i64,
    settle_at_ms: i64,
    retry_on_failure: bool,
}

/// Why a requeued fire is waiting, and whether its age was reported.
#[derive(Clone, Debug)]
struct Deferral {
    reason: String,
    reported: bool,
    frozen: BTreeMap<String, String>,
    /// Runs of this fire that exited non-zero so far.
    failed_runs: u32,
    /// The intent as it was when a pass stopped producing the fire, and
    /// when that was. It resumes from there if the same occurrence returns.
    dormant: Option<(i64, AutomationIntent)>,
}

/// A launched headless action whose exit decides whether its fire is done.
#[derive(Clone, Debug)]
struct AwaitedRun {
    claim: DispatchClaim,
    fire: AutomationFire,
    queued_at_ms: i64,
    delivered_at_ms: i64,
    failed_runs: u32,
    retry_on_failure: bool,
    /// Read back from the ledger: a process that has since quit launched
    /// it, and no run record has been matched to it yet.
    restored: bool,
}

impl AwaitedRun {
    /// What the ledger keeps so a later process can report the run's exit.
    fn record(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "claim": serde_json::to_value(&self.claim).context("encode awaited claim")?,
            "fire": serde_json::to_value(&self.fire).context("encode awaited fire")?,
            "queuedAtMs": self.queued_at_ms,
            "deliveredAtMs": self.delivered_at_ms,
            "failedRuns": self.failed_runs,
            "retryOnFailure": self.retry_on_failure,
        }))
    }

    fn from_record(record: &serde_json::Value) -> Option<Self> {
        Some(Self {
            claim: serde_json::from_value(record["claim"].clone()).ok()?,
            fire: serde_json::from_value(record["fire"].clone()).ok()?,
            queued_at_ms: record["queuedAtMs"].as_i64()?,
            delivered_at_ms: record["deliveredAtMs"].as_i64()?,
            failed_runs: u32::try_from(record["failedRuns"].as_u64()?).unwrap_or(u32::MAX),
            retry_on_failure: record["retryOnFailure"].as_bool()?,
            restored: true,
        })
    }
}

/// How a launched run ended, as its durable record shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunEnd {
    /// A shell command ran to its end and exited non-zero: a definite
    /// failure, unlike a lost reply or a killed run.
    ShellFailed {
        exit_code: i32,
    },
    /// A shell command's worker died before it recorded an exit code: the
    /// command may have succeeded.
    ShellUnknown,
    Other,
}

/// What is said of a run whose worker died before recording an exit code.
pub const RUN_EXIT_UNRECORDED: &str = "its run ended without recording an exit code";
/// What is said of a run a process that has quit launched, when no record
/// of the run is left.
pub const RUN_RECORD_MISSING: &str = "no record of its run is left";

/// A fire whose action retries on failure and whose run's exit cannot be
/// recovered. It may have succeeded, so it is not run again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownRun {
    pub slug: String,
    pub rule: String,
    pub reason: String,
}

/// A fire whose shell action exited non-zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedRun {
    pub slug: String,
    pub rule: String,
    pub exit_code: i32,
    /// Failed runs of this fire so far, this one included.
    pub runs: u32,
    /// Whether its action retries; the action's own failure line is then
    /// kept off the attention feed and this report is the only one.
    pub retried: bool,
    /// When it runs again; `None` when it has ended as failed.
    pub retry_in_ms: Option<i64>,
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
    /// Intents that only their settle window holds back as of the latest
    /// pass: nothing else would stop them dispatching when it ends.
    settling: BTreeSet<String>,
    /// Intents held only by unmet row-backed requirements. They stay queued
    /// with their settle age but are not reported as queued.
    unmet: BTreeSet<String>,
    /// The newest ledger records as of the latest pass, newest first.
    recent: Vec<wt_automations::LedgerRecord>,
    /// Deferred fires by identity, while their intent is queued or running.
    deferrals: BTreeMap<String, Deferral>,
    /// Delivered shell runs by fire identity, until their exit is reported.
    awaited: BTreeMap<String, AwaitedRun>,
    /// Fires ended as skipped since the last [`AutomationEngine::take_skipped`].
    skipped: Vec<SkippedFire>,
    /// Runs ended with an unknown exit since the last
    /// [`AutomationEngine::take_unknown_runs`].
    unknown_runs: Vec<UnknownRun>,
    /// Quit arriving right after this many claims of one pass: timing alone
    /// cannot place a cancellation between two claims on every platform.
    #[cfg(test)]
    quit_after_claims: Option<usize>,
}

impl AutomationEngine {
    pub fn new(ledger: AutomationLedger, cancellation: CancellationToken) -> Self {
        Self {
            ledger,
            cancellation,
            intents: BTreeMap::new(),
            active: BTreeMap::new(),
            waiting: BTreeMap::new(),
            settling: BTreeSet::new(),
            unmet: BTreeSet::new(),
            recent: Vec::new(),
            deferrals: BTreeMap::new(),
            awaited: BTreeMap::new(),
            skipped: Vec::new(),
            unknown_runs: Vec::new(),
            #[cfg(test)]
            quit_after_claims: None,
        }
    }

    /// Fires the engine ended without dispatching since the last call, for
    /// the caller to record in each worktree's history.
    pub fn take_skipped(&mut self) -> Vec<SkippedFire> {
        std::mem::take(&mut self.skipped)
    }

    fn note_skipped(&mut self, fire: &AutomationFire, reason: &str) {
        // Bounded even if no caller drains it.
        if self.skipped.len() < 256 {
            self.skipped.push(SkippedFire {
                slug: fire.slug.clone(),
                rule: fire.rule.id.clone(),
                reason: reason.to_owned(),
            });
        }
    }

    /// Fires being delivered now, oldest first.
    pub fn running(&self) -> Vec<RunningFire> {
        let mut running = self
            .active
            .values()
            .map(|active| RunningFire {
                rule: shown_rule(&active.fire),
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

    /// Whether the fire `id` waits for nothing but the end of its settle
    /// window: it is about to be decided, on the evidence of that moment.
    pub fn in_settle_window(&self, id: &str) -> bool {
        self.settling.contains(id)
    }

    /// Whether the fire `id` is still retained, queued or held by an unmet
    /// requirement: one that left the reported queue without this is gone.
    pub fn retains(&self, id: &str) -> bool {
        self.intents.contains_key(id) || self.deferrals.contains_key(id)
    }

    /// The queue as of the latest pass, oldest first, without intents held
    /// only by unmet requirements (docs/automations.md does not count those).
    pub fn queued(&self) -> Vec<QueuedFire> {
        let mut queued = self
            .intents
            .iter()
            .filter(|(id, _)| !self.unmet.contains(*id))
            .map(|(id, intent)| QueuedFire {
                id: id.clone(),
                rule: shown_rule(&intent.fire),
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
    /// this build cannot read is left for a build that can. A fire no pass
    /// produced when it was last looked at comes back without an intent:
    /// its condition has to be seen to hold again before it may run.
    /// Returns how many were requeued.
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
                    failed_runs: record["failedRuns"]
                        .as_u64()
                        .map_or(0, |runs| u32::try_from(runs).unwrap_or(u32::MAX)),
                    dormant: None,
                },
            );
            // A retry keeps the time it was put off to across a restart.
            let retry_at_ms = record["retryAtMs"].as_i64().unwrap_or(0);
            let intent = AutomationIntent {
                fire,
                queued_at_ms,
                settle_at_ms: retry_at_ms.max(now_ms.saturating_add(DEFER_RETRY_MS)),
            };
            if let Some(since_ms) = record["dormantSinceMs"].as_i64() {
                if let Some(deferral) = self.deferrals.get_mut(&id) {
                    deferral.dormant = Some((since_ms, intent));
                }
                continue;
            }
            self.intents.insert(id, intent);
            restored += 1;
        }
        Ok(restored)
    }

    /// Await again the launched runs whose exit no process reported: the
    /// one that launched them quit first. Their run records outlive it, so
    /// the caller reports each exit through [`Self::run_ended`] as usual and
    /// then calls [`Self::end_lost_runs`] for those with no record left.
    pub async fn restore_awaited(&mut self, now_ms: i64) -> Result<usize> {
        let records = self
            .ledger
            .awaited()
            .await
            .context("read awaited automation runs")?;
        let mut restored = 0;
        for (id, record) in records {
            let Some(run) = AwaitedRun::from_record(&record) else {
                tracing::warn!(%id, "skipping an unreadable awaited automation run");
                continue;
            };
            if now_ms.saturating_sub(run.delivered_at_ms) > AWAIT_RUN_MAX_MS
                || self.awaited.contains_key(&id)
            {
                continue;
            }
            self.awaited.insert(id, run);
            restored += 1;
        }
        Ok(restored)
    }

    /// When the earliest restored run still unmatched was claimed: its run
    /// record, and every later one's, started no earlier.
    pub fn restored_since_ms(&self) -> Option<i64> {
        self.awaited
            .values()
            .filter(|run| run.restored)
            .map(|run| run.claim.claimed_at_ms())
            .min()
    }

    /// End the restored runs that have no run record. `running` names the
    /// fire keys and start time of every run still in progress; the ended
    /// ones were already reported. With `retry_on_failure` the fire is
    /// recorded as ended with an unknown outcome and reported once; it is
    /// never run again, because the lost run may have succeeded.
    pub async fn end_lost_runs(
        &mut self,
        running: &[(Vec<String>, i64)],
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let mut lost = Vec::new();
        for (id, run) in &mut self.awaited {
            if !run.restored {
                continue;
            }
            if running.iter().any(|(keys, started_at_ms)| {
                *keys == run.fire.fire_keys && *started_at_ms >= run.claim.claimed_at_ms()
            }) {
                run.restored = false;
            } else {
                lost.push(id.clone());
            }
        }
        for id in lost {
            let Some(run) = self.awaited.remove(&id) else {
                continue;
            };
            self.end_unreported(run, RUN_RECORD_MISSING, now_ms, cancellation)
                .await?;
        }
        Ok(())
    }

    /// Runs ended with an unknown exit since the last call, for the caller
    /// to report once.
    pub fn take_unknown_runs(&mut self) -> Vec<UnknownRun> {
        std::mem::take(&mut self.unknown_runs)
    }

    /// Stop awaiting a run that ended without a definite failure. One whose
    /// action retries and whose exit is unknown is recorded as such.
    async fn end_unreported(
        &mut self,
        run: AwaitedRun,
        unknown: &str,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if !run.retry_on_failure {
            return self
                .ledger
                .clear_awaited(&run.claim, cancellation)
                .await
                .context("stop awaiting automation run");
        }
        let recorded = self
            .ledger
            .mark_unknown(&run.claim, now_ms, unknown, cancellation)
            .await
            .context("mark automation outcome unknown")?;
        if recorded {
            self.unknown_runs.push(UnknownRun {
                slug: run.fire.slug,
                rule: run.fire.rule.id,
                reason: unknown.to_owned(),
            });
        }
        Ok(())
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
        self.awaited
            .retain(|_, run| input.now_ms.saturating_sub(run.delivered_at_ms) <= AWAIT_RUN_MAX_MS);
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
        self.settling.clear();
        self.unmet.clear();
        if input.globally_paused || !input.enabled {
            self.cancel_pending(input).await?;
            return Ok(());
        }

        let resets = evaluate_breaker_resets(input.rules, input.rows, input.context);
        // Nearly every pair has no breaker record. Only an existing one is
        // worth taking the ledger lock for; a record another process adds
        // after this read is reset on the next pass.
        let breakers = if resets.is_empty() {
            None
        } else {
            Some(
                self.ledger
                    .snapshot()
                    .await
                    .context("read automation ledger")?,
            )
        };
        for (rule_id, target) in resets {
            if !breakers
                .as_ref()
                .is_some_and(|ledger| ledger.has_breaker(&rule_id, &target))
            {
                continue;
            }
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
        let mut ledger = self
            .ledger
            .snapshot()
            .await
            .context("read automation ledger")?;
        // A check that was already due when wt began waking sessions for
        // due checks is not prompted for: after an upgrade that would be
        // every long-merged row a person is already handling. Everything
        // due on the pass that records the beginning was due before it.
        // Later, a check is passed over only where its record shows it came
        // due before then; where that cannot be told its session is woken,
        // since a second prompt costs less than a missed one.
        let began = ledger.check_wakes_from();
        let before = current
            .iter()
            .filter(|fire| wt_automations::is_check_wake(fire))
            .filter(|fire| !fire.fire_keys.iter().all(|key| ledger.recorded(key)))
            .filter(|fire| match began {
                None => true,
                Some(began) => fire
                    .check
                    .as_ref()
                    .and_then(|check| check.due_since_ms)
                    .is_some_and(|since| since < began),
            })
            .map(|fire| (fire.fire_keys.clone(), fire.slug.clone()))
            .collect::<Vec<_>>();
        if began.is_none() || !before.is_empty() {
            self.ledger
                .seed_check_wakes(
                    &before,
                    wt_automations::CHECK_RULE_ID,
                    input.now_ms,
                    &self.cancellation,
                )
                .await
                .context("record the checks already due")?;
            ledger = self
                .ledger
                .snapshot()
                .await
                .context("read automation ledger")?;
        }
        // One prompt per check for a session, whichever rule sends it: wt's
        // own wake, or a configured rule that was added or removed since.
        let mut prompted = Vec::new();
        let mut repeats = Vec::new();
        for fire in &current {
            let Some(prompt) = fire
                .check
                .as_ref()
                .and_then(|check| check.prompt_key.as_ref())
            else {
                continue;
            };
            if fire.fire_keys.iter().all(|key| ledger.sent(key)) {
                if !ledger.recorded(prompt) {
                    prompted.push((prompt.clone(), fire.slug.clone()));
                }
            } else if ledger.recorded(prompt)
                && !fire.fire_keys.iter().any(|key| ledger.recorded(key))
            {
                repeats.push((
                    fire.fire_keys.clone(),
                    fire.rule.id.clone(),
                    fire.slug.clone(),
                ));
            }
        }
        if !prompted.is_empty() || !repeats.is_empty() {
            self.ledger
                .record_check_prompts(&prompted, &repeats, input.now_ms, &self.cancellation)
                .await
                .context("record prompted checks")?;
            ledger = self
                .ledger
                .snapshot()
                .await
                .context("read automation ledger")?;
        }
        self.recent = ledger.recent(RECENT_HISTORY);
        let handled = QueueIntentsInput::handled_keys(
            &candidates,
            |key| ledger.has_handled(key, input.now_ms),
            |key| ledger.recorded_within(key, input.now_ms, LOCAL_SIGHTING_WINDOW_MS),
        );
        let executing = self.active.keys().cloned().collect::<BTreeSet<_>>();
        for fire in &current {
            if let Some(watch) = &fire.watch
                && !executing.contains(&fire_identity(fire))
                && fire.fire_keys.iter().all(|key| ledger.settled(key))
            {
                report.settled_watches.push(SettledWatch {
                    id: watch.id.clone(),
                    action: watch.action,
                    // The landing the fire was claimed under, not this
                    // pass's: a wake delivered before the work landed
                    // closed on delivery, and its prompt said nothing of
                    // closing it.
                    landing: fire
                        .fire_keys
                        .iter()
                        .find_map(|key| ledger.owner_landing(key))
                        .unwrap_or(wt_core::OwnerLanding::NotLanded),
                    unconfirmed: !fire.fire_keys.iter().all(|key| ledger.delivered(key)),
                });
            }
        }
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
        // A pass that does not produce a deferred or retried fire says only
        // that its condition was not confirmed just now. Its backoff and
        // failed-run count wait for it, without an intent, and it resumes
        // where it was when the same occurrence returns. One that stays
        // away is forgotten, as is one that returns as a new occurrence.
        let mut forgotten = Vec::new();
        // Kept in the ledger too: see `restore_deferred`.
        let mut dormancy = Vec::new();
        for (id, deferral) in &mut self.deferrals {
            if self.active.contains_key(id) {
                continue;
            }
            match self.intents.get_mut(id) {
                Some(intent) => {
                    let Some((_, was)) = deferral.dormant.take() else {
                        continue;
                    };
                    dormancy.push((id.clone(), None));
                    let same_occurrence = was
                        .fire
                        .fire_keys
                        .iter()
                        .any(|key| intent.fire.fire_keys.contains(key));
                    if same_occurrence {
                        intent.queued_at_ms = was.queued_at_ms;
                        intent.settle_at_ms = intent.settle_at_ms.max(was.settle_at_ms);
                    } else {
                        forgotten.push((id.clone(), "its condition returned as a new occurrence"));
                    }
                }
                None => {
                    if deferral.dormant.is_none() {
                        deferral.dormant = existing
                            .iter()
                            .find(|intent| fire_identity(&intent.fire) == *id)
                            .map(|intent| (input.now_ms, intent.clone()));
                        if deferral.dormant.is_some() {
                            dormancy.push((id.clone(), Some(input.now_ms)));
                        }
                    }
                    match &deferral.dormant {
                        None => forgotten.push((id.clone(), "nothing produces it")),
                        // A watch comes from stored state, not from a read
                        // that can lapse: one that is not there was
                        // withdrawn or is held, and starts over.
                        Some((_, was)) if was.fire.watch.is_some() => {
                            forgotten.push((id.clone(), "its watch was withdrawn or is held"));
                        }
                        // A fire that needs no checkout outlives its
                        // worktree while queued, and so does its backoff.
                        Some((_, was))
                            if was.fire.slug != wt_automations::FLEET_SLUG
                                && !wt_automations::is_frozen_external_fire(&was.fire)
                                && !input.rows.iter().any(|row| row.slug == was.fire.slug) =>
                        {
                            forgotten.push((id.clone(), "its worktree is gone"));
                        }
                        Some((since_ms, _))
                            if input.now_ms.saturating_sub(*since_ms) > DORMANT_MAX_MS =>
                        {
                            forgotten.push((id.clone(), "its condition has not held for an hour"));
                        }
                        Some(_) => {}
                    }
                }
            }
        }
        for (id, since_ms) in dormancy {
            if forgotten.iter().any(|(gone, _)| *gone == id) {
                continue;
            }
            self.ledger
                .set_deferred_dormant(&id, since_ms, &self.cancellation)
                .await
                .context("record whether a deferred fire is still produced")?;
        }
        for (id, why) in forgotten {
            self.ledger
                .clear_deferred(&id, &self.cancellation)
                .await
                .context("clear a withdrawn fire's deferral")?;
            if let Some(deferral) = self.deferrals.remove(&id) {
                tracing::info!(
                    fire = %id,
                    failed_runs = deferral.failed_runs,
                    reason = %deferral.reason,
                    why,
                    "automation backoff forgotten"
                );
            }
        }

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
            let fire = intent.fire;
            let id = fire_identity(&fire);
            let requirements = input.requirements.get(&id);
            // Decided before any capacity or exclusion wait, so an intent
            // that cannot run anyway is never reported as queued behind one.
            if matches!(requirements, Some(RequirementState::Waiting)) {
                // Preserve the original settle age while a row-backed
                // prerequisite changes. The trigger still has to remain true.
                self.unmet.insert(id);
                continue;
            }
            let rule = &fire.rule;
            let manager_run = manager_bound(&fire, input.context);
            let class = cost_class(&fire, manager_run, input.context);
            let running = self
                .active
                .values()
                .filter(|active| active.class == class)
                .count();
            if running >= class.cap() {
                self.waiting.insert(id, slots_taken(class));
                continue;
            }
            let audience = input
                .context
                .actions
                .get(&rule.run)
                .map_or(wt_automations::ActionAudience::None, |traits| {
                    traits.audience
                });
            let breaker_exempt = breaker_exempt(&fire, manager_run);
            // A retry continues a dispatch the breaker already counted, so
            // three retries are not cut to one by a limit of two.
            let uncounted = breaker_exempt
                || self
                    .deferrals
                    .get(&id)
                    .is_some_and(|deferral| deferral.failed_runs > 0);
            let retry_on_failure = input
                .context
                .actions
                .get(&rule.run)
                .is_some_and(|traits| traits.shell && traits.retry_on_failure);

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
                self.note_skipped(&fire, "its requirements are no longer met");
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
            if !uncounted && breaker.tripped_at.is_some() {
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
                self.note_skipped(&fire, "it fired too often for this worktree");
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
                    None => {
                        let exempt = breaker_exempt && !is_wake(&fire);
                        if exempt || quiesce_block_reason(&fire, &input).is_none() {
                            self.settling.insert(id.clone());
                        }
                        "its settle delay".into()
                    }
                };
                self.waiting.insert(id, reason);
                continue;
            }

            // A wake is a prompt into a session and waits for it like one.
            let quiesce_exempt = breaker_exempt && !is_wake(&fire);
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
                    self.note_skipped(&fire, &reason);
                    self.intents.remove(&id);
                    tracing::info!(rule = %rule.id, %reason, "automation skipped while target busy");
                } else {
                    self.waiting.insert(id, reason);
                }
                continue;
            }

            let policy = DispatchPolicy {
                cooldown_ms,
                breaker_limit: (!uncounted).then_some(wt_automations::BREAKER_LIMIT),
                owner_landing: fire
                    .watch
                    .as_ref()
                    .filter(|watch| watch.action == wt_core::watch::WatchAction::Wake)
                    .map(|watch| watch.landing),
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
                    self.note_skipped(&fire, "it fired too often for this worktree");
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
                            class,
                            queued_at_ms: intent.queued_at_ms,
                            settle_at_ms: intent.settle_at_ms,
                            retry_on_failure,
                        },
                    );
                    report.claims.push(ClaimedFire { id, fire, claim });
                    #[cfg(test)]
                    if self.quit_after_claims == Some(report.claims.len()) {
                        self.cancellation.cancel();
                        anyhow::bail!("evaluation cancelled between claims");
                    }
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
        let paused = input.globally_paused || !input.enabled;
        // While paused the cancellable set also holds what the rules produce
        // now; a key the ledger already has is not cancelled over.
        let evaluated = if paused {
            evaluate(input.rules, input.rows, input.context)
        } else {
            Vec::new()
        };
        let keys = pending
            .iter()
            .chain(&evaluated)
            .flat_map(|fire| fire.fire_keys.iter())
            .cloned()
            .collect::<BTreeSet<_>>();
        let handled = QueueIntentsInput::handled_keys(
            &keys,
            |key| snapshot.has_handled(key, input.now_ms),
            |key| snapshot.recorded_within(key, input.now_ms, LOCAL_SIGHTING_WINDOW_MS),
        );
        let fires = cancellable_fires(CancellableFiresInput {
            paused,
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
                let (reported, failed_runs) = self
                    .deferrals
                    .get(&claim.id)
                    .map_or((false, 0), |deferral| {
                        (deferral.reported, deferral.failed_runs)
                    });
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
                        failed_runs,
                        dormant: None,
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
                let failed_runs = self
                    .deferrals
                    .get(&claim.id)
                    .map_or(0, |deferral| deferral.failed_runs);
                let awaited = (slot == DeliverySlot::Headless
                    && ledger_claim.kind() == DispatchKind::HeadlessAction)
                    .then(|| self.active.get(&claim.id))
                    .flatten()
                    .map(|active| AwaitedRun {
                        claim: ledger_claim.clone(),
                        fire: active.fire.clone(),
                        queued_at_ms: active.queued_at_ms,
                        delivered_at_ms: now_ms,
                        failed_runs,
                        retry_on_failure: active.retry_on_failure,
                        restored: false,
                    });
                match &awaited {
                    // Kept in the delivery's own write: a process that quits
                    // before the run ends leaves its exit to the next one.
                    Some(run) => {
                        self.ledger
                            .mark_delivered_awaited(
                                &ledger_claim,
                                now_ms,
                                &claim.id,
                                run.record()?,
                                cancellation,
                            )
                            .await
                    }
                    None => {
                        self.ledger
                            .mark_delivered(&ledger_claim, now_ms, cancellation)
                            .await
                    }
                }
                .context("mark automation delivered")?;
                self.forget_deferral(&claim.id, cancellation).await?;
                if let Some(run) = awaited {
                    self.awaited.insert(claim.id.clone(), run);
                }
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

    /// Whether any delivered shell run's exit is still unreported.
    pub fn awaits_runs(&self) -> bool {
        !self.awaited.is_empty()
    }

    /// Report how a launched run ended. `keys` and `started_at_ms` come
    /// from the run's own record and pick the delivery it belongs to: a
    /// retry reuses its fire's keys, so an earlier attempt's record, which
    /// started before this claim, matches nothing. A shell command that
    /// exited non-zero ends its fire as failed. When its action sets
    /// `retry_on_failure` the fire first goes back to the queue on a
    /// backoff, a bounded number of times. Every other end leaves the fire
    /// delivered.
    pub async fn run_ended(
        &mut self,
        keys: &[String],
        started_at_ms: i64,
        end: RunEnd,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<Option<FailedRun>> {
        let Some(id) = self
            .awaited
            .iter()
            .find(|(_, run)| {
                run.fire.fire_keys == keys && started_at_ms >= run.claim.claimed_at_ms()
            })
            .map(|(id, _)| id.clone())
        else {
            return Ok(None);
        };
        let Some(run) = self.awaited.remove(&id) else {
            return Ok(None);
        };
        let exit_code = match end {
            RunEnd::ShellFailed { exit_code } => exit_code,
            RunEnd::ShellUnknown | RunEnd::Other => {
                let unknown = if end == RunEnd::ShellUnknown {
                    RUN_EXIT_UNRECORDED
                } else {
                    ""
                };
                let mut run = run;
                // Any other end is an answer: the fire stays delivered.
                run.retry_on_failure &= end == RunEnd::ShellUnknown;
                self.end_unreported(run, unknown, now_ms, cancellation)
                    .await?;
                return Ok(None);
            }
        };
        let runs = run.failed_runs.saturating_add(1);
        let failed = |retry_in_ms| FailedRun {
            slug: run.fire.slug.clone(),
            rule: run.fire.rule.id.clone(),
            exit_code,
            runs,
            retried: run.retry_on_failure,
            retry_in_ms,
        };
        let delay = run
            .retry_on_failure
            .then(|| RUN_RETRY_DELAYS_MS.get(run.failed_runs as usize).copied())
            .flatten();
        let Some(delay) = delay else {
            let ended = self
                .ledger
                .mark_failed(
                    &run.claim,
                    now_ms,
                    &failed_reason(exit_code, runs),
                    runs,
                    cancellation,
                )
                .await
                .context("mark automation failed")?;
            // Another process read the same exit first and reported it.
            return Ok(ended.then(|| failed(None)));
        };
        let reason = format!(
            "retry {runs} of {} after exit {exit_code}",
            RUN_RETRY_DELAYS_MS.len()
        );
        let record = serde_json::json!({
            "fire": serde_json::to_value(&run.fire).context("encode failed fire")?,
            "queuedAtMs": run.queued_at_ms,
            "reason": reason,
            "frozen": {},
            "failedRuns": runs,
            "retryAtMs": now_ms.saturating_add(delay),
        });
        let released = self
            .ledger
            .release_failed(&run.claim, &id, record, cancellation)
            .await
            .context("requeue failed automation run")?;
        if !released {
            // Another writer ended the key (cancelled, or pruned).
            return Ok(None);
        }
        let report = failed(Some(delay));
        // The run has ended, so its slot holds nothing.
        if self
            .active
            .get(&id)
            .is_some_and(|active| active.slot != SlotState::Dispatching)
        {
            self.active.remove(&id);
        }
        self.deferrals.insert(
            id.clone(),
            Deferral {
                reason,
                reported: false,
                frozen: BTreeMap::new(),
                failed_runs: runs,
                dormant: None,
            },
        );
        self.intents.insert(
            id,
            AutomationIntent {
                fire: run.fire,
                queued_at_ms: run.queued_at_ms,
                settle_at_ms: now_ms.saturating_add(delay),
            },
        );
        Ok(Some(report))
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

/// Each fire in the ledger at `path` whose run's exit could not be
/// recovered, newest first, as `(slug, what to say of it)`: `wt doctor`
/// lists them until the ledger forgets the fire.
pub async fn unknown_outcomes(path: &std::path::Path) -> Result<Vec<(String, String)>> {
    let snapshot = AutomationLedger::new(path)
        .snapshot()
        .await
        .context("read automation ledger")?;
    Ok(snapshot
        .unknown_outcomes()
        .into_iter()
        .map(|record| {
            let reason = record.reason.unwrap_or_default();
            (
                record.slug,
                format!(
                    "{}: outcome unknown, not run again: {reason}",
                    record.rule_id
                ),
            )
        })
        .collect())
}

/// A prompt into the worktree's own session that no rule asked for: a
/// watch's wake, or wt's wake for a due check.
fn is_wake(fire: &AutomationFire) -> bool {
    fire.watch
        .as_ref()
        .is_some_and(|watch| watch.action == wt_core::watch::WatchAction::Wake)
        || wt_automations::is_check_wake(fire)
}

/// The rule name a person reads: every watch fire runs under one implicit
/// rule whose id says nothing.
fn shown_rule(fire: &AutomationFire) -> String {
    if fire.watch.is_some() {
        "watch".into()
    } else if wt_automations::is_check_wake(fire) {
        "check".into()
    } else {
        fire.rule.id.clone()
    }
}

/// What a fire costs while it runs. Anything not known to be cheap is heavy.
fn cost_class(
    fire: &AutomationFire,
    manager_run: bool,
    context: &wt_automations::AutomationEvalContext,
) -> CostClass {
    if is_wake(fire) {
        return CostClass::Heavy;
    }
    // A watch's other actions write a gate or send a notification.
    if fire.watch.is_some() {
        return CostClass::Light;
    }
    let run = fire.rule.run.as_str();
    if run == "builtin:restack" || run == "builtin:clean" {
        return CostClass::Heavy;
    }
    // A manager prompt holds its slot only until it is injected, and one
    // is sent at a time.
    if run.starts_with("builtin:") || manager_run {
        return CostClass::Light;
    }
    match context.actions.get(run) {
        Some(traits) if traits.shell => CostClass::Light,
        _ => CostClass::Heavy,
    }
}

/// Why a fire waits when every slot of its class is taken.
fn slots_taken(class: CostClass) -> String {
    format!(
        "{} {} automations already running",
        class.cap(),
        class.noun()
    )
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

/// How a fire that ended failed is described in the ledger and on the feed.
pub fn failed_reason(exit_code: i32, runs: u32) -> String {
    if runs > 1 {
        format!("exit {exit_code} after {runs} runs")
    } else {
        format!("exit {exit_code}")
    }
}

fn breaker_exempt(fire: &AutomationFire, manager_run: bool) -> bool {
    fire.rule.on == wt_config::AutomationTrigger::BranchAdvanced
        || fire.rule.on == wt_config::AutomationTrigger::StageDeployed
        // Each event is a distinct occurrence, and the server leaves out the
        // agents' own writes, so a reply cannot wake its sender in a loop.
        || fire.rule.on == wt_config::AutomationTrigger::McpEvent
        // A watch is met once and each of its actions is delivered once.
        || fire.watch.is_some()
        // So is a due check: its key is the obligation.
        || wt_automations::is_check_wake(fire)
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

    /// A fire dropped for good is reported once, for its worktree's history.
    #[tokio::test]
    async fn a_fire_skipped_while_its_target_is_busy_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger, CancellationToken::new());
        let rules = [AutomationDef {
            busy: AutomationBusyPolicy::Skip,
            ..rule("custom-action", 0.0)
        }];
        let rows = [row()];
        let context = AutomationEvalContext::default();
        let activity = RuntimeActivity::default();
        let requirements = BTreeMap::from([("test|worktree".into(), RequirementState::Ready)]);
        let busy = BTreeMap::from([(
            "worktree".to_owned(),
            WorktreeQuiescence {
                session_busy: true,
                ..WorktreeQuiescence::default()
            },
        )]);
        let start = now_ms();
        for step in 0..2 {
            let tick = engine
                .tick(input(
                    &rules,
                    &rows,
                    &context,
                    &requirements,
                    &busy,
                    &activity,
                    start + step,
                ))
                .await
                .unwrap();
            assert!(tick.claims.is_empty());
        }
        assert!(engine.queued().is_empty(), "a skipped fire does not wait");
        assert_eq!(
            engine.take_skipped(),
            [SkippedFire {
                slug: "worktree".into(),
                rule: "test".into(),
                reason: "session busy on worktree".into(),
            }]
        );
        assert!(engine.take_skipped().is_empty());
    }

    #[tokio::test]
    async fn unmet_requirement_is_not_queued_behind_running_automations() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger, CancellationToken::new());
        let rules = [rule("custom-action", 0.0)];
        let rows = ["a", "b", "c", "d"].map(|slug| AutomationRow {
            slug: slug.into(),
            branch: slug.into(),
            ..row()
        });
        let context = AutomationEvalContext::default();
        let quiescence = BTreeMap::new();
        let activity = RuntimeActivity::default();
        let requirements = BTreeMap::from([
            ("test|a".into(), RequirementState::Ready),
            ("test|b".into(), RequirementState::Ready),
            ("test|c".into(), RequirementState::Waiting),
            ("test|d".into(), RequirementState::Ready),
        ]);
        let start = now_ms();
        for step in 0..2 {
            let tick = engine
                .tick(input(
                    &rules,
                    &rows,
                    &context,
                    &requirements,
                    &quiescence,
                    &activity,
                    start + step,
                ))
                .await
                .unwrap();
            assert_eq!(tick.claims.len(), if step == 0 { MAX_HEAVY } else { 0 });
            // Only the runnable fire waits for a slot; the requirement-held
            // one stays out of the queue while both slots are taken.
            let queued = engine
                .queued()
                .into_iter()
                .map(|fire| (fire.target, fire.waiting_for))
                .collect::<Vec<_>>();
            assert_eq!(queued, [("d".to_owned(), slots_taken(CostClass::Heavy))]);
        }
    }

    /// A tracker update or notification ends in seconds; it must not wait
    /// minutes for an agent's turn to end, and agents keep their own cap.
    #[tokio::test]
    async fn cheap_fires_run_beside_agent_fires_under_their_own_cap() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger, CancellationToken::new());
        let agent = AutomationDef {
            id: "agent".into(),
            ..rule("fix-it", 0.0)
        };
        let tracker = AutomationDef {
            id: "track".into(),
            ..rule("tracker", 0.0)
        };
        let slugs = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let rows = slugs.map(|slug| AutomationRow {
            slug: slug.into(),
            branch: slug.into(),
            ..row()
        });
        let mut context = AutomationEvalContext::default();
        context.actions.insert(
            "tracker".into(),
            wt_automations::ActionTraits {
                shell: true,
                ..Default::default()
            },
        );
        let requirements = slugs
            .iter()
            .flat_map(|slug| {
                ["agent", "track"].map(|rule| (format!("{rule}|{slug}"), RequirementState::Ready))
            })
            .collect::<BTreeMap<_, _>>();
        let quiescence = BTreeMap::new();
        let activity = RuntimeActivity::default();
        let start = now_ms();

        let agents = [agent.clone()];
        let first = engine
            .tick(input(
                &agents,
                &rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                start,
            ))
            .await
            .unwrap();
        assert_eq!(first.claims.len(), MAX_HEAVY);

        let both = [agent, tracker];
        let second = engine
            .tick(input(
                &both,
                &rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                start + 1,
            ))
            .await
            .unwrap();
        assert_eq!(
            second
                .claims
                .iter()
                .map(|claim| claim.fire().rule.id.as_str())
                .collect::<Vec<_>>(),
            ["track"; MAX_LIGHT],
            "cheap fires dispatch while both agent slots are taken, up to their own cap"
        );
        let waits = |engine: &AutomationEngine, rule: &str, reason: String| {
            engine
                .queued()
                .into_iter()
                .filter(|fire| fire.rule == rule && fire.waiting_for == reason)
                .count()
        };
        assert_eq!(
            waits(&engine, "agent", slots_taken(CostClass::Heavy)),
            slugs.len() - MAX_HEAVY
        );
        assert!(waits(&engine, "track", slots_taken(CostClass::Light)) > 0);

        // A finished cheap fire frees a cheap slot, never an agent slot.
        engine
            .finish(
                &second.claims[0],
                AutomationOutcome::Delivered {
                    slot: DeliverySlot::None,
                },
                start + 2,
            )
            .await
            .unwrap();
        let third = engine
            .tick(input(
                &both,
                &rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                start + 3,
            ))
            .await
            .unwrap();
        assert_eq!(
            third
                .claims
                .iter()
                .map(|claim| claim.fire().rule.id.as_str())
                .collect::<Vec<_>>(),
            ["track"]
        );
    }

    #[test]
    fn only_fires_known_to_be_cheap_take_a_cheap_slot() {
        let mut context = AutomationEvalContext::default();
        let traits = |shell, audience| wt_automations::ActionTraits {
            shell,
            audience,
            ..Default::default()
        };
        use wt_automations::ActionAudience::{Manager, None as Headless, Session};
        context
            .actions
            .insert("script".into(), traits(true, Headless));
        context
            .actions
            .insert("agent".into(), traits(false, Headless));
        context
            .actions
            .insert("prompt".into(), traits(false, Session));
        context
            .actions
            .insert("brief".into(), traits(false, Manager));
        let class = |run: &str| {
            let fire: AutomationFire = serde_json::from_value(serde_json::json!({
                "rule": rule(run, 0.0), "slug": "worktree", "detail": "",
                "fire_keys": ["k"], "quiesce_slugs": [],
            }))
            .unwrap();
            cost_class(&fire, manager_bound(&fire, &context), &context)
        };
        for run in [
            "script",
            "brief",
            "builtin:notify",
            "builtin:close-issue",
            "builtin:delete-branch",
        ] {
            assert_eq!(class(run), CostClass::Light, "{run}");
        }
        for run in [
            "agent",
            "prompt",
            "not-configured",
            "builtin:restack",
            "builtin:clean",
        ] {
            assert_eq!(class(run), CostClass::Heavy, "{run}");
        }
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

    /// A landing first seen on trunk alone and then through its merged PR is
    /// one landing: its rule ran once, and an issue a human reopened in
    /// between must not be closed a second time.
    #[tokio::test]
    async fn landing_handled_without_its_pr_does_not_run_again_once_the_pr_is_seen() {
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
        let local = [AutomationRow {
            local_merged: Some(true),
            clean_candidate: true,
            ..row()
        }];
        let merged_pr = |number| {
            [AutomationRow {
                pr: Some(wt_automations::AutomationPr {
                    number,
                    state: "MERGED".into(),
                    ..Default::default()
                }),
                ..local[0].clone()
            }]
        };
        let context = AutomationEvalContext {
            github_fresh: true,
            ..AutomationEvalContext::default()
        };
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
        let delivered = AutomationOutcome::Delivered {
            slot: DeliverySlot::None,
        };

        let first = engine.tick(pass(&local, start)).await.unwrap();
        let claim = first.claims.first().unwrap().clone();
        assert!(
            claim
                .fire()
                .fire_keys
                .contains(&"delete-branch-on-merge:merged:worktree:local".to_owned()),
            "{:?}",
            claim.fire().fire_keys
        );
        engine
            .finish(&claim, delivered.clone(), start + 1)
            .await
            .unwrap();

        let with_pr = merged_pr(7);
        let second = engine.tick(pass(&with_pr, start + 2)).await.unwrap();
        assert!(second.claims.is_empty(), "the same landing ran twice");
        assert_eq!(engine.pending_count(), 0);
        assert!(
            !ledger
                .snapshot()
                .await
                .unwrap()
                .has_handled("delete-branch-on-merge:merged:worktree:7", start + 2)
        );
        // Pausing does not record the PR sighting as a cancelled fire either.
        let mut paused = pass(&with_pr, start + 3);
        paused.globally_paused = true;
        assert_eq!(engine.cancel_pending(paused).await.unwrap(), 0);
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

        engine.quit_after_claims = Some(1);
        let result = engine.tick(pass(start)).await;
        assert!(evaluation.is_cancelled());
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

    use wt_automations::MetWatch;
    use wt_core::watch::{WatchAction, action_key};

    fn met(id: &str, actions: &[(WatchAction, &str)]) -> MetWatch {
        MetWatch {
            id: id.into(),
            slug: "worktree".into(),
            by: "worktree".into(),
            held: "17:00 today".into(),
            actions: actions
                .iter()
                .map(|(action, text)| (*action, (*text).to_owned()))
                .collect(),
            cleared_gate: None,
        }
    }

    fn watching(watches: Vec<MetWatch>) -> AutomationEvalContext {
        AutomationEvalContext {
            watches,
            ..AutomationEvalContext::default()
        }
    }

    fn busy_session() -> BTreeMap<String, WorktreeQuiescence> {
        BTreeMap::from([(
            "worktree".to_owned(),
            WorktreeQuiescence {
                session_busy: true,
                ..WorktreeQuiescence::default()
            },
        )])
    }

    const DONE: AutomationOutcome = AutomationOutcome::Delivered {
        slot: DeliverySlot::None,
    };

    /// Watches need no configured rule, and several on one worktree are
    /// separate fires: none replaces another in the queue.
    #[tokio::test]
    async fn three_met_watches_on_one_worktree_each_deliver() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let rows = [row()];
        let context = watching(
            ["aaaaaa", "bbbbbb", "cccccc"]
                .map(|id| met(id, &[(WatchAction::Notify, "")]))
                .to_vec(),
        );
        let (requirements, quiescence, activity) =
            (BTreeMap::new(), BTreeMap::new(), RuntimeActivity::default());
        let start = now_ms();
        let mut delivered = Vec::new();
        for step in 0..3 {
            let tick = engine
                .tick(input(
                    &[],
                    &rows,
                    &context,
                    &requirements,
                    &quiescence,
                    &activity,
                    start + step,
                ))
                .await
                .unwrap();
            for claim in &tick.claims {
                assert_eq!(claim.fire().rule.run, "builtin:watch");
                delivered.push(claim.fire().watch.clone().unwrap().id);
                engine.finish(claim, DONE, start + step).await.unwrap();
            }
        }
        delivered.sort();
        assert_eq!(delivered, ["aaaaaa", "bbbbbb", "cccccc"]);
        let snapshot = ledger.snapshot().await.unwrap();
        for id in &delivered {
            assert!(snapshot.has_handled(&action_key(id, WatchAction::Notify), start + 3));
        }
        assert!(engine.queued().is_empty());
    }

    /// A pause and a cleared queue hold watch fires; neither writes their
    /// keys, so the watch still delivers afterwards.
    #[tokio::test]
    async fn pausing_or_clearing_the_queue_consumes_no_watch_fire() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let rows = [row()];
        let context = watching(vec![met(
            "aaaaaa",
            &[(WatchAction::Notify, ""), (WatchAction::Wake, "go")],
        )]);
        let (requirements, activity) = (BTreeMap::new(), RuntimeActivity::default());
        let busy = busy_session();
        let start = now_ms();
        let keys =
            [WatchAction::Notify, WatchAction::Wake].map(|action| action_key("aaaaaa", action));

        let mut paused = input(&[], &rows, &context, &requirements, &busy, &activity, start);
        paused.globally_paused = true;
        let tick = engine.tick(paused).await.unwrap();
        assert!(tick.claims.is_empty());
        let snapshot = ledger.snapshot().await.unwrap();
        assert!(keys.iter().all(|key| !snapshot.has_handled(key, start)));

        // Resumed: the notification goes out although the session is busy,
        // and the wake waits for it.
        let tick = engine
            .tick(input(
                &[],
                &rows,
                &context,
                &requirements,
                &busy,
                &activity,
                start + 1,
            ))
            .await
            .unwrap();
        assert_eq!(tick.claims.len(), 1);
        assert_eq!(tick.claims[0].fire().fire_keys, [keys[0].clone()]);
        engine
            .finish(&tick.claims[0], DONE, start + 1)
            .await
            .unwrap();
        let queued = engine.queued();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            (
                queued[0].rule.as_str(),
                queued[0].detail.as_str(),
                queued[0].waiting_for.as_str()
            ),
            ("watch", "wake: 17:00 today", "session busy on worktree")
        );

        let cleared = engine
            .cancel_pending(input(
                &[],
                &rows,
                &context,
                &requirements,
                &busy,
                &activity,
                start + 2,
            ))
            .await
            .unwrap();
        assert_eq!(cleared, 0);
        assert!(
            !ledger
                .snapshot()
                .await
                .unwrap()
                .has_handled(&keys[1], start + 2)
        );

        let idle = BTreeMap::new();
        let tick = engine
            .tick(input(
                &[],
                &rows,
                &context,
                &requirements,
                &idle,
                &activity,
                start + 3,
            ))
            .await
            .unwrap();
        assert_eq!(tick.claims.len(), 1);
        assert_eq!(tick.claims[0].fire().fire_keys, [keys[1].clone()]);
        assert_eq!(tick.claims[0].fire().quiesce_slugs, ["worktree"]);
    }

    /// A build that wakes sessions for due checks first finds every check
    /// that was already due, on rows a person is handling. Those are not
    /// prompted for. One that comes due afterwards is, and so is one whose
    /// record cannot say when it came due.
    #[tokio::test]
    async fn a_check_already_due_when_wt_first_looks_wakes_nobody() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let start = now_ms();
        let at = |ms: i64| {
            time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
                .unwrap()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        };
        // Each owes a staging check asserted an hour ago. `merged` is when
        // its pull request merged; none means Git alone shows the landing.
        let owing = |slug: &str, landed: bool, merged: Option<i64>| {
            let mut owing = row();
            owing.slug = slug.into();
            owing.local_merged = landed.then_some(true);
            owing.pr = merged.map(|merged_at_ms| wt_automations::AutomationPr {
                number: 7,
                state: "MERGED".into(),
                merged_at_ms: Some(merged_at_ms),
                ..Default::default()
            });
            let mut work =
                wt_core::WorkStatusRecord::new(wt_core::WorkState::Ready, at(start - 3_600_000));
            work.set_check(wt_core::VerifyStage::Staging, "1. open the page");
            owing.work = Some(work);
            owing
        };
        let context = AutomationEvalContext {
            github_fresh: true,
            ..AutomationEvalContext::default()
        };
        let (requirements, idle, activity) =
            (BTreeMap::new(), BTreeMap::new(), RuntimeActivity::default());
        let mut pass = async |rows: &[AutomationRow], at| {
            let mut woken = engine
                .tick(input(
                    &[],
                    rows,
                    &context,
                    &requirements,
                    &idle,
                    &activity,
                    at,
                ))
                .await
                .unwrap()
                .claims
                .iter()
                .map(|claim| claim.fire().slug.clone())
                .collect::<Vec<_>>();
            woken.sort();
            woken
        };
        // The first pass: "old" is due and was before wt began.
        let first = [
            owing("old", true, None),
            owing("late", false, None),
            owing("new", false, None),
            owing("plain", false, None),
        ];
        assert!(pass(&first, start).await.is_empty());
        // Next pass, seconds later, GitHub's answer has arrived. "late"
        // merged before wt began: passed over, with no window to wait out.
        // "new" merged since: woken. "plain" landed with no pull request to
        // date it: woken, since a second prompt beats a missed one.
        let second = [
            owing("old", true, None),
            owing("late", false, Some(start - 60_000)),
            owing("new", false, Some(start + 1_000)),
            owing("plain", true, None),
        ];
        assert_eq!(pass(&second, start + 2_000).await, ["new", "plain"]);
        assert_eq!(
            ledger.snapshot().await.unwrap().check_wakes_from(),
            Some(start)
        );
    }

    /// A session is prompted once for a check, whichever rule sends it:
    /// adding a rule for due checks does not prompt again for one wt's own
    /// wake delivered, and removing the rule does not either.
    #[tokio::test]
    async fn a_rule_added_later_does_not_prompt_again_for_a_delivered_check() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let start = now_ms();
        ledger
            .seed_check_wakes(
                &[],
                wt_automations::CHECK_RULE_ID,
                start - 1,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let mut owing = row();
        owing.local_merged = Some(true);
        let mut work = wt_core::WorkStatusRecord::new(wt_core::WorkState::Ready, "t");
        work.set_check(wt_core::VerifyStage::Staging, "1. open the page");
        owing.work = Some(work);
        let rows = [owing];
        let rule = AutomationDef {
            id: "verify".into(),
            on: wt_config::AutomationTrigger::StatusVerificationDue,
            run: "verify-deployed".into(),
            settle_seconds: 0.0,
            ..AutomationDef::default()
        };
        let mut context = AutomationEvalContext::default();
        context.actions.insert(
            "verify-deployed".into(),
            wt_automations::ActionTraits {
                shell: false,
                external: false,
                audience: wt_automations::ActionAudience::Session,
                ..Default::default()
            },
        );
        let mut changed = rows[0].clone();
        changed
            .work
            .as_mut()
            .unwrap()
            .set_check(wt_core::VerifyStage::Staging, "1. open the other page");
        let changed = [changed];
        let identity = evaluate(std::slice::from_ref(&rule), &changed, &context)
            .iter()
            .find(|fire| fire.rule.id == "verify")
            .map(fire_identity)
            .unwrap();
        let requirements = BTreeMap::from([(identity, RequirementState::Ready)]);
        let (idle, activity) = (BTreeMap::new(), RuntimeActivity::default());
        // wt's own wake delivers the check.
        let tick = engine
            .tick(input(
                &[],
                &rows,
                &context,
                &requirements,
                &idle,
                &activity,
                start,
            ))
            .await
            .unwrap();
        assert_eq!(tick.claims.len(), 1);
        assert!(wt_automations::is_check_wake(tick.claims[0].fire()));
        engine
            .finish(&tick.claims[0], DONE, start + 1)
            .await
            .unwrap();
        let noted = engine
            .tick(input(
                &[],
                &rows,
                &context,
                &requirements,
                &idle,
                &activity,
                start + 2,
            ))
            .await
            .unwrap();
        assert!(noted.claims.is_empty());
        // The rule is added: it owns delivery from here, and has nothing
        // left to deliver for this check.
        let rules = [rule];
        for at in [start + 3, start + 4] {
            let tick = engine
                .tick(input(
                    &rules,
                    &rows,
                    &context,
                    &requirements,
                    &idle,
                    &activity,
                    at,
                ))
                .await
                .unwrap();
            assert!(tick.claims.is_empty(), "{:?}", tick.claims);
        }
        assert!(engine.queued().is_empty());
        // New steps are a new check: the rule delivers that one.
        let tick = engine
            .tick(input(
                &rules,
                &changed,
                &context,
                &requirements,
                &idle,
                &activity,
                start + 5,
            ))
            .await
            .unwrap();
        assert_eq!(tick.claims.len(), 1);
        assert_eq!(tick.claims[0].fire().rule.id, "verify");
        engine
            .finish(&tick.claims[0], DONE, start + 6)
            .await
            .unwrap();
        engine
            .tick(input(
                &rules,
                &changed,
                &context,
                &requirements,
                &idle,
                &activity,
                start + 7,
            ))
            .await
            .unwrap();
        // The rule is removed: wt's own wake has nothing to add.
        let tick = engine
            .tick(input(
                &[],
                &changed,
                &context,
                &requirements,
                &idle,
                &activity,
                start + 8,
            ))
            .await
            .unwrap();
        assert!(tick.claims.is_empty(), "{:?}", tick.claims);
    }

    /// wt's own wake for a due check needs no rule and behaves like a wake:
    /// it waits for the session, a pause or a cleared queue consumes
    /// nothing, and it is delivered once per obligation.
    #[tokio::test]
    async fn a_due_check_wakes_its_session_once_and_waits_like_a_wake() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let mut owing = row();
        owing.local_merged = Some(true);
        let mut work = wt_core::WorkStatusRecord::new(wt_core::WorkState::Ready, "t");
        work.set_check(wt_core::VerifyStage::Staging, "1. open the page");
        owing.work = Some(work);
        let rows = [owing];
        let context = AutomationEvalContext::default();
        let (requirements, activity) = (BTreeMap::new(), RuntimeActivity::default());
        let (busy, idle) = (busy_session(), BTreeMap::new());
        let start = now_ms();
        // wt has been waking sessions for due checks for a while: this
        // check came due since.
        let began = start - 1;
        ledger
            .seed_check_wakes(
                &[],
                wt_automations::CHECK_RULE_ID,
                began,
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        let mut paused = input(&[], &rows, &context, &requirements, &idle, &activity, start);
        paused.globally_paused = true;
        assert!(engine.tick(paused).await.unwrap().claims.is_empty());
        let tick = engine
            .tick(input(
                &[],
                &rows,
                &context,
                &requirements,
                &busy,
                &activity,
                start + 1,
            ))
            .await
            .unwrap();
        assert!(tick.claims.is_empty());
        let queued = engine.queued();
        assert_eq!(
            (
                queued[0].rule.as_str(),
                queued[0].detail.as_str(),
                queued[0].waiting_for.as_str()
            ),
            (
                "check",
                "verify in staging: 1. open the page",
                "session busy on worktree"
            )
        );
        let cleared = engine
            .cancel_pending(input(
                &[],
                &rows,
                &context,
                &requirements,
                &busy,
                &activity,
                start + 2,
            ))
            .await
            .unwrap();
        assert_eq!(cleared, 0);

        let tick = engine
            .tick(input(
                &[],
                &rows,
                &context,
                &requirements,
                &idle,
                &activity,
                start + 3,
            ))
            .await
            .unwrap();
        assert_eq!(tick.claims.len(), 1);
        let fire = tick.claims[0].fire();
        assert!(wt_automations::is_check_wake(fire));
        assert_eq!(fire.quiesce_slugs, ["worktree"]);
        engine
            .finish(&tick.claims[0], DONE, start + 4)
            .await
            .unwrap();
        // Still owed, still due: not prompted again.
        let again = engine
            .tick(input(
                &[],
                &rows,
                &context,
                &requirements,
                &idle,
                &activity,
                start + 5,
            ))
            .await
            .unwrap();
        assert!(again.claims.is_empty());
        assert!(engine.queued().is_empty());
    }

    /// A wake the ledger shows sent without proof is settled as such, with
    /// the landing this pass evaluated: the store then keeps it as a due
    /// step that says its delivery is not confirmed.
    #[tokio::test]
    async fn a_settled_wake_carries_its_landing_and_whether_delivery_was_proven() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let mut landed = row();
        landed.local_merged = Some(true);
        let rows = [landed];
        let context = watching(vec![met("aaaaaa", &[(WatchAction::Wake, "go")])]);
        let (requirements, quiescence, activity) =
            (BTreeMap::new(), BTreeMap::new(), RuntimeActivity::default());
        let start = now_ms();
        let pass = |at| {
            input(
                &[],
                &rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                at,
            )
        };
        let tick = engine.tick(pass(start)).await.unwrap();
        assert_eq!(
            tick.claims[0].fire().watch.as_ref().unwrap().landing,
            wt_core::OwnerLanding::Landed
        );
        engine
            .finish(
                &tick.claims[0],
                AutomationOutcome::Ambiguous {
                    reason: "the session did not answer".into(),
                    slot: DeliverySlot::None,
                },
                start + 1,
            )
            .await
            .unwrap();
        let mut restarted = AutomationEngine::new(ledger, CancellationToken::new());
        let tick = restarted.tick(pass(start + 2)).await.unwrap();
        assert!(tick.claims.is_empty());
        assert_eq!(
            tick.settled_watches,
            [SettledWatch {
                id: "aaaaaa".into(),
                action: WatchAction::Wake,
                landing: wt_core::OwnerLanding::Landed,
                unconfirmed: true,
            }]
        );
    }

    /// A wake delivered before its worktree landed closed on delivery, and
    /// its prompt said nothing of closing it. Settled after a restart, by
    /// which time the work has landed, it still closes: the landing is the
    /// one its fire was claimed under.
    #[tokio::test]
    async fn a_wake_settled_after_a_restart_keeps_the_landing_it_was_delivered_under() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let context = watching(vec![met("aaaaaa", &[(WatchAction::Wake, "go")])]);
        let (requirements, quiescence, activity) =
            (BTreeMap::new(), BTreeMap::new(), RuntimeActivity::default());
        let start = now_ms();
        let before = [row()];
        let tick = engine
            .tick(input(
                &[],
                &before,
                &context,
                &requirements,
                &quiescence,
                &activity,
                start,
            ))
            .await
            .unwrap();
        assert_eq!(
            tick.claims[0].fire().watch.as_ref().unwrap().landing,
            wt_core::OwnerLanding::NotLanded
        );
        engine
            .finish(&tick.claims[0], DONE, start + 1)
            .await
            .unwrap();
        let mut landed = row();
        landed.local_merged = Some(true);
        let after = [landed];
        let mut restarted = AutomationEngine::new(ledger, CancellationToken::new());
        let tick = restarted
            .tick(input(
                &[],
                &after,
                &context,
                &requirements,
                &quiescence,
                &activity,
                start + 2,
            ))
            .await
            .unwrap();
        assert_eq!(
            tick.settled_watches,
            [SettledWatch {
                id: "aaaaaa".into(),
                action: WatchAction::Wake,
                landing: wt_core::OwnerLanding::NotLanded,
                unconfirmed: false,
            }]
        );
    }

    /// The process can stop after the ledger recorded a watch action and
    /// before the watch dropped it. The next pass reports the action as
    /// settled instead of leaving the watch met with nothing to deliver it,
    /// and never while the delivery is still in flight.
    #[tokio::test]
    async fn a_watch_action_the_ledger_already_ended_is_reported_settled() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let rows = [row()];
        let context = watching(vec![met("aaaaaa", &[(WatchAction::Notify, "")])]);
        let (requirements, quiescence, activity) =
            (BTreeMap::new(), BTreeMap::new(), RuntimeActivity::default());
        let start = now_ms();
        let pass = |now| {
            input(
                &[],
                &rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                now,
            )
        };

        let tick = engine.tick(pass(start)).await.unwrap();
        assert_eq!(tick.claims.len(), 1);
        assert!(tick.settled_watches.is_empty());
        let flying = engine.tick(pass(start + 1)).await.unwrap();
        assert!(flying.claims.is_empty() && flying.settled_watches.is_empty());
        engine
            .finish(&tick.claims[0], DONE, start + 2)
            .await
            .unwrap();

        // A new process: the ledger remembers, the watch record was never
        // updated.
        let mut restarted = AutomationEngine::new(ledger, CancellationToken::new());
        let tick = restarted.tick(pass(start + 3)).await.unwrap();
        assert!(tick.claims.is_empty());
        assert_eq!(
            tick.settled_watches,
            [SettledWatch {
                id: "aaaaaa".into(),
                action: WatchAction::Notify,
                landing: wt_core::OwnerLanding::NotLanded,
                unconfirmed: false,
            }]
        );
    }

    /// Clearing the gate needs nothing from the session, so it happens while
    /// the session works. The wake comes after it and waits for the session.
    #[tokio::test]
    async fn a_gate_clears_while_the_session_is_busy_and_the_wake_waits() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger, CancellationToken::new());
        let rows = [row()];
        let (requirements, activity) = (BTreeMap::new(), RuntimeActivity::default());
        let busy = busy_session();
        let start = now_ms();
        let gated = watching(vec![met(
            "aaaaaa",
            &[
                (WatchAction::Unblock, "the release"),
                (WatchAction::Wake, "go"),
            ],
        )]);
        let tick = engine
            .tick(input(
                &[],
                &rows,
                &gated,
                &requirements,
                &busy,
                &activity,
                start,
            ))
            .await
            .unwrap();
        assert_eq!(tick.claims.len(), 1);
        let unblock = tick.claims[0].fire().watch.clone().unwrap();
        assert_eq!(
            (unblock.action, unblock.text.as_str()),
            (WatchAction::Unblock, "the release")
        );
        assert!(tick.claims[0].fire().quiesce_slugs.is_empty());
        engine.finish(&tick.claims[0], DONE, start).await.unwrap();

        let mut cleared = met("aaaaaa", &[(WatchAction::Wake, "go")]);
        cleared.cleared_gate = Some("the release".into());
        let cleared = watching(vec![cleared]);
        let tick = engine
            .tick(input(
                &[],
                &rows,
                &cleared,
                &requirements,
                &busy,
                &activity,
                start + 1,
            ))
            .await
            .unwrap();
        assert!(tick.claims.is_empty());
        assert_eq!(engine.queued()[0].waiting_for, "session busy on worktree");

        let idle = BTreeMap::new();
        let tick = engine
            .tick(input(
                &[],
                &rows,
                &cleared,
                &requirements,
                &idle,
                &activity,
                start + 2,
            ))
            .await
            .unwrap();
        let wake = tick.claims[0].fire().watch.clone().unwrap();
        assert_eq!(wake.action, WatchAction::Wake);
        assert_eq!(wake.cleared_gate.as_deref(), Some("the release"));
    }

    /// A rule's prompt whose session cannot be started costs one attempt per
    /// backoff step, not one per pass, is never recorded as handled, and
    /// ends when its worktree does.
    #[tokio::test]
    async fn an_undeliverable_session_prompt_is_tried_once_per_backoff_step() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let rules = [rule("tell-session", 0.0)];
        let rows = [row()];
        let mut context = AutomationEvalContext::default();
        context.actions.insert(
            "tell-session".into(),
            wt_automations::ActionTraits {
                audience: wt_automations::ActionAudience::Session,
                ..Default::default()
            },
        );
        let identity = evaluate(&rules, &rows, &context)
            .iter()
            .map(fire_identity)
            .next()
            .unwrap();
        let requirements = BTreeMap::from([(identity.clone(), RequirementState::Ready)]);
        let (quiescence, activity) = (BTreeMap::new(), RuntimeActivity::default());
        let start = now_ms();
        let pass = |rows, now| {
            input(
                &rules,
                rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                now,
            )
        };
        let unreachable = || AutomationOutcome::Deferred {
            reason: "cannot reach its session: its token helper failed".into(),
            frozen: BTreeMap::new(),
        };

        // The source passes once a second. Five minutes of passes dispatch
        // once per backoff step.
        let mut attempts = 0;
        let mut now = start;
        while now < start + 5 * 60 * 1000 {
            let tick = engine.tick(pass(&rows, now)).await.unwrap();
            for claim in &tick.claims {
                attempts += 1;
                engine.finish(claim, unreachable(), now).await.unwrap();
            }
            now += 1_000;
        }
        let steps = (5 * 60 * 1000 / DEFER_RETRY_MS) as usize;
        assert!((2..=steps + 1).contains(&attempts), "{attempts} attempts");
        assert!(
            engine.queued()[0]
                .waiting_for
                .contains("its token helper failed")
        );
        assert_eq!(engine.queued().len(), 1);
        assert_eq!(ledger.deferred().await.unwrap().len(), 1);

        // A pass that does not show its condition, with the worktree still
        // there, does not start the backoff over.
        let claim = loop {
            let tick = engine.tick(pass(&rows, now)).await.unwrap();
            if let Some(claim) = tick.claims.first() {
                break claim.clone();
            }
            now += 1_000;
        };
        engine.finish(&claim, unreachable(), now).await.unwrap();
        engine.tick(pass(&rows, now + 1)).await.unwrap();
        let waiting = engine.queued().remove(0);
        let hidden = [AutomationRow {
            created_at: None,
            ..row()
        }];
        engine.tick(pass(&hidden, now + 2)).await.unwrap();
        assert!(engine.queued().is_empty() && engine.retains(&identity));
        assert_eq!(ledger.deferred().await.unwrap().len(), 1);
        let back = engine.tick(pass(&rows, now + 3)).await.unwrap();
        assert!(back.claims.is_empty(), "tried again before its time");
        assert_eq!(engine.queued(), [waiting]);
        now += 3;

        // Its worktree removed, nobody is left to hear it.
        let gone = engine.tick(pass(&[], now + DEFER_RETRY_MS)).await.unwrap();
        assert!(gone.claims.is_empty());
        assert_eq!(engine.pending_count(), 0);
        assert!(ledger.deferred().await.unwrap().is_empty());
    }

    /// A session that cannot be reached is tried again on the deferral
    /// backoff, not on every pass, and the deferral survives a restart.
    #[tokio::test]
    async fn a_wake_that_cannot_reach_its_session_backs_off() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let rows = [row()];
        let context = watching(vec![met("aaaaaa", &[(WatchAction::Wake, "go")])]);
        let (requirements, quiescence, activity) =
            (BTreeMap::new(), BTreeMap::new(), RuntimeActivity::default());
        let start = now_ms();
        let pass = |now| {
            input(
                &[],
                &rows,
                &context,
                &requirements,
                &quiescence,
                &activity,
                now,
            )
        };

        let tick = engine.tick(pass(start)).await.unwrap();
        assert_eq!(tick.claims.len(), 1);
        let unreachable = AutomationOutcome::Deferred {
            reason: "no session can be reached".into(),
            frozen: BTreeMap::new(),
        };
        engine
            .finish(&tick.claims[0], unreachable.clone(), start)
            .await
            .unwrap();
        for now in [start + 1_000, start + DEFER_RETRY_MS - 1] {
            assert!(engine.tick(pass(now)).await.unwrap().claims.is_empty());
        }
        assert_eq!(
            engine.queued()[0].waiting_for,
            "a re-check: no session can be reached"
        );
        let key = action_key("aaaaaa", WatchAction::Wake);
        assert!(!ledger.snapshot().await.unwrap().has_handled(&key, start));

        let mut restarted = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        assert_eq!(restarted.restore_deferred(start + 2_000).await.unwrap(), 1);
        assert!(
            restarted
                .tick(pass(start + 3_000))
                .await
                .unwrap()
                .claims
                .is_empty()
        );
        let retry = restarted
            .tick(pass(start + 2_000 + DEFER_RETRY_MS))
            .await
            .unwrap();
        assert_eq!(retry.claims.len(), 1);
        assert_eq!(retry.claims[0].fire().watch.as_ref().unwrap().text, "go");

        // Withdrawn while deferred (its watch was cancelled): the stored
        // deferral goes too, so no later start restores it.
        restarted
            .finish(&retry.claims[0], unreachable, start + 40_000)
            .await
            .unwrap();
        let none = watching(Vec::new());
        restarted
            .tick(input(
                &[],
                &rows,
                &none,
                &requirements,
                &quiescence,
                &activity,
                start + 41_000,
            ))
            .await
            .unwrap();
        assert!(ledger.deferred().await.unwrap().is_empty());
    }

    /// A shell action that exits non-zero is run again on a backoff, a
    /// bounded number of times, and then ends as failed instead of
    /// delivered. A retry survives a restart, and an earlier attempt's run
    /// record does not end a later one.
    #[tokio::test]
    async fn a_failed_shell_run_is_retried_on_a_backoff_and_then_ends_failed() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let rules = [AutomationDef {
            on: AutomationTrigger::WtMerged,
            ..rule("tracker", 0.0)
        }];
        let rows = [AutomationRow {
            local_merged: Some(true),
            clean_candidate: true,
            ..row()
        }];
        let mut context = AutomationEvalContext::default();
        // No frozen inputs: the fire is under the breaker, which counts
        // the first dispatch and none of its retries.
        context.actions.insert(
            "tracker".into(),
            wt_automations::ActionTraits {
                shell: true,
                external: false,
                audience: wt_automations::ActionAudience::None,
                retry_on_failure: true,
            },
        );
        let requirements = BTreeMap::from([("test|worktree".into(), RequirementState::Ready)]);
        let quiescence = BTreeMap::new();
        let activity = RuntimeActivity::default();
        let cancel = CancellationToken::new();
        let launched = AutomationOutcome::Delivered {
            slot: DeliverySlot::Headless,
        };
        let failed = RunEnd::ShellFailed { exit_code: 7 };
        let mut at = now_ms();
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

        let first = engine.tick(pass(at)).await.unwrap();
        let claim = first.claims.first().unwrap().clone();
        let keys = claim.fire().fire_keys.clone();
        let first_started = at;
        engine.finish(&claim, launched.clone(), at).await.unwrap();
        assert!(engine.awaits_runs());
        // A run that ended any other way leaves the fire delivered.
        let mut other = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        assert_eq!(
            other
                .run_ended(&keys, at, failed, at, &cancel)
                .await
                .unwrap(),
            None,
            "only the engine that launched a run acts on its end"
        );

        for (attempt, delay) in RUN_RETRY_DELAYS_MS.into_iter().enumerate() {
            let report = engine
                .run_ended(&keys, at, failed, at + 1, &cancel)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(report.runs as usize, attempt + 1);
            assert_eq!(report.retry_in_ms, Some(delay));
            assert!(!engine.awaits_runs());
            let snapshot = ledger.snapshot().await.unwrap();
            assert!(keys.iter().all(|key| !snapshot.recorded(key)));

            // A restart keeps the retry and its count.
            engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
            assert_eq!(engine.restore_deferred(at + 1).await.unwrap(), 1);
            let early = engine.tick(pass(at + 2)).await.unwrap();
            assert!(early.claims.is_empty());
            assert!(
                engine.queued()[0].waiting_for.contains("after exit 7"),
                "{:?}",
                engine.queued()
            );
            // The restart's own re-check delay may be the longer one.
            at += 1 + delay.max(DEFER_RETRY_MS);
            let tick = engine.tick(pass(at)).await.unwrap();
            let claim = tick.claims.first().unwrap().clone();
            assert_eq!(claim.fire().fire_keys, keys);
            engine.finish(&claim, launched.clone(), at).await.unwrap();
            assert!(ledger.deferred().await.unwrap().is_empty());
            // The first attempt's record is older than this claim.
            assert_eq!(
                engine
                    .run_ended(&keys, first_started, failed, at, &cancel)
                    .await
                    .unwrap(),
                None
            );
            assert!(engine.awaits_runs());
        }

        let last = engine
            .run_ended(&keys, at, failed, at + 1, &cancel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(last.retry_in_ms, None);
        assert_eq!(last.runs as usize, RUN_RETRY_DELAYS_MS.len() + 1);
        let snapshot = ledger.snapshot().await.unwrap();
        let record = snapshot.recent(1).remove(0);
        assert_eq!(record.state, "failed");
        assert_eq!(record.reason.as_deref(), Some("exit 7 after 4 runs"));
        assert!(last.retried);
        assert_eq!(snapshot.breaker_state("test", "worktree", at).count, 1);
        // Failed is terminal: the still-true condition does not fire again.
        let after = engine.tick(pass(at + 2)).await.unwrap();
        assert!(after.claims.is_empty());
        assert_eq!(engine.pending_count(), 0);
    }

    /// A deferred branch delete whose condition stopped being produced must
    /// not run until that condition is seen to hold again: not after a
    /// restart (its ledger record alone is not a fire to run), and not
    /// because its worktree was removed meanwhile. Being a fire that needs
    /// no checkout, it is also not forgotten for the removal alone.
    #[tokio::test]
    async fn a_dormant_fire_does_not_run_until_its_condition_is_seen_again() {
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
        // New commits: the branch is no longer a clean, landed candidate.
        let moved_on = [row()];
        let removed: [AutomationRow; 0] = [];
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
        let id = "delete-branch-on-merge|worktree";
        let deferred = || AutomationOutcome::Deferred {
            reason: "open PR(s) #7 still target worktree".into(),
            frozen: BTreeMap::new(),
        };

        let first = engine.tick(pass(&landed, start)).await.unwrap();
        engine
            .finish(&first.claims[0], deferred(), start + 1)
            .await
            .unwrap();
        engine.tick(pass(&moved_on, start + 2)).await.unwrap();
        assert_eq!(engine.pending_count(), 0);
        assert_eq!(
            ledger.deferred().await.unwrap()[id]["dormantSinceMs"],
            start + 2
        );

        // Its worktree removed while it is away: still no fire, and its
        // backoff is kept as a queued one's would be.
        let late = start + DEFER_SLOW_RETRY_MS;
        let gone = engine.tick(pass(&removed, late)).await.unwrap();
        assert!(gone.claims.is_empty());
        assert!(engine.retains(id) && engine.pending_count() == 0);

        // A restart reads the record back as away, not as a fire to run.
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        assert_eq!(engine.restore_deferred(late).await.unwrap(), 0);
        assert_eq!(engine.pending_count(), 0);
        let after = engine
            .tick(pass(&removed, late + DEFER_SLOW_RETRY_MS))
            .await
            .unwrap();
        assert!(after.claims.is_empty(), "ran without its condition");
        assert!(engine.retains(id));

        // Seen to hold again, it resumes; the record says so.
        let mut resumed = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        resumed.restore_deferred(late).await.unwrap();
        let back = resumed
            .tick(pass(&landed, late + DEFER_SLOW_RETRY_MS))
            .await
            .unwrap();
        assert_eq!(back.claims.len(), 1);
        assert!(ledger.deferred().await.unwrap()[id]["dormantSinceMs"].is_null());
        resumed
            .finish(&back.claims[0], deferred(), late + DEFER_SLOW_RETRY_MS)
            .await
            .unwrap();

        // Away for the whole window, counted from when it first went, it
        // is forgotten whichever process is looking.
        engine
            .tick(pass(&removed, start + 3 + DORMANT_MAX_MS))
            .await
            .unwrap();
        assert!(!engine.retains(id));
    }

    /// A condition read from GitHub holds only while that data is fresh, so
    /// a pass between two retries may not produce the fire at all. The
    /// retry keeps its count and its wait through that pass and the fire
    /// still ends as failed; forgetting them there ran it forever. A fire
    /// that stays away is forgotten and starts over if it ever returns.
    #[tokio::test]
    async fn a_retry_keeps_its_count_when_its_condition_flickers() {
        let rules = [AutomationDef {
            on: AutomationTrigger::WtMerged,
            ..rule("tracker", 0.0)
        }];
        let rows = [AutomationRow {
            local_merged: Some(true),
            clean_candidate: true,
            ..row()
        }];
        // The worktree is there; what showed it landed is not, this pass.
        let unconfirmed = [row()];
        let mut context = AutomationEvalContext::default();
        context.actions.insert(
            "tracker".into(),
            wt_automations::ActionTraits {
                shell: true,
                retry_on_failure: true,
                ..Default::default()
            },
        );
        let requirements = BTreeMap::from([("test|worktree".into(), RequirementState::Ready)]);
        let quiescence = BTreeMap::new();
        let activity = RuntimeActivity::default();
        let cancel = CancellationToken::new();
        let launched = AutomationOutcome::Delivered {
            slot: DeliverySlot::Headless,
        };
        let failed = RunEnd::ShellFailed { exit_code: 2 };
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

        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let mut at = now_ms();
        let queued_at = at;
        let first = engine.tick(pass(&rows, at)).await.unwrap();
        let claim = first.claims.first().unwrap().clone();
        let keys = claim.fire().fire_keys.clone();
        engine.finish(&claim, launched.clone(), at).await.unwrap();
        for (attempt, delay) in RUN_RETRY_DELAYS_MS.into_iter().enumerate() {
            let report = engine
                .run_ended(&keys, at, failed, at + 1, &cancel)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(report.runs as usize, attempt + 1, "the count started over");
            assert_eq!(report.retry_in_ms, Some(delay));
            // One pass does not produce the fire: it leaves the queue but
            // is not forgotten, here or in the ledger.
            let gone = engine.tick(pass(&unconfirmed, at + 2)).await.unwrap();
            assert!(gone.claims.is_empty() && engine.queued().is_empty());
            assert!(engine.retains("test|worktree"));
            assert_eq!(ledger.deferred().await.unwrap().len(), 1);
            // Back again, it waits out the rest of its backoff.
            let early = engine.tick(pass(&rows, at + 3)).await.unwrap();
            assert!(early.claims.is_empty(), "the retry ran before its time");
            let waiting = engine.queued().remove(0);
            assert!(waiting.waiting_for.contains("after exit 2"), "{waiting:?}");
            assert_eq!(waiting.queued_at_ms, queued_at);
            at += 1 + delay;
            let tick = engine.tick(pass(&rows, at)).await.unwrap();
            let claim = tick.claims.first().unwrap().clone();
            engine.finish(&claim, launched.clone(), at).await.unwrap();
        }
        let last = engine
            .run_ended(&keys, at, failed, at + 1, &cancel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(last.retry_in_ms, None);
        assert_eq!(last.runs as usize, RUN_RETRY_DELAYS_MS.len() + 1);
        // Ended failed: the same landing never fires again, however often
        // its condition goes and returns.
        for step in [2, 3, 4] {
            let rows: &[AutomationRow] = if step == 3 { &unconfirmed } else { &rows };
            let tick = engine.tick(pass(rows, at + step)).await.unwrap();
            assert!(tick.claims.is_empty());
            assert_eq!(engine.pending_count(), 0);
        }
        assert!(ledger.deferred().await.unwrap().is_empty());

        // A fire that stays away is forgotten, once, and starts over.
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let first = engine.tick(pass(&rows, at)).await.unwrap();
        let claim = first.claims.first().unwrap().clone();
        engine.finish(&claim, launched.clone(), at).await.unwrap();
        engine
            .run_ended(&keys, at, failed, at + 1, &cancel)
            .await
            .unwrap()
            .unwrap();
        engine.tick(pass(&unconfirmed, at + 2)).await.unwrap();
        engine
            .tick(pass(&unconfirmed, at + 2 + DORMANT_MAX_MS))
            .await
            .unwrap();
        assert!(engine.retains("test|worktree"), "forgotten before its time");
        engine
            .tick(pass(&unconfirmed, at + 3 + DORMANT_MAX_MS))
            .await
            .unwrap();
        assert!(!engine.retains("test|worktree"));
        assert!(ledger.deferred().await.unwrap().is_empty());
        at += 4 + DORMANT_MAX_MS;
        let again = engine.tick(pass(&rows, at)).await.unwrap();
        let claim = again.claims.first().unwrap().clone();
        engine.finish(&claim, launched, at).await.unwrap();
        let report = engine
            .run_ended(&keys, at, failed, at + 1, &cancel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.runs, 1);
    }

    /// Without `retry_on_failure` a non-zero exit ends the fire as failed
    /// at once, in a form a build from before failed fires reads as handled.
    #[tokio::test]
    async fn a_failed_shell_run_is_not_run_again_unless_its_action_asks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let ledger = AutomationLedger::new(&path);
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let rules = [rule("custom-action", 0.0)];
        let rows = [row()];
        let mut context = AutomationEvalContext::default();
        context.actions.insert(
            "custom-action".into(),
            wt_automations::ActionTraits {
                shell: true,
                ..Default::default()
            },
        );
        let requirements = BTreeMap::from([("test|worktree".into(), RequirementState::Ready)]);
        let (quiescence, activity) = (BTreeMap::new(), RuntimeActivity::default());
        let cancel = CancellationToken::new();
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
        let tick = engine.tick(pass(start)).await.unwrap();
        let claim = tick.claims.first().unwrap().clone();
        let keys = claim.fire().fire_keys.clone();
        let launched = AutomationOutcome::Delivered {
            slot: DeliverySlot::Headless,
        };
        engine.finish(&claim, launched, start).await.unwrap();
        let failed = engine
            .run_ended(
                &keys,
                start,
                RunEnd::ShellFailed { exit_code: 3 },
                start + 1,
                &cancel,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!((failed.runs, failed.retry_in_ms), (1, None));
        assert!(!failed.retried);
        assert!(ledger.deferred().await.unwrap().is_empty());
        let snapshot = ledger.snapshot().await.unwrap();
        let record = snapshot.recent(1).remove(0);
        assert_eq!(record.state, "failed");
        assert_eq!(record.reason.as_deref(), Some("exit 3"));
        assert!(!snapshot.delivered(&keys[0]));
        assert!(snapshot.has_handled(&keys[0], start + 2));
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let entry = &stored["fired"][&keys[0]];
        assert_eq!(entry["state"], "delivered");
        assert_eq!(
            (&entry["failed"], &entry["failedRuns"]),
            (&true.into(), &1.into())
        );
        let after = engine.tick(pass(start + 2)).await.unwrap();
        assert!(after.claims.is_empty());
        assert_eq!(engine.pending_count(), 0);
    }

    /// The process that launched a run may quit before the run ends. The
    /// next one awaits it again and acts on the exit its record shows; a
    /// run whose exit cannot be recovered is ended as unknown, told once
    /// and never run again.
    #[tokio::test]
    async fn a_run_that_outlives_its_engine_is_settled_by_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let rules = [AutomationDef {
            on: AutomationTrigger::WtMerged,
            ..rule("tracker", 0.0)
        }];
        let rows = [AutomationRow {
            local_merged: Some(true),
            clean_candidate: true,
            ..row()
        }];
        let mut context = AutomationEvalContext::default();
        context.actions.insert(
            "tracker".into(),
            wt_automations::ActionTraits {
                shell: true,
                external: false,
                audience: wt_automations::ActionAudience::None,
                retry_on_failure: true,
            },
        );
        let requirements = BTreeMap::from([("test|worktree".into(), RequirementState::Ready)]);
        let (quiescence, activity) = (BTreeMap::new(), RuntimeActivity::default());
        let cancel = CancellationToken::new();
        let launched = AutomationOutcome::Delivered {
            slot: DeliverySlot::Headless,
        };
        let failed = RunEnd::ShellFailed { exit_code: 7 };
        let mut at = now_ms();
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
        let tick = engine.tick(pass(at)).await.unwrap();
        let claim = tick.claims.first().unwrap().clone();
        let keys = claim.fire().fire_keys.clone();
        engine.finish(&claim, launched.clone(), at).await.unwrap();

        // A new process awaits the run again. While its record shows it
        // running it is not lost.
        let mut next = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        assert!(!next.awaits_runs());
        assert_eq!(next.restore_awaited(at + 1).await.unwrap(), 1);
        assert_eq!(next.restore_awaited(at + 1).await.unwrap(), 0);
        next.end_lost_runs(&[(keys.clone(), at)], at + 1, &cancel)
            .await
            .unwrap();
        assert!(next.awaits_runs());
        // The record then shows a definite failure: the fire is retried,
        // by one process only.
        let report = next
            .run_ended(&keys, at, failed, at + 1, &cancel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (report.runs, report.retry_in_ms),
            (1, Some(RUN_RETRY_DELAYS_MS[0]))
        );
        assert_eq!(
            engine
                .run_ended(&keys, at, failed, at + 1, &cancel)
                .await
                .unwrap(),
            None
        );
        assert!(ledger.awaited().await.unwrap().is_empty());
        assert_eq!(ledger.deferred().await.unwrap().len(), 1);

        // The retry is launched, and that process quits too. Nothing is
        // left of the run: it may have succeeded, so it is not run again.
        at += 1 + RUN_RETRY_DELAYS_MS[0].max(DEFER_RETRY_MS);
        let tick = next.tick(pass(at)).await.unwrap();
        let claim = tick.claims.first().unwrap().clone();
        next.finish(&claim, launched.clone(), at).await.unwrap();
        let mut third = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        assert_eq!(third.restore_awaited(at + 1).await.unwrap(), 1);
        third.end_lost_runs(&[], at + 1, &cancel).await.unwrap();
        assert!(!third.awaits_runs());
        assert_eq!(
            third.take_unknown_runs(),
            [UnknownRun {
                slug: "worktree".into(),
                rule: "test".into(),
                reason: RUN_RECORD_MISSING.into(),
            }]
        );
        assert!(third.take_unknown_runs().is_empty());
        let snapshot = ledger.snapshot().await.unwrap();
        let unknown = snapshot.unknown_outcomes();
        assert_eq!(unknown.len(), 1);
        assert_eq!(
            (unknown[0].state.as_str(), unknown[0].reason.as_deref()),
            ("delivered", Some(RUN_RECORD_MISSING))
        );
        assert!(snapshot.has_handled(&keys[0], at + 2));
        assert!(ledger.awaited().await.unwrap().is_empty());
        // The outcome is final for every process: a failure read later
        // neither retries the fire nor reports it again.
        assert_eq!(
            next.run_ended(&keys, at, failed, at + 2, &cancel)
                .await
                .unwrap(),
            None
        );
        assert!(ledger.deferred().await.unwrap().is_empty());
        let mut fourth = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        assert_eq!(fourth.restore_awaited(at + 2).await.unwrap(), 0);
        assert_eq!(fourth.restore_deferred(at + 2).await.unwrap(), 0);
        let after = fourth.tick(pass(at + 3)).await.unwrap();
        assert!(after.claims.is_empty());
        assert_eq!(fourth.pending_count(), 0);
    }

    /// A run whose worker died before recording an exit code may have
    /// succeeded. With `retry_on_failure` that is told once and the fire
    /// is not run again; without it the fire simply stays delivered, as
    /// does a run that ended any other way.
    #[tokio::test]
    async fn a_run_without_a_recorded_exit_is_unknown_and_not_retried() {
        for retry_on_failure in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let ledger = AutomationLedger::new(dir.path().join("automations.json"));
            let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
            let rules = [AutomationDef {
                on: AutomationTrigger::WtMerged,
                ..rule("tracker", 0.0)
            }];
            let rows = [AutomationRow {
                local_merged: Some(true),
                clean_candidate: true,
                ..row()
            }];
            let mut context = AutomationEvalContext::default();
            context.actions.insert(
                "tracker".into(),
                wt_automations::ActionTraits {
                    shell: true,
                    external: false,
                    audience: wt_automations::ActionAudience::None,
                    retry_on_failure,
                },
            );
            let requirements = BTreeMap::from([("test|worktree".into(), RequirementState::Ready)]);
            let (quiescence, activity) = (BTreeMap::new(), RuntimeActivity::default());
            let cancel = CancellationToken::new();
            let at = now_ms();
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
            let tick = engine.tick(pass(at)).await.unwrap();
            let claim = tick.claims.first().unwrap().clone();
            let keys = claim.fire().fire_keys.clone();
            let launched = AutomationOutcome::Delivered {
                slot: DeliverySlot::Headless,
            };
            engine.finish(&claim, launched, at).await.unwrap();
            assert_eq!(ledger.awaited().await.unwrap().len(), 1);
            assert_eq!(
                engine
                    .run_ended(&keys, at, RunEnd::ShellUnknown, at + 1, &cancel)
                    .await
                    .unwrap(),
                None
            );
            let told = engine.take_unknown_runs();
            assert_eq!(told.len(), usize::from(retry_on_failure));
            if let Some(told) = told.first() {
                assert_eq!(told.reason, RUN_EXIT_UNRECORDED);
            }
            let snapshot = ledger.snapshot().await.unwrap();
            assert_eq!(
                snapshot.unknown_outcomes().len(),
                usize::from(retry_on_failure)
            );
            assert!(snapshot.has_handled(&keys[0], at + 2));
            assert!(ledger.awaited().await.unwrap().is_empty());
            assert!(ledger.deferred().await.unwrap().is_empty());
            // Nothing is left for a later process to settle or run.
            let mut next = AutomationEngine::new(ledger.clone(), CancellationToken::new());
            assert_eq!(next.restore_awaited(at + 2).await.unwrap(), 0);
            let after = next.tick(pass(at + 2)).await.unwrap();
            assert!(after.claims.is_empty());
        }
    }

    /// A run that succeeded, was killed, or is not a shell command leaves
    /// its fire delivered.
    #[tokio::test]
    async fn a_run_that_did_not_exit_non_zero_stays_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let mut engine = AutomationEngine::new(ledger.clone(), CancellationToken::new());
        let rules = [rule("custom-action", 0.0)];
        let rows = [row()];
        let context = AutomationEvalContext::default();
        let requirements = BTreeMap::from([("test|worktree".into(), RequirementState::Ready)]);
        let (quiescence, activity) = (BTreeMap::new(), RuntimeActivity::default());
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
        let claim = tick.claims.first().unwrap().clone();
        let keys = claim.fire().fire_keys.clone();
        engine
            .finish(
                &claim,
                AutomationOutcome::Delivered {
                    slot: DeliverySlot::Headless,
                },
                start,
            )
            .await
            .unwrap();
        let ended = engine
            .run_ended(
                &keys,
                start,
                RunEnd::Other,
                start + 1,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(ended, None);
        assert!(!engine.awaits_runs());
        assert!(ledger.snapshot().await.unwrap().delivered(&keys[0]));
    }

    /// Deferred fires written before watches existed still load, and one
    /// carrying an action this build does not know is skipped, not run.
    #[test]
    fn stored_fires_load_across_builds() {
        let context = watching(vec![met("aaaaaa", &[(WatchAction::Wake, "go")])]);
        let fire = evaluate(&[], &[row()], &context).remove(0);
        let mut stored = serde_json::to_value(&fire).unwrap();
        assert_eq!(stored["watch"]["action"], "wake");
        stored["watch"]["action"] = "teleport".into();
        assert!(serde_json::from_value::<AutomationFire>(stored.clone()).is_err());
        stored.as_object_mut().unwrap().remove("watch");
        assert_eq!(
            serde_json::from_value::<AutomationFire>(stored)
                .unwrap()
                .watch,
            None
        );
    }
}
