//! Bounded, read-only harness naming for committed worktree changes.
//!
//! The application owns persistence and presentation. This crate builds
//! content-addressed diff contexts, invokes a configured installed harness in
//! read-only one-shot mode, and parses its response.

mod cache_key;
mod completion;
mod diff;
mod service;

pub use cache_key::{NamingCacheKey, diff_cache_key, stack_cache_key, stack_signature};
pub use completion::{
    CompletionSpec, HarnessCompletionError, HarnessPrograms, build_completion_spec,
    parse_stack_title, parse_title_description,
};
pub use diff::{
    DiffContext, DiffContextError, FileMode, ModeCounts, build_diff_context, compact_diff,
    parse_diff_parts,
};
pub use service::{NamingError, NamingService, NamingServiceConfig};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AiSummary {
    /// Absent when the model omitted the `TITLE:` marker.
    pub title: Option<String>,
    /// On successful completion this is always present, possibly the full
    /// response if the model omitted structured markers.
    pub description: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StackMember {
    pub branch: String,
    pub title: String,
}
