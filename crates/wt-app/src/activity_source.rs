//! Manager activity has its own lane: reports never trigger a Git or GitHub scan.
use std::{path::PathBuf, sync::Arc, time::Duration};

use notify::{RecursiveMode, Watcher};
use wt_runtime::{RefreshPolicy, SourceHandle, TaskScope, start_source};

pub fn start(scope: &TaskScope, cache_root: PathBuf) -> SourceHandle<Vec<String>> {
    let directory = cache_root.join("manager");
    let path = Arc::new(directory.join("reports.jsonl"));
    let source = start_source(
        scope,
        RefreshPolicy {
            debounce: Duration::from_millis(50),
            minimum_interval: Duration::from_millis(100),
        },
        move |_| {
            let path = path.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let batch = crate::commands::manager::read_reports_from(&path, 0)?;
                    Ok::<_, anyhow::Error>(
                        batch
                            .reports
                            .iter()
                            .rev()
                            .take(400)
                            .rev()
                            .map(|report| {
                                wt_core::sanitize_terminal_text(&format!(
                                    "{} [manager {:?}] {}",
                                    report.at, report.level, report.text
                                ))
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .await?
            }
        },
    );
    let observed = source.clone();
    let cancellation = scope.token();
    scope.spawn(async move {
        let callback = observed.clone();
        let watcher = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory)?;
            let mut watcher = notify::recommended_watcher(
                move |event: notify::Result<notify::Event>| match event {
                    Ok(event)
                        if !matches!(event.kind, notify::EventKind::Access(_))
                            && event.paths.iter().any(|path| {
                                path.file_name().is_some_and(|name| name == "reports.jsonl")
                            }) =>
                    {
                        callback.refresh();
                    }
                    Err(error) => {
                        tracing::warn!(%error, "manager report notification failed");
                        callback.refresh();
                    }
                    _ => {}
                },
            )?;
            watcher.watch(&directory, RecursiveMode::NonRecursive)?;
            Ok::<_, anyhow::Error>(watcher)
        })
        .await;
        // Retain the watcher for the task lifetime; the backstop also recovers
        // from a removed/recreated directory or unavailable OS watcher.
        let _watcher = match watcher {
            Ok(Ok(watcher)) => Some(watcher),
            error => {
                tracing::warn!(?error, "manager reports use refresh backstop");
                None
            }
        };
        observed.refresh();
        let period = Duration::from_secs(30);
        let mut backstop = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        backstop.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = backstop.tick() => { observed.refresh(); },
            }
        }
    });
    source
}
