//! Worktree and fleet operations that both the CLI and the host run: removal
//! and its background worker, creation, fork bases, landing evidence,
//! restacking, the GitHub events daemon, release updates, managed workers and
//! remote dispatch, and the command implementations those reuse. Depends only
//! on wt-app-core among the application crates (docs/architecture.md).

pub mod action_dispatch;
pub mod automation_builtins;
pub mod commands;
pub mod controller_tunnels;
pub mod events;
pub mod fork_base;
pub mod host_cleanup;
pub mod install;
pub mod lifecycle_ops;
pub mod managed;
pub mod origin;
pub mod remote;
pub mod restack_action;
pub mod updates;
pub mod worktree_facts;
