//! Safe restacking of inferred worktree stacks.

mod backup;
mod chain;
mod git;
mod replay;
mod service;

pub use backup::PruneBackupsResult;
pub use chain::{RestackChain, StackStep};
pub use service::{
    RestackOptions, RestackOutcome, StackConfig, StackError, StackEvent, StackService, StateConfig,
};
