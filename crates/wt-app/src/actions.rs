//! Application composition for durable native action workers.
use anyhow::{Context, Result};
use wt_actions::{ActionService, ActionServiceConfig};
use wt_tmux::{TmuxClient, TmuxServer};

use crate::context::AppContext;

pub fn service(context: &AppContext) -> Result<ActionService> {
    Ok(ActionService::new(ActionServiceConfig {
        log_dir: context.config.paths.log_dir.clone(),
        lock_dir: context.config.paths.lock_dir.clone(),
        executable: std::env::current_exe().context("find native action worker executable")?,
        runner: context.processes.clone(),
        tmux: TmuxClient::new(
            context.processes.clone(),
            TmuxServer::named(context.config.tmux.socket.clone()).with_cwd(&context.home),
        ),
    }))
}
