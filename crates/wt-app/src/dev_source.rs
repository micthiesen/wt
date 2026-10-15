//! Dev-server events have a separate, batched lane. They never invalidate Git,
//! GitHub, or run the potentially expensive user health command.
use std::{path::PathBuf, sync::Arc, time::Duration};

use notify::{RecursiveMode, Watcher};
use wt_core::Domain;
use wt_dev::{DevServerStatus, DevStatusRow, DevWorktree};
use wt_runtime::{
    DiskCache, LeafHandle, Persistence, RefreshPolicy, SourceHandle, SourceSnapshot, SourceSpec,
    SourceState, TaskScope, derived_channel, start_source,
};
use wt_tui::Board;

use crate::context::AppContext;

const BACKSTOP: Duration = Duration::from_secs(15);

pub struct DevSources {
    pub board: SourceHandle<Board>,
    pub status: Option<LeafHandle<Vec<DevStatusRow>>>,
}

pub fn overlay(
    scope: &TaskScope,
    context: &AppContext,
    local: SourceHandle<Board>,
    board: SourceHandle<Board>,
) -> DevSources {
    if context.config.dev_server.is_none() {
        return DevSources {
            board,
            status: None,
        };
    }
    let dev = start_source(scope, status_spec(context), {
        let context = context.clone();
        let local = local.clone();
        move |cancel| {
            let context = context.clone();
            let rows = worktrees(&local.snapshot());
            async move {
                Ok::<_, anyhow::Error>(
                    crate::dev::service(&context)?
                        .status_all(&rows, &cancel)
                        .await?
                        .worktrees,
                )
            }
        }
    });
    watch(
        scope,
        context.config.paths.cache_root.join("dev"),
        dev.view(),
    );
    DevSources {
        board: project(scope, local, board, dev.view()),
        status: Some(dev),
    }
}

/// Last known dev-server status survives restarts as restored display data.
fn status_spec(context: &AppContext) -> SourceSpec<Vec<DevStatusRow>> {
    SourceSpec {
        name: "dev-status",
        domain: Domain::Dev,
        persistence: Persistence::Disk(DiskCache::new(&context.caches)),
        policy: RefreshPolicy {
            debounce: Duration::from_millis(50),
            minimum_interval: Duration::from_millis(500),
        },
    }
}

fn worktrees(snapshot: &SourceSnapshot<Board>) -> Vec<DevWorktree> {
    snapshot
        .data()
        .into_iter()
        .flat_map(|board| &board.rows)
        .map(|row| DevWorktree {
            slug: row.slug.clone(),
            path: PathBuf::from(&row.path),
            branch: row.branch.clone(),
        })
        .collect()
}

fn project(
    scope: &TaskScope,
    local: SourceHandle<Board>,
    board: SourceHandle<Board>,
    dev: SourceHandle<Vec<DevStatusRow>>,
) -> SourceHandle<Board> {
    let (source, mut publisher) = derived_channel(scope, "dev-board");
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut local_updates = local.subscribe();
        let mut board_updates = board.subscribe();
        let mut dev_updates = dev.subscribe();
        let mut inventory = Vec::new();
        // The upstream may already have published before this task subscribed.
        local_updates.mark_changed();
        board_updates.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    dev.refresh();
                    continue;
                }
                changed = local_updates.changed() => {
                    if changed.is_err() { break; }
                    let next = worktrees(&local_updates.borrow_and_update()).into_iter()
                        .map(|row| (row.slug, row.path, row.branch)).collect::<Vec<_>>();
                    if next != inventory {
                        inventory = next;
                        dev.refresh();
                    }
                    continue;
                }
                changed = board_updates.changed() => {
                    if changed.is_err() { break; }
                    board_updates.borrow_and_update();
                }
                changed = dev_updates.changed() => {
                    if changed.is_err() { break; }
                    dev_updates.borrow_and_update();
                }
            }
            publisher.publish_if_changed(compose(
                board_updates.borrow().clone(),
                dev_updates.borrow().clone(),
            ));
        }
    });
    source
}

