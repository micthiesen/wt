//! Confirmed full refresh of native source snapshots and derived naming data.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use wt_github::GithubData;
use wt_runtime::SourceHandle;
use wt_tui::Board;
use wt_vcs::FetchOriginReport;

use crate::github_pickers::GithubPickers;
use crate::naming_source::NamingCommands;

#[derive(Clone)]
pub struct HardRefreshCommands {
    board: SourceHandle<Board>,
    github: SourceHandle<GithubData>,
    origin: Option<SourceHandle<FetchOriginReport>>,
    bypass_github_cache: Arc<AtomicBool>,
    github_pickers: Arc<GithubPickers>,
    naming: NamingCommands,
}

impl HardRefreshCommands {
    pub fn new(
        board: SourceHandle<Board>,
        github: SourceHandle<GithubData>,
        origin: Option<SourceHandle<FetchOriginReport>>,
        bypass_github_cache: Arc<AtomicBool>,
        naming: NamingCommands,
        github_pickers: Arc<GithubPickers>,
    ) -> Self {
        Self {
            board,
            github,
            origin,
            bypass_github_cache,
            github_pickers,
            naming,
        }
    }

    /// Clear derived naming data before asking every active source to reread
    /// live state. Durable worktree state, actions, and automation history stay intact.
    pub async fn refresh(&self) -> Result<(), String> {
        self.naming.invalidate_derived().await?;
        self.github_pickers.invalidate_cache().await;
        self.bypass_github_cache.store(true, Ordering::Release);
        self.board.refresh();
        self.github.refresh();
        if let Some(origin) = &self.origin {
            origin.refresh();
        }
        Ok(())
    }
}
