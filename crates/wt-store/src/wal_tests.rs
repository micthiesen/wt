use super::*;
use std::{sync::mpsc, thread};
use tempfile::tempdir;

#[test]
fn simultaneous_first_opens_repeatedly_create_a_fresh_database() {
    let temp = tempdir().unwrap();
    let workers = 8;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(workers));
    let threads = (0..workers)
        .map(|_| {
            let root = temp.path().to_path_buf();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let mut failures = Vec::new();
                for round in 0..40 {
                    barrier.wait();
                    let result = Store::open(
                        root.join(format!("round-{round}.sqlite")),
                        RepositoryIdentity::new("repo", "/repo"),
                    );
                    // Keep every participant alive through the barriers so a
                    // failure reports an error instead of wedging the fixture.
                    barrier.wait();
                    if let Err(error) = result {
                        failures.push(format!("round {round}: {error}"));
                    }
                }
                failures
            })
        })
        .collect::<Vec<_>>();
    for worker in threads {
        let failures = worker.join().unwrap();
        assert!(failures.is_empty(), "{failures:?}");
    }
}

#[test]
fn journal_mode_waits_for_a_competing_reader_to_release_its_lock() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let reader = Connection::open(&path).unwrap();
    reader
        .execute_batch("CREATE TABLE fixture (value); BEGIN; SELECT * FROM fixture;")
        .unwrap();
    let writer = Connection::open(&path).unwrap();
    writer.busy_timeout(Duration::ZERO).unwrap();
    let error = writer
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap_err();
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        started_tx.send(()).unwrap();
        let result = enable_wal(&writer, Duration::from_secs(3));
        done_tx.send(()).unwrap();
        result.unwrap();
        let mode: String = writer
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    });
    started_rx.recv().unwrap();
    assert!(matches!(
        done_rx.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    reader.execute_batch("ROLLBACK").unwrap();
    worker.join().unwrap();
}

#[test]
fn journal_mode_contention_has_a_bounded_deadline() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let reader = Connection::open(&path).unwrap();
    reader
        .execute_batch("CREATE TABLE fixture (value); BEGIN; SELECT * FROM fixture;")
        .unwrap();
    let writer = Connection::open(&path).unwrap();
    let started = Instant::now();
    let error = enable_wal(&writer, Duration::from_millis(50)).unwrap_err();
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    reader.execute_batch("ROLLBACK").unwrap();
}
