//! Read-only Git repository and worktree inventory services.

mod origin;
mod repository;
mod status;

pub use origin::{FetchOriginOptions, FetchOriginReport};
pub use repository::{
    DiffStats, GitRepository, RepositoryConfig, RepositoryError, RepositoryKind, StageConfig,
    WorktreeRecord, WorktreeSnapshot,
};
pub use status::GitStatus;
