//! Everything that feeds the board: the host-local source graph (`sources::start`),
//! its leaves (Git, GitHub, sessions, actions, issues, history, activity) and
//! the derived stages that prepare rows, details and layout. Depends on
//! wt-app-core and wt-ops among the application crates (docs/architecture.md).

pub mod action_source;
pub mod activity_source;
pub mod attention_source;
pub mod automation_facts;
pub mod automation_source;
pub mod board_layout;
pub mod dev_source;
pub mod freshness;
pub mod git_presentation;
pub mod github_actions;
pub mod github_events_source;
pub mod github_pickers;
pub mod github_source;
pub mod hard_refresh;
pub mod history_source;
pub mod inventory;
pub mod issue_source;
pub mod local_source;
pub mod naming_source;
pub mod palette_source;
pub mod perf_source;
pub mod review_requests;
pub mod session_activity;
pub mod session_board;
pub mod session_source;
pub mod sources;
pub mod update_source;
pub mod work_presentation;
