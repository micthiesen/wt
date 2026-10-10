use anyhow::{Context, Result, bail};
use wt_dev::{DevServerConfig, DevServerService, StateConfig};
use wt_store::RepositoryIdentity;
use wt_tmux::{TmuxClient, TmuxServer};

use crate::context::AppContext;

/// Build the configured native dev-server service. Dev data is repository
/// scoped below the configured cache root; state stays in wtstate.
pub fn service(context: &AppContext) -> Result<DevServerService> {
    let settings = context
        .config
        .dev_server
        .clone()
        .context("[dev_server] is not configured")?;
    if settings.command.trim().is_empty() {
        bail!("[dev_server].command is empty");
    }
    let server = TmuxServer::named(context.config.tmux.socket.clone()).with_cwd(&context.home);
    let processes = context.processes.clone();
    Ok(DevServerService::new(
        DevServerConfig {
            main_clone: context.config.paths.main_clone.clone(),
            dev_dir: context.config.paths.cache_root.join("dev"),
            lock_dir: context.config.paths.lock_dir.clone(),
            state: StateConfig {
                path: context.config.paths.state_db.clone(),
                identity: RepositoryIdentity::new(
                    &context.config.repo_id,
                    context.config.repo_path.to_string_lossy(),
                ),
            },
            settings,
            tmux: TmuxClient::new(processes.clone(), server),
            executable: std::env::current_exe().context("find native wt executable")?,
            config_selector: context.config.repository_config.clone(),
            home: context.home.clone(),
        },
        (*context.repository).clone(),
        processes,
    ))
}
