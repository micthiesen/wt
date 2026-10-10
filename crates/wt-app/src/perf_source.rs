//! Dormant, prepared process snapshots for the TUI performance overlay.

use std::{future::Future, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel};
use wt_tui::Board;

use crate::context::AppContext;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

pub struct PerfSources {
    pub board: SourceHandle<Board>,
    pub commands: PerfCommands,
}

#[derive(Clone)]
pub struct PerfCommands {
    controls: watch::Sender<Controls>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Controls {
    active: bool,
    continuous: bool,
    refresh: u64,
}

impl PerfCommands {
    /// Start one sample when opening the overlay; closing it suspends all work.
    pub fn set_active(&self, active: bool) {
        self.controls.send_if_modified(|controls| {
            if controls.active == active {
                return false;
            }
            controls.active = active;
            true
        });
    }

    /// Enable or disable two-second sampling while the overlay is open.
    pub fn set_continuous(&self, continuous: bool) {
        self.controls.send_if_modified(|controls| {
            if controls.continuous == continuous {
                return false;
            }
            controls.continuous = continuous;
            true
        });
    }

    /// Request a fresh sample. Requests while closed are intentionally ignored.
    pub fn refresh(&self) {
        self.controls.send_if_modified(|controls| {
            if !controls.active {
                return false;
            }
            controls.refresh = controls.refresh.wrapping_add(1);
            true
        });
    }
}

pub fn start(scope: &TaskScope, context: &AppContext, board: SourceHandle<Board>) -> PerfSources {
    let context = context.clone();
    start_with_sampler(scope, board, move |cancellation| {
        let mut context = context.clone();
        context.cancellation = cancellation;
        async move {
            let snapshot = crate::commands::perf::snapshot(&context, true).await?;
            Ok(format_snapshot(&snapshot))
        }
    })
}

fn start_with_sampler<F, Fut>(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    sampler: F,
) -> PerfSources
where
    F: Fn(CancellationToken) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<Vec<String>>> + Send + 'static,
{
    let (perf, perf_publisher) = source_channel::<Vec<String>>();
    let (control_tx, mut control_rx) = watch::channel(Controls::default());
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut controls = *control_rx.borrow_and_update();
        let mut latest: Option<Arc<Vec<String>>> = None;
        let mut should_sample = controls.active;
        loop {
            if cancellation.is_cancelled() {
                break;
            }
            if !controls.active || !should_sample {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    changed = control_rx.changed() => {
                        if changed.is_err() { break; }
                        let previous = controls;
                        controls = *control_rx.borrow_and_update();
                        should_sample = controls.active
                            && (!previous.active
                                || controls.refresh != previous.refresh
                                || (!previous.continuous && controls.continuous));
                    }
                }
                continue;
            }

            let sample_token = cancellation.child_token();
            let sample = sampler.clone()(sample_token.clone());
            tokio::pin!(sample);
            enum SampleResult<T> {
                Shutdown,
                Restart,
                Completed(T),
            }
            let sampled = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    sample_token.cancel();
                    SampleResult::Shutdown
                }
                changed = control_rx.changed() => {
                    sample_token.cancel();
                    if changed.is_err() {
                        SampleResult::Shutdown
                    } else {
                        let previous = controls;
                        controls = *control_rx.borrow_and_update();
                        should_sample = controls.active
                            && (!previous.active
                                || controls.refresh != previous.refresh
                                || (!previous.continuous && controls.continuous));
                        SampleResult::Restart
                    }
                }
                result = &mut sample => SampleResult::Completed(result),
            };
            let sampled = match sampled {
                SampleResult::Shutdown => {
                    let _ = sample.await;
                    break;
                }
                SampleResult::Restart => {
                    // ProcessRunner observes the child cancellation and reaps
                    // the subprocess before the next control state is handled.
                    let _ = sample.await;
                    continue;
                }
                SampleResult::Completed(result) => result,
            };
            if !controls.active {
                should_sample = false;
                continue;
            }

            let now = tokio::time::Instant::now();
            let (data, state) = match sampled {
                Ok(lines) => {
                    let data = Arc::new(lines);
                    latest = Some(data.clone());
                    (Some(data), SourceState::Ready)
                }
                Err(error) => {
                    let error_line = format!("Performance snapshot failed: {error:#}");
                    let mut lines = latest
                        .as_deref()
                        .map_or_else(Vec::new, |data| data.to_vec());
                    lines.retain(|line| !line.starts_with("Performance snapshot failed:"));
                    lines.push(error_line.clone());
                    let data = Arc::new(lines);
                    (Some(data), SourceState::Failed(Arc::from(error_line)))
                }
            };
            perf_publisher.publish(SourceSnapshot {
                data,
                state,
                updated_at: Some(now),
                revision: 0,
            });
            should_sample = false;

            if !controls.active || !controls.continuous {
                continue;
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                changed = control_rx.changed() => {
                    if changed.is_err() { break; }
                    let previous = controls;
                    controls = *control_rx.borrow_and_update();
                    should_sample = controls.active
                        && (!previous.active
                            || controls.refresh != previous.refresh
                            || (!previous.continuous && controls.continuous));
                }
                _ = tokio::time::sleep(SAMPLE_INTERVAL) => should_sample = true,
            }
        }
    });

    let output = overlay(scope, board, perf);
    PerfSources {
        board: output,
        commands: PerfCommands {
            controls: control_tx,
        },
    }
}

fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    perf: SourceHandle<Vec<String>>,
) -> SourceHandle<Board> {
    let (output, mut publisher) = source_channel();
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut board_updates = board.subscribe();
        let mut perf_updates = perf.subscribe();
        let mut previous: Option<(Board, SourceState)> = None;
        board_updates.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    board_updates.mark_changed();
                    perf_updates.mark_changed();
                }
                changed = board_updates.changed() => {
                    if changed.is_err() { break; }
                    board_updates.borrow_and_update();
                }
                changed = perf_updates.changed() => {
                    if changed.is_err() { break; }
                    perf_updates.borrow_and_update();
                }
            }
            let base = board_updates.borrow().clone();
            let perf_snapshot = perf_updates.borrow().clone();
            let mut projected = base.data.as_deref().cloned().unwrap_or_default();
            projected.perf = perf_snapshot
                .data
                .as_deref()
                .map_or_else(Vec::new, Clone::clone);
            let state = base.state.clone();
            let key = (projected.clone(), state.clone());
            if previous.as_ref() == Some(&key) {
                continue;
            }
            previous = Some(key);
            publisher.publish(SourceSnapshot {
                data: Some(Arc::new(projected)),
                state,
                updated_at: base.updated_at,
                revision: 0,
            });
        }
    });
    output
}

