//! Read-only Git repository and worktree inventory services.

mod mainline;
mod origin;
mod repository;
mod status;

pub use mainline::{
    MAINLINE_WALK, branch_reflog_args, committed_on_branch, contained_head_landed, mainline_args,
    on_first_parent_line,
};
pub use origin::{FetchOriginOptions, FetchOriginReport};
pub use repository::{
    DiffStats, GitRepository, RepositoryConfig, RepositoryError, RepositoryKind, StageConfig,
    WorktreeRecord, WorktreeSnapshot, operation_in_progress, stack_parent_refs,
};
pub use status::GitStatus;
