//! Pure automation evaluation and its durable once-only ledger.

mod evaluate;
mod ledger;
mod queue;
mod types;

pub use evaluate::{
    CHECK_RULE_ID, CHECK_RUN, FLEET_SLUG, MERGE_ARM_GRACE_MS, WATCH_RULE_ID, WATCH_RUN, check_rule,
    checks_newly_due, eligible, evaluate, evaluate_breaker_resets, finished_merge_arms,
    fire_identity, guide_owner, is_check_wake, mcp_fire_key, mcp_subscription, merge_dropped,
    observed_merge_arms, pending_mcp_events, watch_rule,
};
pub use ledger::{
    AutomationLedger, BREAKER_LIMIT, BreakerState, DispatchClaim, DispatchClaimResult,
    DispatchKind, DispatchPolicy, DispatchRequest, LedgerError, LedgerRecord, LedgerSnapshot,
};
pub use queue::{
    CancellableFiresInput, LOCAL_SIGHTING_WINDOW_MS, QueueIntentsInput, cancellable_fires,
    queue_intents,
};
pub use types::{
    ActionAudience, ActionTraits, AutomationConflict, AutomationEvalContext, AutomationFire,
    AutomationIntent, AutomationPr, AutomationRow, AutomationStack, BranchRange, BranchTip,
    CheckFire, DispatchDisposition, FrozenPr, GuideOwner, McpEvent, MetWatch, PauseSnapshot,
    StackParent, WatchFire, status_is_gated, status_suffix, status_trigger_state,
};
