#![cfg(unix)]

use std::num::NonZeroUsize;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use wt_platform::lock::MachineSlots;
use wt_platform::process::{
    CommandSpec, ProcessBudget, ProcessClass, ProcessError, ProcessRunner, ProcessStream,
};

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
async fn a_child_that_exits_without_reading_stdin_reports_its_own_failure() {
    let runner = ProcessRunner::default();
    let mut command = CommandSpec::new("sh").args(["-c", "printf refused >&2; exit 3"]);
    // Larger than any pipe buffer, so the write must hit the closed pipe.
    command.input = Some(vec![b'x'; 4 * 1024 * 1024]);
    let output = runner
        .run(command, &CancellationToken::new())
        .await
        .expect("the exit status, not the broken stdin pipe, is the result");
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(output.stderr, b"refused");
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

/// Holds one of `runner`'s slots until the returned token is cancelled.
/// `name` must be "started": the readiness wait accepts only that marker empty.
async fn occupy(
    runner: &ProcessRunner,
    directory: &std::path::Path,
    name: &str,
) -> (
    CancellationToken,
    tokio::task::JoinHandle<Result<wt_platform::process::ProcessOutput, ProcessError>>,
) {
    let started = directory.join(name);
    let cancel = CancellationToken::new();
    let task = {
        let runner = runner.clone();
        let token = cancel.clone();
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
    (cancel, task)
}

#[tokio::test]
async fn the_run_timeout_starts_at_admission() {
    let runner = ProcessRunner::new(NonZeroUsize::new(1).unwrap());
    let directory = tempfile::tempdir().unwrap();
    let (first_cancel, first) = occupy(&runner, directory.path(), "started").await;
    let ran = directory.path().join("second");
    let mut command = CommandSpec::new("touch").args([ran.as_os_str()]);
    command.timeout = Duration::from_millis(300);
    let queued = {
        let runner = runner.clone();
        tokio::spawn(async move { runner.run(command, &CancellationToken::new()).await })
    };
    // Longer in the queue than the whole run timeout.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!queued.is_finished());
    first_cancel.cancel();
    assert!(matches!(
        first.await.unwrap(),
        Err(ProcessError::Cancelled { .. })
    ));
    assert!(queued.await.unwrap().unwrap().status.success());
    assert!(ran.exists());
}

#[tokio::test]
async fn waiting_for_admission_has_its_own_bound_and_never_spawns_past_it() {
    let runner = ProcessRunner::new(NonZeroUsize::new(1).unwrap());
    let directory = tempfile::tempdir().unwrap();
    let (first_cancel, first) = occupy(&runner, directory.path(), "started").await;
    let should_not_exist = directory.path().join("second");
    let mut command = CommandSpec::new("touch").args([should_not_exist.as_os_str()]);
    command.admission_timeout = Some(Duration::from_millis(50));
    let result = runner.run(command.clone(), &CancellationToken::new()).await;
    assert!(
        matches!(result, Err(ProcessError::Timeout { timeout, .. }) if timeout == Duration::from_millis(50))
    );
    // Irreversible work keeps bounding admission by its command timeout.
    command.admission_timeout = None;
    command.timeout = Duration::from_millis(50);
    let result = runner
        .run_to_completion(command, &CancellationToken::new())
        .await;
    assert!(matches!(result, Err(ProcessError::Timeout { .. })));
    assert!(!should_not_exist.exists());
    first_cancel.cancel();
    assert!(matches!(
        first.await.unwrap(),
        Err(ProcessError::Cancelled { .. })
    ));
}

#[tokio::test]
async fn a_command_cancelled_in_the_queue_gives_its_place_to_the_next() {
    let runner = ProcessRunner::new(NonZeroUsize::new(1).unwrap());
    let directory = tempfile::tempdir().unwrap();
    let (first_cancel, first) = occupy(&runner, directory.path(), "started").await;
    let withdrawn = directory.path().join("withdrawn");
    let cancel = CancellationToken::new();
    let queued = {
        let (runner, cancel) = (runner.clone(), cancel.clone());
        let command = CommandSpec::new("touch").args([withdrawn.as_os_str()]);
        tokio::spawn(async move { runner.run(command, &cancel).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    assert!(matches!(
        queued.await.unwrap(),
        Err(ProcessError::Cancelled { .. })
    ));
    first_cancel.cancel();
    let _ = first.await.unwrap();
    runner
        .run(CommandSpec::new("true"), &CancellationToken::new())
        .await
        .unwrap();
    assert!(!withdrawn.exists());
}

#[tokio::test]
async fn a_person_is_admitted_past_saturated_periodic_reads_on_one_budget() {
    let runner = ProcessRunner::with_budget(ProcessBudget {
        total: NonZeroUsize::new(2).unwrap(),
        interactive_reserve: 1,
        background: NonZeroUsize::new(1).unwrap(),
        bulk: NonZeroUsize::new(1).unwrap(),
        starvation_bound: Duration::from_secs(5),
    });
    assert_eq!(runner.class(), ProcessClass::Background);
    let directory = tempfile::tempdir().unwrap();
    let (reader_cancel, reader) = occupy(&runner, directory.path(), "started").await;
    let quick = |class| {
        let runner = runner.with_class(class);
        async move {
            let mut command = CommandSpec::new("true");
            command.admission_timeout = Some(Duration::from_millis(100));
            runner.run(command, &CancellationToken::new()).await
        }
    };
    for class in [
        ProcessClass::Background,
        ProcessClass::Confirming,
        ProcessClass::Bulk,
    ] {
        assert!(
            matches!(quick(class).await, Err(ProcessError::Timeout { .. })),
            "{class:?} took the slot reserved for a person"
        );
    }
    quick(ProcessClass::Interactive).await.unwrap();
    // Heavy work a person asked for keeps their place; other heavy work is bulk.
    assert_eq!(
        runner.with_class(ProcessClass::Interactive).bulk().class(),
        ProcessClass::Interactive
    );
    assert_eq!(runner.bulk().class(), ProcessClass::Bulk);
    reader_cancel.cancel();
    let _ = reader.await.unwrap();
}

#[tokio::test]
async fn heavy_commands_take_a_machine_wide_slot_and_nothing_else_does() {
    let directory = tempfile::tempdir().unwrap();
    let locks = directory.path().join("locks");
    let one = NonZeroUsize::new(1).unwrap();
    let open = || MachineSlots::open(&locks, "process-heavy", one);
    let runner = ProcessRunner::default().with_heavy_slots(open().await.unwrap());
    // Another process's heavy command holds the only slot.
    let elsewhere = open().await.unwrap().try_acquire().unwrap().unwrap();
    let marker = directory.path().join("ran");
    let waiting = {
        let runner = runner.heavy();
        assert_eq!(runner.class(), ProcessClass::Bulk);
        let mut command = CommandSpec::new("touch").args([marker.as_os_str()]);
        // Short enough to fail the test if the wait for a slot counted.
        command.timeout = Duration::from_millis(250);
        tokio::spawn(async move { runner.run(command, &CancellationToken::new()).await })
    };
    for unbounded in [
        runner.clone(),
        runner.bulk(),
        runner.with_class(ProcessClass::Interactive).heavy(),
    ] {
        unbounded
            .run(CommandSpec::new("true"), &CancellationToken::new())
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!waiting.is_finished() && !marker.exists());
    drop(elsewhere);
    assert!(waiting.await.unwrap().unwrap().status.success());
    // The slot came back with the subprocess.
    assert!(open().await.unwrap().try_acquire().unwrap().is_some());
}

#[tokio::test]
async fn a_heavy_command_waiting_for_a_machine_slot_holds_no_local_permit_and_then_runs_anyway() {
    let directory = tempfile::tempdir().unwrap();
    let locks = directory.path().join("locks");
    let one = NonZeroUsize::new(1).unwrap();
    let open = || MachineSlots::open(&locks, "process-heavy", one);
    // One local slot: a heavy command that held it while waiting would
    // block the fetch below.
    let runner = ProcessRunner::new(one).with_heavy_slots(open().await.unwrap());
    let _elsewhere = open().await.unwrap().try_acquire().unwrap().unwrap();
    let marker = directory.path().join("ran");
    let heavy = {
        let runner = runner.heavy();
        let mut command = CommandSpec::new("touch").args([marker.as_os_str()]);
        command.admission_timeout = Some(Duration::from_millis(600));
        tokio::spawn(async move { runner.run(command, &CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut fetch = CommandSpec::new("true");
    fetch.admission_timeout = Some(Duration::from_millis(200));
    runner
        .bulk()
        .run(fetch, &CancellationToken::new())
        .await
        .expect("a bulk command is admitted while a heavy one waits for its machine slot");
    assert!(!marker.exists());
    // The slot never frees. The limit guards no data, so the command runs.
    assert!(heavy.await.unwrap().unwrap().status.success());
    assert!(marker.exists());
}

#[tokio::test]
async fn a_heavy_command_queued_for_a_local_permit_keeps_no_machine_slot_idle() {
    let directory = tempfile::tempdir().unwrap();
    let locks = directory.path().join("locks");
    let one = NonZeroUsize::new(1).unwrap();
    let open = || MachineSlots::open(&locks, "process-heavy", one);
    let runner = ProcessRunner::new(one).with_heavy_slots(open().await.unwrap());
    // This process's only permit is busy with a fetch.
    let (fetch_cancel, fetch) = occupy(&runner.bulk(), directory.path(), "started").await;
    let marker = directory.path().join("ran");
    let heavy = {
        let runner = runner.heavy();
        let command = CommandSpec::new("touch").args([marker.as_os_str()]);
        tokio::spawn(async move { runner.run(command, &CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    // The machine slot stays free for every other process meanwhile.
    let elsewhere = open()
        .await
        .unwrap()
        .try_acquire()
        .unwrap()
        .expect("a heavy command without a local permit holds no machine slot");
    // The permit frees while another process has the slot: the command
    // gives the permit back and waits for the slot instead.
    fetch_cancel.cancel();
    let _ = fetch.await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!heavy.is_finished() && !marker.exists());
    let mut quick = CommandSpec::new("true");
    quick.admission_timeout = Some(Duration::from_millis(500));
    runner
        .bulk()
        .run(quick, &CancellationToken::new())
        .await
        .expect("the permit is free while the heavy command waits for a slot");
    drop(elsewhere);
    assert!(heavy.await.unwrap().unwrap().status.success());
    assert!(open().await.unwrap().try_acquire().unwrap().is_some());
}

#[tokio::test(start_paused = true)]
async fn a_fleet_scan_stops_waiting_for_a_turn_that_does_not_come() {
    let directory = tempfile::tempdir().unwrap();
    let one = NonZeroUsize::new(1).unwrap();
    let open = || MachineSlots::open(directory.path(), "fleet-scan", one);
    // A suspended wt keeps its turn for good.
    let _stuck = open().await.unwrap().try_acquire().unwrap().unwrap();
    let runner = ProcessRunner::default().with_fleet_scan_slots(open().await.unwrap());
    let started = tokio::time::Instant::now();
    assert!(runner.fleet_scan(&CancellationToken::new()).await.is_none());
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(10) && waited < Duration::from_secs(11),
        "{waited:?}"
    );
}

#[tokio::test]
async fn a_fleet_scan_waits_its_machine_wide_turn_unless_someone_is_waiting_on_it() {
    let directory = tempfile::tempdir().unwrap();
    let one = NonZeroUsize::new(1).unwrap();
    let open = || MachineSlots::open(directory.path(), "fleet-scan", one);
    let cancel = CancellationToken::new();
    assert!(ProcessRunner::default().fleet_scan(&cancel).await.is_none());
    let runner = ProcessRunner::default().with_fleet_scan_slots(open().await.unwrap());
    let turn = runner.fleet_scan(&cancel).await.expect("a free turn");
    // Another process's scan must wait for it.
    let elsewhere = ProcessRunner::default().with_fleet_scan_slots(open().await.unwrap());
    let waiting = {
        let (elsewhere, cancel) = (elsewhere.clone(), cancel.clone());
        tokio::spawn(async move { elsewhere.fleet_scan(&cancel).await.is_some() })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiting.is_finished());
    for class in [ProcessClass::Interactive, ProcessClass::Confirming] {
        assert!(
            elsewhere
                .with_class(class)
                .fleet_scan(&cancel)
                .await
                .is_none(),
            "{class:?} does not queue behind other processes' scans"
        );
    }
    drop(turn);
    assert!(waiting.await.unwrap());
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

#[tokio::test]
async fn irreversible_work_finishes_after_its_deadline_and_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let started = directory.path().join("started");
    let retained = directory.path().join("retained");
    std::fs::write(&retained, "fixture only").unwrap();
    let mut command = CommandSpec::new("sh")
        .args([
            "-c",
            "touch started; sleep .4; printf large-output; rm retained; printf finished",
        ])
        .cwd(directory.path());
    command.timeout = Duration::from_millis(150);
    command.output_limit = 3;
    let cancellation = CancellationToken::new();
    let task_cancel = cancellation.clone();
    let runner = ProcessRunner::default();
    let task = tokio::spawn(async move { runner.run_to_completion(command, &task_cancel).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();
    let output = task.await.unwrap().unwrap();
    assert!(output.status.success());
    assert!(output.elapsed > Duration::from_millis(150));
    assert!(output.stdout_truncated);
    assert_eq!(output.stdout, b"lar");
    assert!(!retained.exists());
}

#[tokio::test]
async fn irreversible_work_cancelled_before_admission_never_starts() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("unexpected");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let result = ProcessRunner::default()
        .run_to_completion(
            CommandSpec::new("touch").args([marker.as_os_str()]),
            &cancellation,
        )
        .await;
    assert!(matches!(result, Err(ProcessError::Cancelled { .. })));
    assert!(!marker.exists());
}
