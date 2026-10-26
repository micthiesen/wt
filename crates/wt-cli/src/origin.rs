//! One native ref-maintenance boundary shared by UI, events and CLI work.
use anyhow::Result;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use wt_core::Domain;
use wt_runtime::{
    LeafHandle, Persistence, RefreshPolicy, SourceHandle, SourceSpec, SourceState, TaskScope,
    derived_channel, start_source,
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
    refresh_for(ctx, cancellation, "command").await
}

/// Fetch origin, logging what asked for it. The logged duration includes
/// any wait for another fetch or the repository's fetch lock.
pub async fn refresh_for(
    ctx: &AppContext,
    cancellation: &CancellationToken,
    trigger: &str,
) -> Result<FetchOriginReport> {
    let started = std::time::Instant::now();
    let report = ctx
        .repository
        .fetch_origin(options(ctx), cancellation)
        .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let report = match report {
        Ok(report) => report,
        Err(error) => {
            tracing::info!(target: "wt::origin", trigger, elapsed_ms, %error, "origin fetch failed");
            return Err(error.into());
        }
    };
    // The activity feed shows each fetch, as TS did; ref-maintenance
    // warnings join it so they stay visible.
    let text = format!("fetched git origin ({elapsed_ms}ms, {trigger})");
    tracing::info!(target: "wt::origin", trigger, elapsed_ms, event_source = "origin", event_text = %text, "origin fetch");
    for warning in &report.warnings {
        let text = wt_core::sanitize_terminal_text(&warning.to_string());
        tracing::warn!(target: "wt::origin", event_source = "origin", event_text = %text, "Git ref maintenance");
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

/// Fetches have their own lane. A board refresh (`r`) and the hard refresh
/// may request Git/network work; a local title/status write never reaches
/// this owner. The leaf's minimum interval bounds repeated requests.
pub struct OriginSources {
    pub board: SourceHandle<Board>,
    pub origin: Option<LeafHandle<FetchOriginReport>>,
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
    let trigger = Trigger::default();
    let origin = start_source(
        scope,
        SourceSpec {
            name: "origin",
            domain: Domain::Origin,
            persistence: Persistence::Never("maintenance, not display data"),
            policy: RefreshPolicy {
                debounce: Duration::from_millis(50),
                minimum_interval: Duration::from_secs(10),
            },
        },
        {
            let ctx = ctx.clone();
            let trigger = trigger.clone();
            move |cancel| {
                let ctx = ctx.clone();
                let why = trigger.take();
                async move { refresh_for(&ctx, &cancel, why).await }
            }
        },
    );
    let source = spawn_overlay(scope, board, local, origin.clone(), backstop(ctx), trigger);
    OriginSources {
        board: source,
        origin: Some(origin),
    }
}

/// Why the origin lane's next fetch runs. Requests through the leaf handle
/// itself (a Git-affecting action, the hard refresh) leave it unset.
#[derive(Clone, Default)]
struct Trigger(Arc<std::sync::Mutex<Option<&'static str>>>);

impl Trigger {
    fn request(&self, origin: &SourceHandle<FetchOriginReport>, why: &'static str) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(why);
        origin.refresh();
    }

    fn take(&self) -> &'static str {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or("action or hard refresh")
    }
}

/// Publishes the board with origin failures and warnings. Origin is fetched
/// at startup, on the backstop, on a board refresh, and when asked through
/// its leaf handle.
fn spawn_overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    local: SourceHandle<Board>,
    origin: LeafHandle<FetchOriginReport>,
    backstop: Duration,
    trigger: Trigger,
) -> SourceHandle<Board> {
    let (source, mut publisher) = derived_channel(scope, "origin-board");
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut board_updates = board.subscribe();
        let mut origin_updates = origin.subscribe();
        board_updates.mark_changed();
        origin_updates.mark_changed();
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + backstop, backstop);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        trigger.request(&origin, "startup");
        let mut failures = crate::feeds::FailureClock::default();
        loop {
            tokio::select! { biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh(); trigger.request(&origin, "refresh"); continue;
                }
                changed = board_updates.changed() => {
                    if changed.is_err() { break; }
                    board_updates.borrow_and_update();
                }
                changed = origin_updates.changed() => {
                    if changed.is_err() { break; }
                    if matches!(origin_updates.borrow_and_update().state(), SourceState::Ready) { local.refresh(); }
                }
                _ = interval.tick() => { trigger.request(&origin, "backstop"); continue; }
            }
            let snapshot = board_updates.borrow().clone();
            let origin = origin_updates.borrow().clone();
            let mut data = snapshot.data().cloned();
            if let SourceState::Failed(error) = origin.state() {
                if let Some(board) = data.as_mut() {
                    failures.attention(Arc::make_mut(board), "Fetch origin", error);
                }
            } else if let (Some(board), Some(report)) = (data.as_mut(), origin.data()) {
                for warning in &report.warnings {
                    failures.activity(Arc::make_mut(board), "WARN", "Fetch origin", warning);
                }
            }
            if data.is_some() {
                failures.sweep();
            }
            publisher.publish_if_changed(snapshot.with_data(data).with_input(&origin));
        }
    });
    source
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_runtime::{RefreshPolicy, leaf_channel};

    fn spec<T>(name: &'static str, domain: Domain) -> SourceSpec<T> {
        SourceSpec {
            name,
            domain,
            persistence: Persistence::Never("test fixture"),
            policy: RefreshPolicy::default(),
        }
    }

    /// A board refresh rereads the board but does not fetch origin; the
    /// backstop does.
    #[tokio::test(start_paused = true)]
    async fn origin_fetches_name_their_trigger() {
        let scope = TaskScope::new();
        let (board, mut board_requests) = leaf_channel::<Board>(&scope, spec("board", Domain::Git));
        let (local, _local_requests) = leaf_channel::<Board>(&scope, spec("local", Domain::Git));
        let (origin, mut origin_requests) =
            leaf_channel::<FetchOriginReport>(&scope, spec("origin", Domain::Origin));
        let trigger = Trigger::default();
        let backstop = Duration::from_secs(180);
        let output = spawn_overlay(
            &scope,
            board.view(),
            local.view(),
            origin,
            backstop,
            trigger.clone(),
        );
        // Startup fetches once.
        assert_eq!(origin_requests.requested().await, Some(()));
        assert_eq!(trigger.take(), "startup");
        output.refresh();
        assert_eq!(board_requests.requested().await, Some(()));
        assert_eq!(origin_requests.requested().await, Some(()));
        assert_eq!(trigger.take(), "refresh");
        tokio::time::advance(backstop).await;
        assert_eq!(origin_requests.requested().await, Some(()));
        assert_eq!(trigger.take(), "backstop");
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
