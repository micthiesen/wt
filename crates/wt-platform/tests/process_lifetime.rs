#![cfg(unix)]

use std::num::NonZeroUsize;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessError, ProcessRunner, ProcessStream};

#[test]
fn public_process_futures_do_not_embed_pipe_buffers_in_callers() {
    let runner = ProcessRunner::default();
    let cancel = CancellationToken::new();
    // These futures are composed through many service layers. Large inline
    // captures previously overflowed a runtime worker's stack in debug builds.
    let ordinary = runner.run(CommandSpec::new("true"), &cancel);
    let streaming = runner.run_streaming(CommandSpec::new("true"), &cancel, |_, _| {});
    assert!(std::mem::size_of_val(&ordinary) < 4096);
    assert!(std::mem::size_of_val(&streaming) < 4096);
}

#[tokio::test]
async fn argv_cwd_stdin_and_nonzero_status_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let runner = ProcessRunner::default();
    let mut command = CommandSpec::new("sh")
        .args([
            "-c",
            "printf '%s\\n' \"$1\"; cat; printf failure >&2; exit 7",
            "test",
            "literal $(not-a-command)",
        ])
        .cwd(dir.path());
    command.input = Some(b"input\n".to_vec());
    let output = runner
        .run(command, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(output.stdout, b"literal $(not-a-command)\ninput\n");
    assert_eq!(output.stderr, b"failure");
    assert_eq!(output.status.code(), Some(7));
    assert!(matches!(
        output.checked("sh"),
        Err(ProcessError::Exit { code: Some(7), .. })
    ));
}

#[tokio::test]
async fn output_limit_stops_a_producer_instead_of_allocating_without_bound() {
    let runner = ProcessRunner::default();
    let mut command = CommandSpec::new("sh").args(["-c", "while :; do printf 1234567890; done"]);
    command.output_limit = 128;
    let result = runner.run(command, &CancellationToken::new()).await;
    assert!(matches!(
        result,
        Err(ProcessError::OutputLimit { limit: 128, .. })
    ));
    // A failed command returns its lifetime permit and does not poison the runner.
    let output = runner
        .run(CommandSpec::new("true"), &CancellationToken::new())
        .await
        .unwrap();
    assert!(output.status.success());
}

#[tokio::test]
async fn streaming_drains_beyond_bounded_capture_and_reports_truncation() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = seen.clone();
    let mut command =
        CommandSpec::new("sh").args(["-c", "printf 'abcdefgh'; printf 'stderr-output' >&2"]);
    command.output_limit = 3;
    let output = ProcessRunner::default()
        .run_streaming(command, &CancellationToken::new(), move |stream, chunk| {
            observed.lock().unwrap().push((stream, chunk.to_vec()));
        })
        .await
        .unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, b"abc");
    assert_eq!(output.stderr, b"std");
    assert!(output.stdout_truncated);
    assert!(output.stderr_truncated);
    let seen = seen.lock().unwrap();
    let stdout: Vec<_> = seen
        .iter()
        .filter(|(stream, _)| *stream == ProcessStream::Stdout)
        .flat_map(|(_, bytes)| bytes.iter().copied())
        .collect();
    let stderr: Vec<_> = seen
        .iter()
        .filter(|(stream, _)| *stream == ProcessStream::Stderr)
        .flat_map(|(_, bytes)| bytes.iter().copied())
        .collect();
    assert_eq!(stdout, b"abcdefgh");
    assert_eq!(stderr, b"stderr-output");
}

#[tokio::test]
async fn streaming_observer_panic_returns_error_and_cleans_process_group() {
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        ProcessRunner::default()
            .run_streaming(
                CommandSpec::new("sh").args(["-c", "printf x; sleep 30 & wait"]),
                &CancellationToken::new(),
                |_, _| panic!("fixture observer panic"),
            )
            .await
    })
    .await
    .expect("observer failure must not strand the process");
    assert!(matches!(result, Err(ProcessError::ObserverPanicked { .. })));
}

#[tokio::test]
async fn pre_cancelled_work_never_spawns() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("unexpected");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = ProcessRunner::default()
        .run(
            CommandSpec::new("touch").args([marker.as_os_str()]),
            &cancel,
        )
        .await;
    assert!(matches!(result, Err(ProcessError::Cancelled { .. })));
    assert!(!marker.exists());
}

