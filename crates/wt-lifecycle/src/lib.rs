//! Cancellable, lock-scoped local worktree lifecycle operations.

mod creation_trace;
mod rift_binary;
mod service;

pub use service::{
    CleanupCandidate, CreateOptions, CreateResult, LifecycleError, LifecycleService,
    REMOVED_CHANGED, RemovalRevision, RemoveOptions, RemoveResult, ServiceConfig, StoreLocation,
    UNCOMMITTED_HAZARD, is_lost_work_hazard, unpushed_hazard,
};
