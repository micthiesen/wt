//! The foundation of the wt application: the process context (configuration,
//! database, repository, process budgets), build identity, harness glue,
//! host wire protocol and request classification, and small helpers every
//! layer above shares. Nothing here depends on another wt application crate.
//! The layering is documented in docs/architecture.md.

pub mod action_builtins;
pub mod actions;
pub mod automation_engine;
pub mod bootstrap;
pub mod build_info;
pub mod codex_reauth;
pub mod context;
pub mod database;
pub mod dev;
pub mod display_time;
pub mod editor;
pub mod feeds;
pub mod harness;
pub mod host_errors;
pub mod host_protocol;
pub mod host_routing;
pub mod issue_identity;
pub mod issue_tracker;
pub mod lanes;
pub mod logging;
pub mod managed_pi;
pub mod prompt;
pub mod query_cache;
pub mod session_discovery;
pub mod skills;
#[cfg(any(test, feature = "testing"))]
pub mod test_support;
pub mod watcher;
pub mod watcher_cleanup;
pub mod workload;
