//! Cancellable, lock-scoped local worktree lifecycle operations.

mod copy_globs;
mod creation_trace;
mod removal_trace;
mod rift_binary;
mod service;

pub use service::{
    CleanupCandidate, CreateOptions, CreateResult, LifecycleError, LifecycleService,
    MISSING_CHECKOUT_HAZARD, REMOVED_CHANGED, RemovalRevision, RemoveOptions, RemoveResult,
    ServiceConfig, StoreLocation, UNCOMMITTED_HAZARD, checkout_missing, is_lost_work_hazard,
    unpushed_hazard,
};
