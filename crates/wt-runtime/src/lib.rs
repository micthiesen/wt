//! Supervised background work and the shared query layer: push-driven
//! sources with provenance, a disposable per-source cache, confirmed-only
//! inputs for safety decisions, and one optimistic-update engine.

mod cache;
#[cfg(any(test, feature = "testing"))]
pub mod conformance;
mod domain;
mod pending;
mod scope;
mod snapshot;
mod source;
mod stamp;

pub use cache::{CacheStore, DiskCache, MAX_CACHE_AGE, MAX_CACHE_BYTES};
pub use domain::Domain;
pub use pending::{Expired, Mutation, Pending, PendingLimits, PendingPhase, PendingView, Target};
pub use scope::{ShutdownError, TaskScope};
pub use snapshot::{
    Confirmed, DomainMap, DomainMeta, Provenance, SourceSnapshot, SourceState, merge_domains,
};
pub use source::{
    DerivedPublisher, Fetched, LeafHandle, LeafPublisher, Persistence, RefreshPolicy, SourceHandle,
    SourceInfo, SourceKind, SourceSpec, derived_channel, leaf_channel, start_observed_source,
    start_skippable_source, start_source,
};
pub use stamp::Stamp;
