use std::{path::PathBuf, sync::Arc};

use tokio_util::sync::CancellationToken;
use wt_config::Config;
use wt_platform::process::ProcessRunner;
use wt_vcs::GitRepository;

use crate::database::Database;

/// Explicit application services shared by CLI handlers and TUI actions.
/// Presentation never owns this context or calls these services during render.
#[derive(Clone)]
pub struct AppContext {
    pub config: Arc<Config>,
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub database: Database,
    pub repository: Arc<GitRepository>,
    pub processes: ProcessRunner,
    pub cancellation: CancellationToken,
}
