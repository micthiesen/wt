//! Confirmed full refresh of native source snapshots and derived naming data.

use std::sync::Arc;

use wt_runtime::SourceHandle;
use wt_tui::Board;
use wt_vcs::FetchOriginReport;

use crate::github_pickers::GithubPickers;
use crate::github_source::GithubRefresh;
use crate::naming_source::NamingCommands;

#[derive(Clone)]
pub struct HardRefreshCommands {
    board: SourceHandle<Board>,
    github: GithubRefresh,
    origin: Option<SourceHandle<FetchOriginReport>>,
    github_pickers: Arc<GithubPickers>,
    naming: NamingCommands,
}

impl HardRefreshCommands {
    pub fn new(
        board: SourceHandle<Board>,
        github: GithubRefresh,
        origin: Option<SourceHandle<FetchOriginReport>>,
        naming: NamingCommands,
        github_pickers: Arc<GithubPickers>,
    ) -> Self {
        Self {
            board,
            github,
            origin,
            github_pickers,
            naming,
        }
    }

    /// Clear derived naming data before asking every active source to reread
    /// live state. Durable worktree state, actions, and automation history stay intact.
    pub async fn refresh(&self) -> Result<(), String> {
        self.naming.invalidate_derived().await?;
        self.github_pickers.invalidate_cache().await;
        self.board.refresh();
        self.github.refresh_live();
        if let Some(origin) = &self.origin {
            origin.refresh();
        }
        Ok(())
    }
}
