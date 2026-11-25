//! Durable, repository-scoped state storage for wt.
//!
//! `Store` is synchronous and owns its SQLite connection. Callers that need
//! asynchronous behavior should move it to a dedicated database thread rather
//! than hold an async mutex while SQLite performs I/O.

mod events;
mod migrations;
mod mutations;
mod store;
mod titles;
mod types;
mod watches;

pub use events::{
    WORKTREE_EVENT_CHECKS_MAX, WORKTREE_EVENT_MAX_AGE_MS, WORKTREE_EVENTS_PER_SLUG,
    bound_worktree_event,
};
pub use migrations::{
    CURRENT_WT_STATE_VERSION, MigrationOutcome, migrate_wt_state, raw_wt_state_version,
};
pub use mutations::{
    BaseUpdate, ClearFailedRemoval, RemovedSettle, SeenToggle, is_merged_removal,
    verification_owed_at_removal, work_status_event,
};
pub use store::{Store, StoreError};
pub use titles::{TitleSource, TitleWrite, sentence_case, stored_title};
pub use types::{
    ForeignRepositoryRow, GuideRecord, MergeEdge, RemovedWorktree, RepositoryIdentity,
    ReviewRequestDismissal, WorkStatusRecord, WorkStatusWrite, WtState,
};
pub use watches::{
    NewWatch, UnblockDelivery, WatchRefusal, WatchSelector, cleared_gate, stored_watches,
};

#[cfg(test)]
mod events_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod watches_tests;
