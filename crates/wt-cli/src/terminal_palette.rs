//! Startup palette observation and persistence for the app-owned TUI.

use std::{io, time::Duration};

use tokio_util::sync::CancellationToken;
use wt_runtime::{SourceHandle, TaskScope};
use wt_tmux::{TmuxClient, TmuxServer, write_terminal_palette_config};
use wt_tui::{Board, UiActions};

use crate::context::AppContext;

pub async fn run(
    scope: &TaskScope,
    source: SourceHandle<Board>,
    actions: UiActions,
    cancel: CancellationToken,
    context: &AppContext,
) -> io::Result<()> {
    let cache_root = context.config.paths.cache_root.clone();
    let home = context.home.clone();
    let config_path = cache_root.join("tmux-palette.conf");
    let tmux = TmuxClient::new(
        context.processes.clone(),
        TmuxServer::named(context.config.tmux.socket.clone())
            .with_cwd(&home)
            .with_config_file(config_path),
    );
    let palette_cancel = cancel.clone();
    wt_tui::run_with_palette_probe(source, actions, cancel, move |observation| async move {
        if let Some((foreground, background)) = observation {
            let save_root = cache_root.clone();
            let save_home = home.clone();
            let save_foreground = foreground.clone();
            let save_background = background.clone();
            let persisted = tokio::task::spawn_blocking(move || {
                let saved = wt_tmux::save_terminal_palette(
                    &save_root,
                    &save_foreground,
                    &save_background,
                )?;
                if saved {
                    write_terminal_palette_config(&save_root, &save_home)?;
                }
                Ok::<_, io::Error>(saved)
            })
            .await;
            match persisted {
                Ok(Ok(true)) => {
                    scope.spawn(async move {
                        match tokio::time::timeout(
                            Duration::from_secs(1),
                            tmux.apply_terminal_palette(
                                &foreground,
                                &background,
                                &palette_cancel,
                            ),
                        )
                        .await
                        {
                            Ok(Ok(_)) => {}
                            Ok(Err(error)) => tracing::debug!(%error, "could not apply palette to an existing tmux server"),
                            Err(_) => tracing::debug!("timed out applying palette to an existing tmux server"),
                        }
                    });
                }
                Ok(Ok(false)) => {
                    tracing::debug!("terminal returned an unsupported palette; retaining cached colors");
                }
                Ok(Err(error)) => tracing::warn!(%error, "could not persist terminal palette"),
                Err(error) => tracing::warn!(%error, "terminal palette persistence task failed"),
            }
        }
    })
    .await
}
