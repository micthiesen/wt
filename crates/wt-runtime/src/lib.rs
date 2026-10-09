//! Supervised background work and push-driven source snapshots.

mod scope;
mod source;

pub use scope::{ShutdownError, TaskScope};
pub use source::{
    RefreshPolicy, SourceHandle, SourcePublisher, SourceSnapshot, SourceState, source_channel,
    start_source,
};
