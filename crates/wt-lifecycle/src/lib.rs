//! Cancellable, lock-scoped local worktree lifecycle operations.

mod rift_binary;
mod service;

pub use service::{
    CleanupCandidate, CreateOptions, CreateResult, LifecycleError, LifecycleService,
    RemovalRevision, RemoveOptions, RemoveResult, ServiceConfig, StoreLocation,
};
