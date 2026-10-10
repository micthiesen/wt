//! One native ref-maintenance boundary shared by UI, events and CLI work.
use anyhow::Result;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use wt_runtime::{
    RefreshPolicy, SourceHandle, SourceState, TaskScope, source_channel, start_source,
};
use wt_tui::Board;
use wt_vcs::{FetchOriginOptions, FetchOriginReport};

use crate::context::AppContext;

pub fn options(ctx: &AppContext) -> FetchOriginOptions {
    FetchOriginOptions {
        keep_fresh: ctx.config.branch.keep_fresh.clone(),
        auto_regen_paths: ctx
            .config
            .sst
            .as_ref()
            .map(|sst| sst.auto_regen_paths.clone())
            .unwrap_or_default(),
        sync_install: Some(wt_platform::install::InstallPolicy {
            command: ctx.config.lifecycle.install_command.clone(),
            shell: std::env::var_os("SHELL")
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "bash".into()),
        }),
    }
}

pub async fn refresh(
    ctx: &AppContext,
    cancellation: &CancellationToken,
) -> Result<FetchOriginReport> {
    let report = ctx
        .repository
        .fetch_origin(options(ctx), cancellation)
        .await?;
    for warning in &report.warnings {
        tracing::warn!(%warning, "Git ref maintenance");
    }
    Ok(report)
}

pub fn backstop(ctx: &AppContext) -> Duration {
    ctx.config
        .github
        .events
        .as_ref()
        .map(|events| {
            Duration::from_millis(events.backstop_poll_ms.clamp(1000.0, 86_400_000.0) as u64)
        })
        .unwrap_or(Duration::from_secs(180))
}

/// Fetches have their own lane. A keyboard refresh may request Git/network
/// work, but a local title/status write never reaches this owner.
pub struct OriginSources {
    pub board: SourceHandle<Board>,
    pub origin: Option<SourceHandle<FetchOriginReport>>,
}

pub fn overlay(
    scope: &TaskScope,
    ctx: &AppContext,
    board: SourceHandle<Board>,
    local: SourceHandle<Board>,
) -> OriginSources {
    if std::env::var("WT_FETCH_ORIGIN").as_deref() == Ok("off") {
        return OriginSources {
            board,
            origin: None,
        };
    }
    let origin = start_source(
        scope,
        RefreshPolicy {
            debounce: Duration::from_millis(50),
            minimum_interval: Duration::from_secs(10),
        },
        {
            let ctx = ctx.clone();
            move |cancel| {
                let ctx = ctx.clone();
                async move { refresh(&ctx, &cancel).await }
            }
        },
    );
    let (source, mut publisher) = source_channel();
    let cancellation = scope.token();
    let refresh = origin.clone();
    let backstop = backstop(ctx);
    scope.spawn(async move {
        let mut board_updates = board.subscribe();
        let mut origin_updates = origin.subscribe();
        board_updates.mark_changed();
        origin_updates.mark_changed();
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + backstop, backstop);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        origin.refresh();
        loop {
            tokio::select! { biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh(); origin.refresh(); continue;
                }
                changed = board_updates.changed() => {
                    if changed.is_err() { break; }
                    board_updates.borrow_and_update();
                }
                changed = origin_updates.changed() => {
                    if changed.is_err() { break; }
                    if matches!(origin_updates.borrow_and_update().state, SourceState::Ready) { local.refresh(); }
                }
                _ = interval.tick() => { origin.refresh(); continue; }
            }
            let mut snapshot = board_updates.borrow().clone();
            let origin = origin_updates.borrow().clone();
            if let SourceState::Failed(error) = origin.state {
                if let Some(board) = snapshot.data.as_mut() {
                    Arc::make_mut(board).activity.push(wt_core::sanitize_terminal_text(&format!("Fetch origin: {error}")));
                }
            } else if let (Some(board), Some(report)) = (snapshot.data.as_mut(), origin.data) {
                Arc::make_mut(board).activity.extend(report.warnings.iter().map(|warning| wt_core::sanitize_terminal_text(warning)));
            }
            publisher.publish(snapshot);
        }
    });
    OriginSources {
        board: source,
        origin: Some(refresh),
    }
}