fn format_snapshot(snapshot: &crate::commands::perf::Snapshot) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "Sampled at {} UTC",
        time::OffsetDateTime::from_unix_timestamp_nanos(
            i128::try_from(snapshot.sampled_at_ms).unwrap_or(i128::MAX) * 1_000_000
        )
        .map(|time| time
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default())
        .unwrap_or_else(|_| "unknown time".into())
    ));
    if let Some(load) = snapshot.load_average {
        lines.push(format!(
            "Load {:.2} / {:.2} / {:.2}    system CPU {:.0}%",
            load[0], load[1], load[2], snapshot.system_cpu
        ));
    } else {
        lines.push(format!("System CPU {:.0}%", snapshot.system_cpu));
    }
    if let Some(memory) = &snapshot.memory {
        lines.push(format!(
            "Memory {:.1} / {:.1} GiB",
            memory.used_bytes as f64 / 1_073_741_824.0,
            memory.total_bytes as f64 / 1_073_741_824.0
        ));
    }
    lines.push(format!(
        "wt tree {:.0}% CPU, {:.1} GiB RSS, {} processes; other processes {}",
        snapshot.wt_cpu,
        snapshot.wt_rss_kb as f64 / 1024.0 / 1024.0,
        snapshot.downstream_count,
        snapshot.other_count
    ));
    for (category, (cpu, rss)) in &snapshot.category_totals {
        let count = snapshot
            .categories
            .get(category)
            .copied()
            .unwrap_or_default();
        lines.push(format!(
            "  {category}: {count} process(es), {cpu:.1}% CPU, {:.1} MiB RSS",
            *rss as f64 / 1024.0
        ));
    }
    for session in &snapshot.sessions {
        lines.push(format!(
            "Session {}: {:.1}% CPU, {:.1} MiB RSS, {} process(es): {}",
            session.name,
            session.cpu,
            session.rss_kb as f64 / 1024.0,
            session.count,
            session.summary
        ));
    }
    lines.push("Top wt processes:".into());
    lines.extend(snapshot.top_downstream.iter().map(format_process));
    lines.push("Top other processes:".into());
    lines.extend(snapshot.top_other.iter().map(format_process));
    if snapshot.orphan_probe_available {
        if snapshot.orphans.is_empty() {
            lines.push("No orphaned headless wt processes found.".into());
        } else {
            lines.push(format!(
                "Orphaned headless wt processes: {} (verify identity before acting)",
                snapshot.orphans.len()
            ));
            lines.extend(snapshot.orphans.iter().map(format_process));
        }
    } else {
        lines.push("Orphan detection unavailable on this platform.".into());
    }
    if !snapshot.tmux_probe_available {
        lines.push("tmux session attribution unavailable.".into());
    }
    lines
}

fn format_process(process: &crate::commands::perf::ProcessRow) -> String {
    let session = process
        .session
        .as_deref()
        .map(|name| format!(" [{name}]"))
        .unwrap_or_default();
    format!(
        "  pid {} {} {:.1}% CPU {:.1} MiB RSS up {}: {}{}",
        process.pid,
        process.category,
        process.cpu,
        process.rss_kb as f64 / 1024.0,
        process.elapsed,
        process.command,
        session
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use wt_runtime::{SourceState, source_channel};

    #[tokio::test]
    async fn remains_dormant_until_open_and_stops_sampling_when_closed() {
        let scope = TaskScope::new();
        let (board, board_writer) = source_channel();
        board_writer.publish(SourceSnapshot {
            data: Some(Arc::new(Board::default())),
            state: SourceState::Ready,
            updated_at: None,
            revision: 0,
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let sampler_calls = calls.clone();
        let perf = start_with_sampler(&scope, board, move |_cancel| {
            let calls = sampler_calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec!["sample".into()])
            }
        });

        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        perf.commands.set_active(true);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if calls.load(Ordering::SeqCst) > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("opening the overlay triggers one sample");
        perf.commands.set_active(false);
        perf.commands.set_continuous(true);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test]
    async fn closing_overlay_cancels_an_in_flight_snapshot() {
        let scope = TaskScope::new();
        let (board, board_writer) = source_channel();
        board_writer.publish(SourceSnapshot {
            data: Some(Arc::new(Board::default())),
            state: SourceState::Ready,
            updated_at: None,
            revision: 0,
        });
        let cancelled = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let sampler_cancelled = cancelled.clone();
        let sampler_started = started.clone();
        let perf = start_with_sampler(&scope, board, move |cancel| {
            let cancelled = sampler_cancelled.clone();
            let started = sampler_started.clone();
            async move {
                started.fetch_add(1, Ordering::SeqCst);
                tokio::select! {
                    _ = cancel.cancelled() => {
                        cancelled.fetch_add(1, Ordering::SeqCst);
                        anyhow::bail!("cancelled")
                    }
                    _ = tokio::time::sleep(Duration::from_secs(60)) => Ok(vec!["late".into()]),
                }
            }
        });
        perf.commands.set_active(true);
        tokio::time::timeout(Duration::from_secs(1), async {
            while started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("opening the overlay starts the snapshot");
        perf.commands.set_active(false);
        tokio::time::timeout(Duration::from_secs(1), async {
            while cancelled.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closing the overlay cancels the process snapshot");
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
