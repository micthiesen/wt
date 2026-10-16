//! Sparse per-phase timings for worktree removal, never command output.
use std::{
    future::Future,
    path::{Path, PathBuf},
    time::Instant,
};

use crate::LifecycleError;

pub(crate) struct RemovalPhase {
    phase: &'static str,
    path: PathBuf,
    start: Instant,
    finished: bool,
}

impl RemovalPhase {
    pub(crate) fn new(path: &Path, phase: &'static str) -> Self {
        tracing::debug!(target: "wt::removal", phase, path = %path.display(), "removal phase started");
        Self {
            phase,
            path: path.to_owned(),
            start: Instant::now(),
            finished: false,
        }
    }

    pub(crate) fn finish(mut self, outcome: &'static str) {
        self.log(outcome);
    }

    pub(crate) fn lifecycle<T>(self, result: &Result<T, LifecycleError>) {
        self.finish(match result {
            Ok(_) => "success",
            Err(LifecycleError::Cancelled) => "cancelled",
            Err(LifecycleError::Refused(_)) => "refused",
            Err(_) => "error",
        });
    }

    fn log(&mut self, outcome: &'static str) {
        tracing::info!(target: "wt::removal", phase = self.phase, path = %self.path.display(),
            elapsed_ms = self.start.elapsed().as_secs_f64() * 1000.0, outcome,
            "removal phase finished");
        self.finished = true;
    }
}

impl Drop for RemovalPhase {
    fn drop(&mut self) {
        if !self.finished {
            self.log("interrupted");
        }
    }
}

/// Time one fallible removal phase.
pub(crate) async fn timed<T>(
    path: &Path,
    phase: &'static str,
    work: impl Future<Output = Result<T, LifecycleError>>,
) -> Result<T, LifecycleError> {
    let span = RemovalPhase::new(path, phase);
    let result = work.await;
    span.lifecycle(&result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn removal_phases_log_outcome_and_elapsed_without_details() {
        let log = tempfile::NamedTempFile::new().unwrap();
        let file = log.reopen().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_writer(move || file.try_clone().unwrap())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let path = Path::new("/fixture");
        timed(path, "success", async { Ok::<_, LifecycleError>(()) })
            .await
            .unwrap();
        let _ = timed(path, "refused", async {
            Err::<(), _>(LifecycleError::Refused("private detail".into()))
        })
        .await;
        let _ = timed(path, "cancelled", async {
            Err::<(), _>(LifecycleError::Cancelled)
        })
        .await;
        drop(RemovalPhase::new(path, "interrupted"));
        drop(_guard);
        let log = std::fs::read_to_string(log.path()).unwrap();
        assert!(!log.contains("private detail"));
        let finished: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| event["fields"]["message"] == "removal phase finished")
            .collect();
        assert_eq!(finished.len(), 4);
        for event in finished {
            assert_eq!(event["fields"]["phase"], event["fields"]["outcome"]);
            assert!(event["fields"]["elapsed_ms"].as_f64().unwrap() >= 0.0);
        }
    }
}