#[tokio::test]
async fn deadline_includes_waiting_for_capacity() {
    let runner = ProcessRunner::new(NonZeroUsize::new(1).unwrap());
    let directory = tempfile::tempdir().unwrap();
    let started = directory.path().join("started");
    let first_cancel = CancellationToken::new();
    let first = {
        let runner = runner.clone();
        let token = first_cancel.clone();
        let marker = started.clone();
        tokio::spawn(async move {
            runner
                .run(
                    CommandSpec::new("sh").args([
                        "-c",
                        "touch \"$1\"; sleep 30",
                        "test",
                        marker.to_str().unwrap(),
                    ]),
                    &token,
                )
                .await
        })
    };
    wait_for_file(&started).await;
    let should_not_exist = directory.path().join("second");
    let mut command = CommandSpec::new("touch").args([should_not_exist.as_os_str()]);
    command.timeout = Duration::from_millis(50);
    let result = runner.run(command, &CancellationToken::new()).await;
    assert!(matches!(result, Err(ProcessError::Timeout { .. })));
    assert!(!should_not_exist.exists());
    first_cancel.cancel();
    assert!(matches!(
        first.await.unwrap(),
        Err(ProcessError::Cancelled { .. })
    ));
}

#[tokio::test]
async fn timeout_kills_descendants_that_hold_output_pipes() {
    let runner = ProcessRunner::default();
    let mut command = CommandSpec::new("sh").args(["-c", "sleep 30 & wait"]);
    command.timeout = Duration::from_millis(50);
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        runner.run(command, &CancellationToken::new()),
    )
    .await;
    assert!(matches!(result, Ok(Err(ProcessError::Timeout { .. }))));
}

#[tokio::test]
async fn successful_leader_does_not_leak_children_that_closed_their_pipes() {
    let output = ProcessRunner::default()
        .run(
            CommandSpec::new("sh").args(["-c", "sleep 30 </dev/null >/dev/null 2>&1 & echo $!"]),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(output.status.success());
    let pid: i32 = output.stdout_text().trim().parse().unwrap();
    let exited = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let state = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&state.stdout);
            // An orphaned zombie is already dead; its system reaper owns wait.
            if state.trim().is_empty() || state.trim_start().starts_with('Z') {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if exited.is_err() {
        // Do not leave the regression fixture running if this assertion fails.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    exited.expect("successful command must leave no running descendants");
}

#[tokio::test]
async fn dropping_the_runner_future_kills_the_owned_process_group() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("pid");
    let task = {
        let marker = marker.clone();
        tokio::spawn(async move {
            ProcessRunner::default()
                .run(
                    CommandSpec::new("sh").args([
                        "-c",
                        "echo $$ > \"$1\"; sleep 30 & wait",
                        "test",
                        marker.to_str().unwrap(),
                    ]),
                    &CancellationToken::new(),
                )
                .await
        })
    };
    wait_for_file(&marker).await;
    let pid: i32 = std::fs::read_to_string(&marker)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            // SAFETY: signal zero only checks whether the process exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("aborted command must be killed and reaped");
}

async fn wait_for_file(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if path.metadata().is_ok_and(|m| m.len() > 0) {
                break;
            }
            // Empty touch marker is also enough for the capacity test.
            if path.file_name().is_some_and(|name| name == "started") && path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("child readiness marker");
}

#[tokio::test]
async fn external_children_survive_only_a_successful_explicit_ownership_transfer() {
    for exit in [0, 7] {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("finished");
        let mut spec = CommandSpec::new("sh").args([
            "-c",
            "(sleep 0.1; printf done > \"$1\") </dev/null >/dev/null 2>&1 & exit \"$2\"",
            "test",
            marker.to_str().unwrap(),
            &exit.to_string(),
        ]);
        spec.preserve_children_on_success = true;
        let result = ProcessRunner::default()
            .run(spec, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.status.code(), Some(exit));
        if exit == 0 {
            wait_for_file(&marker).await;
        } else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(!marker.exists());
        }
    }
}