fn watch(scope: &TaskScope, directory: PathBuf, source: SourceHandle<Vec<DevStatusRow>>) {
    let cancellation = scope.token();
    scope.spawn(async move {
        let callback = source.clone();
        let watcher = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory)?;
            let mut watcher =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    match event {
                        Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => {}
                        // Streaming log output must not turn into status polling.
                        Ok(event)
                            if event
                                .paths
                                .iter()
                                .any(|path| path.extension().is_some_and(|ext| ext == "log")) => {}
                        _ => {
                            callback.refresh();
                        }
                    }
                })?;
            watcher.watch(&directory, RecursiveMode::Recursive)?;
            Ok::<_, anyhow::Error>(watcher)
        })
        .await;
        if !matches!(watcher, Ok(Ok(_))) {
            tracing::warn!(
                ?watcher,
                "dev status watcher unavailable; using refresh backstop"
            );
        }
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + BACKSTOP, BACKSTOP);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                _ = interval.tick() => { source.refresh(); }
            }
        }
        crate::watcher_cleanup::drop_watcher(watcher).await;
    });
}

fn compose(
    board: SourceSnapshot<Board>,
    dev: SourceSnapshot<Vec<DevStatusRow>>,
) -> SourceSnapshot<Board> {
    let Some(data) = board.data() else {
        return board;
    };
    let mut data = data.as_ref().clone();
    let error = match dev.state() {
        SourceState::Failed(error) => Some(error.as_ref()),
        _ => None,
    };
    let mut contributes = false;
    for row in &mut data.rows {
        if wt_core::is_remote_worktree_ledger_key(&row.key) {
            continue;
        }
        // An empty successful inventory still proves no local dev status.
        contributes |= dev.data().is_some() || error.is_some();
        let status = dev
            .data()
            .and_then(|rows| rows.iter().find(|status| status.slug == row.slug));
        if let Some(status) = status.and_then(|row| row.status.as_ref()) {
            row.dev_status = Some(label(status));
            row.environment_live |= status.running || status.starting;
            row.dev_url = status.url.as_deref().map(wt_core::sanitize_terminal_text);
        }
        if let Some(error) = error.or_else(|| status.and_then(|row| row.error.as_deref())) {
            row.details
                .push(format!("Dev: {}", wt_core::sanitize_terminal_text(error)));
        }
    }
    let result = board.with_data(Some(Arc::new(data)));
    if contributes {
        result.with_input(&dev)
    } else {
        result
    }
}

