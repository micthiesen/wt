//! The host: the controller that schedules UI requests onto lanes, the host
//! service and dispatch that execute them, action palettes, remote hosts and
//! the fleet board that merges them, and the terminal palette loop. Depends on
//! wt-app-core, wt-ops and wt-sources among the application crates
//! (docs/architecture.md).

pub mod action_palette;
pub mod controller;
pub mod controller_actions;
pub mod creation_placement;
pub mod fleet_cleanup;
pub mod history_actions;
pub mod host_dispatch;
pub mod host_server;
pub mod host_service;
pub mod host_stdio;
pub mod remote_board;
pub mod remote_host;
pub mod section_actions;
pub mod session_ui;
pub mod terminal_palette;
