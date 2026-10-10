//! Pure automation evaluation and its durable once-only ledger.

mod evaluate;
mod ledger;
mod queue;
mod types;

pub use evaluate::{FLEET_SLUG, eligible, evaluate, fire_identity};
pub use ledger::{AutomationLedger, BreakerState, LedgerError};
pub use queue::{CancellableFiresInput, QueueIntentsInput, cancellable_fires, queue_intents};
pub use types::{
    ActionAudience, ActionTraits, AutomationConflict, AutomationEvalContext, AutomationFire,
    AutomationIntent, AutomationPr, AutomationRow, AutomationStack, BranchRange, BranchTip,
    DispatchDisposition, FrozenPr, PauseSnapshot, StackParent, status_is_gated, status_suffix,
    status_trigger_state,
};