fn label(status: &DevServerStatus) -> String {
    let mut text = if status.crashed {
        "crashed".to_owned()
    } else if status.starting {
        "starting".to_owned()
    } else if status.running {
        "running".to_owned()
    } else if let Some(waiting) = status.waiting {
        format!("queued #{}", waiting.rank + 1)
    } else {
        "not running".to_owned()
    };
    if (status.running || status.starting)
        && let Some(url) = &status.url
    {
        text.push_str(&format!(" · {}", wt_core::sanitize_terminal_text(url)));
    }
    if status.rebased_since == Some(true) {
        text.push_str(" · needs reset after rebase");
    }
    if let Some(restarts) = &status.restarts {
        text.push_str(&format!(
            " · {} restarts (exit {})",
            restarts.count, restarts.last_exit
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_runtime::{Stamp, leaf_channel};
    use wt_tui::BoardRow;

    fn snapshot<T>(data: T, domain: Domain) -> SourceSnapshot<T> {
        SourceSnapshot::test_live(Some(data), SourceState::Ready, domain)
    }

    fn test_spec<T>(domain: Domain) -> SourceSpec<T> {
        SourceSpec {
            name: "test",
            domain,
            persistence: Persistence::Never("test"),
            policy: RefreshPolicy::default(),
        }
    }

    fn board() -> Board {
        Board {
            rows: vec![BoardRow {
                key: "one".into(),
                slug: "one".into(),
                path: "/tmp/one".into(),
                branch: "feature".into(),
                title: "Original".into(),
                ..BoardRow::default()
            }],
            ..Board::default()
        }
    }

    fn dev() -> Vec<DevStatusRow> {
        vec![DevStatusRow {
            slug: "one".into(),
            status: Some(DevServerStatus {
                running: true,
                starting: false,
                crashed: false,
                port: Some(3000),
                url: Some("http://localhost:3000".into()),
                since: None,
                waiting: None,
                rebased_since: Some(true),
                restarts: None,
            }),
            error: None,
        }]
    }

    #[test]
    fn unused_restored_dev_inventory_does_not_mark_remote_or_empty_boards_restored() {
        let restored = SourceSnapshot::test_restored(dev(), Domain::Dev, 1);
        let mut remote = board();
        remote.rows[0].key = wt_core::remote_worktree_ledger_key("remote", "one");
        for base in [remote, Board::default()] {
            for status in [
                restored.clone(),
                restored
                    .clone()
                    .with_state(SourceState::Failed("local tmux failed".into())),
            ] {
                let result = compose(snapshot(base.clone(), Domain::Git), status);
                assert!(!result.is_restored());
                assert!(!result.domains().contains_key(&Domain::Dev));
                assert_eq!(result.data().unwrap().rows, base.rows);
            }
        }
        assert!(compose(snapshot(board(), Domain::Git), restored).is_restored());
        // A known empty inventory still contributes a negative assertion for
        // local rows, so it must retain its restored provenance.
        let empty = SourceSnapshot::test_restored(Vec::new(), Domain::Dev, 1);
        assert!(compose(snapshot(board(), Domain::Git), empty).is_restored());
    }

    #[test]
    fn failed_dev_fetch_keeps_last_good_status_visible_and_reports_the_error() {
        let status =
            snapshot(dev(), Domain::Dev).with_state(SourceState::Failed("tmux unavailable".into()));
        let result = compose(snapshot(board(), Domain::Git), status);
        assert!(result.domains().contains_key(&Domain::Dev));
        assert!(result.domains().contains_key(&Domain::Git));
        let row = &result.data().unwrap().rows[0];
        assert_eq!(row.dev_url.as_deref(), Some("http://localhost:3000"));
        assert!(
            row.dev_status
                .as_deref()
                .is_some_and(|status| status.contains("needs reset after rebase"))
        );
        assert!(
            row.details
                .iter()
                .any(|line| line == "Dev: tmux unavailable")
        );
        assert!(!row.details.iter().any(|line| line.contains("not running")));
    }

    #[tokio::test(start_paused = true)]
    async fn dev_updates_do_not_refresh_upstream_and_title_updates_do_not_refetch_dev() {
        let scope = TaskScope::new();
        let (local, mut local_writer) = derived_channel(&scope, "test");
        let (upstream, mut upstream_writer) = derived_channel(&scope, "test");
        let (status, mut status_writer) = leaf_channel(&scope, test_spec(Domain::Dev));
        // Exercise startup after the input already contains data.
        local_writer.publish(snapshot(board(), Domain::Git));
        upstream_writer.publish(snapshot(board(), Domain::Git));
        let output = project(&scope, local, upstream, status.view());
        assert_eq!(status_writer.requested().await, Some(()));
        status_writer.publish_live(dev(), Stamp::now());
        tokio::time::timeout(
            Duration::from_secs(1),
            output
                .subscribe()
                .wait_for(|s| s.data().is_some_and(|b| b.rows[0].dev_url.is_some())),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1), local_writer.requested())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(1), upstream_writer.requested())
                .await
                .is_err()
        );
        let mut renamed = board();
        renamed.rows[0].title = "Renamed".into();
        local_writer.publish(snapshot(renamed.clone(), Domain::Git));
        upstream_writer.publish(snapshot(renamed, Domain::Git));
        tokio::time::timeout(
            Duration::from_secs(1),
            output
                .subscribe()
                .wait_for(|s| s.data().is_some_and(|b| b.rows[0].title == "Renamed")),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1), status_writer.requested())
                .await
                .is_err()
        );
        output.refresh();
        assert_eq!(upstream_writer.requested().await, Some(()));
        assert_eq!(status_writer.requested().await, Some(()));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn dev_status_meets_the_persisted_source_contract() {
        let mut rows = dev();
        rows[0].status.as_mut().unwrap().restarts = Some(wt_dev::RestartStatus {
            count: 2,
            last_exit: 137,
        });
        rows.push(DevStatusRow {
            slug: "two".into(),
            status: Some(DevServerStatus {
                waiting: Some(wt_dev::WaitingStatus {
                    rank: 0,
                    since: 1_700_000_000_000.0,
                }),
                ..DevServerStatus::default()
            }),
            error: None,
        });
        rows.push(DevStatusRow {
            slug: "three".into(),
            status: None,
            error: Some("tmux unavailable".into()),
        });
        wt_runtime::conformance::persisted_source("dev-status", Domain::Dev, rows).await
    }
}
