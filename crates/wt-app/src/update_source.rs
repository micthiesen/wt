//! A quiet "newer wt release" notice for the title bar. Read-only: it never
//! stamps the install state, so the startup prompt keeps its own daily
//! cadence, and it never downloads anything.

use std::{sync::Arc, time::Duration};

use wt_core::Domain;
use wt_runtime::{
    Persistence, SourceHandle, SourceSpec, Stamp, TaskScope, derived_channel, leaf_channel,
};
use wt_tui::Board;
use wt_update::{ReleaseSource, StateStore};

use crate::context::AppContext;

const FIRST_CHECK: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

/// `None` when this build is current, the check is disabled, or it failed;
/// a failed check shows nothing rather than a stale or alarming notice.
pub type UpdateNotice = Option<String>;

pub fn overlay(
    scope: &TaskScope,
    context: &AppContext,
    board: SourceHandle<Board>,
) -> SourceHandle<Board> {
    let (notice, publisher) = leaf_channel(
        scope,
        SourceSpec {
            name: "update-check",
            // Perf is the domain the refresh motion ignores: a background
            // release check must never animate the title bar.
            domain: Domain::Perf,
            persistence: Persistence::Never("network release check; repeats every 6 hours"),
            policy: Default::default(),
        },
    );
    publisher.publish_live(None, Stamp::now());
    if enabled(context) {
        let cancellation = scope.token();
        scope.spawn(async move {
            let mut wait = FIRST_CHECK;
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = tokio::time::sleep(wait) => {}
                }
                let checked = tokio::select! {
                    _ = cancellation.cancelled() => break,
                    result = tokio::time::timeout(CHECK_TIMEOUT, check()) => result,
                };
                match checked {
                    Ok(Ok(notice)) => publisher.publish_live(notice, Stamp::now()),
                    Ok(Err(error)) => tracing::debug!(%error, "update check unavailable"),
                    Err(_) => tracing::debug!("update check timed out"),
                }
                wait = CHECK_EVERY;
            }
        });
    }
    let (output, mut board_publisher) = derived_channel(scope, "update-board");
    let cancellation = scope.token();
    let notice_view = notice.view();
    scope.spawn(async move {
        let mut boards = board.subscribe();
        let mut notices = notice_view.subscribe();
        boards.mark_changed();
        notices.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = board_publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    continue;
                }
                changed = boards.changed() => if changed.is_err() { break; },
                changed = notices.changed() => if changed.is_err() { break; },
            }
            let base = boards.borrow_and_update().clone();
            let latest = notices.borrow_and_update().clone();
            let data = base.data().map(|board| {
                let mut board = board.as_ref().clone();
                board.update_notice = latest.data().and_then(|notice| (**notice).clone());
                Arc::new(board)
            });
            board_publisher.publish_if_changed(base.with_data(data).with_input(&latest));
        }
    });
    output
}

/// Workers run the build their controller installs, and `WT_UPDATE=off`
/// disables release checks entirely.
fn enabled(context: &AppContext) -> bool {
    context.config.instance.role != wt_config::InstanceRole::Worker
        && std::env::var("WT_UPDATE").as_deref() != Ok("off")
}

async fn check() -> anyhow::Result<UpdateNotice> {
    let options = wt_config::LoadOptions::default();
    let paths = crate::updates::install_paths(&options)?;
    let store = StateStore::new(paths);
    let state = tokio::task::spawn_blocking(move || store.load()).await??;
    // Not a native install (a source build run directly): nothing to offer.
    let Some(current) = state.current else {
        return Ok(None);
    };
    let release = ReleaseSource::new(crate::updates::repository(&options)?)?
        .latest(state.channel)
        .await?;
    let latest = release.version_id(env!("WT_TARGET"))?;
    Ok(notice(&latest, &current, env!("WT_BUILD_ID")))
}

/// The running build may be older than the installed one: an update already
/// installed takes effect when wt restarts.
fn notice(
    latest: &wt_update::VersionId,
    installed: &wt_update::VersionId,
    running_build: &str,
) -> UpdateNotice {
    if latest.build_id() == running_build {
        None
    } else if latest == installed {
        Some(format!("restart for {}", latest.release_version()))
    } else {
        Some(format!("{} available", latest.release_version()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(release: &str, build: &str) -> wt_update::VersionId {
        wt_update::VersionId::new(release, build, "aarch64-apple-darwin").unwrap()
    }

    #[test]
    fn notice_names_a_newer_release_or_a_pending_restart() {
        let old = version("v0.1.30", "aaa");
        let new = version("v0.1.31", "bbb");
        // Running the latest build: nothing to say.
        assert_eq!(notice(&new, &new, "bbb"), None);
        // A newer release that is not installed yet.
        assert_eq!(
            notice(&new, &old, "aaa").as_deref(),
            Some("v0.1.31 available")
        );
        // Installed by `wt update` but this process still runs the old build.
        assert_eq!(
            notice(&new, &new, "aaa").as_deref(),
            Some("restart for v0.1.31")
        );
    }
}
