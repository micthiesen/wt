//! Supervised per-worktree development servers and fleet slot management.

mod queue;
mod service;
mod supervisor;

pub use queue::{DevWaiter, QueueReport};
pub use service::{
    DevHealth, DevServerConfig, DevServerError, DevServerService, DevServerStatus, DevSlotDecision,
    DevSlotHolder, DevSlotReport, DevStartOptions, DevStartOutcome, DevStatusRow,
    DevStatusSnapshot, DevWorktree, ReadyOutcome, RestartStatus, StateConfig, WaitOutcome,
    WaitingStatus,
};
pub use supervisor::{SupervisorConfig, SupervisorError, SupervisorExit, run_supervisor};
