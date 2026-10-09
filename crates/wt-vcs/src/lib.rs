//! Read-only Git repository and worktree inventory services.

mod repository;
mod status;

pub use repository::{
    DiffStats, GitRepository, RepositoryConfig, RepositoryError, RepositoryKind, StageConfig,
    WorktreeRecord, WorktreeSnapshot,
};
pub use status::GitStatus;
