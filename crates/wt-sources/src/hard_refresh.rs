//! Confirmed full refresh of native source snapshots.

use std::sync::Arc;

use wt_runtime::SourceHandle;
use wt_tui::Board;
use wt_vcs::FetchOriginReport;

use crate::github_pickers::GithubPickers;
use crate::github_source::GithubRefresh;

#[derive(Clone)]
pub struct HardRefreshCommands {
    board: SourceHandle<Board>,
    github: GithubRefresh,
    origin: Option<SourceHandle<FetchOriginReport>>,
    github_pickers: Arc<GithubPickers>,
}

impl HardRefreshCommands {
    pub fn new(
        board: SourceHandle<Board>,
        github: GithubRefresh,
        origin: Option<SourceHandle<FetchOriginReport>>,
        github_pickers: Arc<GithubPickers>,
    ) -> Self {
        Self {
            board,
            github,
            origin,
            github_pickers,
        }
    }

    /// Ask every active source to reread live state. Durable worktree state,
    /// actions, and automation history stay intact.
    pub async fn refresh(&self) -> Result<(), String> {
        self.github_pickers.invalidate_cache().await;
        self.board.refresh();
        self.github.refresh_live();
        if let Some(origin) = &self.origin {
            origin.refresh();
        }
        Ok(())
    }
}
