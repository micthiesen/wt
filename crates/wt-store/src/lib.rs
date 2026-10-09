//! Durable, repository-scoped state storage for wt.
//!
//! `Store` is synchronous and owns its SQLite connection. Callers that need
//! asynchronous behavior should move it to a dedicated database thread rather
//! than hold an async mutex while SQLite performs I/O.

mod migrations;
mod mutations;
mod store;
mod types;

pub use migrations::{
    CURRENT_WT_STATE_VERSION, MigrationOutcome, migrate_wt_state, raw_wt_state_version,
};
pub use mutations::{is_merged_removal, verification_owed_at_removal};
pub use store::{Store, StoreError};
pub use types::{
    ForeignRepositoryRow, MergeEdge, RemovedWorktree, RepositoryIdentity, ReviewRequestDismissal,
    WorkStatusRecord, WtState,
};

#[cfg(test)]
mod tests;
