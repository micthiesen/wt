//! Sparse stage timings for worktree creation, never command output or hooks' arguments.
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use wt_platform::process::{ProcessError, ProcessOutput};

pub(crate) struct CreationPhase {
    phase: &'static str,
    path: PathBuf,
    start: Instant,
    finished: bool,
}

impl CreationPhase {
    pub(crate) fn new(path: &Path, phase: &'static str) -> Self {
        tracing::info!(target: "wt::creation", phase, path = %path.display(), "creation phase started");
        Self {
            phase,
            path: path.to_owned(),
            start: Instant::now(),
            finished: false,
        }
    }

    pub(crate) fn process(mut self, result: &Result<ProcessOutput, ProcessError>, allow_one: bool) {
        let (outcome, exit_code) = match result {
            Ok(output)
                if output.status.success() || (allow_one && output.status.code() == Some(1)) =>
            {
                ("success", output.status.code())
            }
            Ok(output) => ("error", output.status.code()),
            Err(ProcessError::Cancelled { .. }) => ("cancelled", None),
            Err(ProcessError::Timeout { .. }) => ("timeout", None),
            Err(_) => ("error", None),
        };
        self.finish(outcome, exit_code);
    }

    fn finish(&mut self, outcome: &'static str, exit_code: Option<i32>) {
        tracing::info!(target: "wt::creation", phase = self.phase, path = %self.path.display(),
            elapsed_ms = self.start.elapsed().as_secs_f64() * 1000.0, outcome, exit_code,
            "creation phase finished");
        self.finished = true;
    }
}

impl Drop for CreationPhase {
    fn drop(&mut self) {
        if !self.finished {
            self.finish("interrupted", None);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn stage_log_distinguishes_exit_failure_cancellation_timeout_and_dropped_future() {
        let log = tempfile::NamedTempFile::new().unwrap();
        let file = log.reopen().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_writer(move || file.try_clone().unwrap())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            for (phase, result) in [
                (
                    "success",
                    Ok(ProcessOutput {
                        status: std::process::ExitStatus::from_raw(0),
                        stdout: vec![],
                        stderr: vec![],
                        stdout_truncated: false,
                        stderr_truncated: false,
                        elapsed: std::time::Duration::ZERO,
                    }),
                ),
                (
                    "error",
                    Ok(ProcessOutput {
                        status: std::process::ExitStatus::from_raw(256),
                        stdout: b"private output".to_vec(),
                        stderr: vec![],
                        stdout_truncated: false,
                        stderr_truncated: false,
                        elapsed: std::time::Duration::ZERO,
                    }),
                ),
                (
                    "cancelled",
                    Err(ProcessError::Cancelled {
                        program: "secret command".into(),
                    }),
                ),
                (
                    "timeout",
                    Err(ProcessError::Timeout {
                        program: "secret command".into(),
                        timeout: std::time::Duration::from_secs(1),
                    }),
                ),
            ] {
                CreationPhase::new(Path::new("/fixture"), phase).process(&result, false);
            }
            drop(CreationPhase::new(Path::new("/fixture"), "interrupted"));
        });
        let log = std::fs::read_to_string(log.path()).unwrap();
        assert!(!log.contains("private output") && !log.contains("secret command"));
        let finished: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| event["fields"]["message"] == "creation phase finished")
            .collect();
        assert_eq!(finished.len(), 5);
        for event in finished {
            assert_eq!(event["fields"]["phase"], event["fields"]["outcome"]);
            assert!(event["fields"]["elapsed_ms"].as_f64().unwrap() >= 0.0);
            assert_eq!(event["fields"]["path"], "/fixture");
        }
    }
}
