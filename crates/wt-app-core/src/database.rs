use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use tokio::sync::{mpsc, oneshot};
use wt_config::Config;
use wt_store::{RepositoryIdentity, Store};

type Job = Box<dyn FnOnce(&mut Store) + Send>;
type ReadJob = Box<dyn FnOnce(&mut Store) + Send>;

enum Message<J> {
    Run(J),
    Shutdown,
}

/// One owner keeps SQLite blocking work out of both the input thread and the
/// async worker pool. Accepted writes drain before shutdown even if a caller
/// stops waiting for its result; queue admission is bounded.
///
/// Writes, and reads that must be ordered with queued writes, go through
/// [`Database::call`] on the one writable connection. [`Database::read`] uses
/// a second, read-only connection on its own thread, so a read neither waits
/// for nor delays a write that is waiting out another process's lock.
#[derive(Clone)]
pub struct Database {
    send: mpsc::Sender<Message<Job>>,
    worker: Arc<Mutex<Option<JoinHandle<()>>>>,
    reader: Arc<Reader>,
}

/// The read-only connection, opened by the first read: most one-shot
/// commands never need it.
struct Reader {
    path: PathBuf,
    identity: RepositoryIdentity,
    started: tokio::sync::OnceCell<mpsc::Sender<Message<ReadJob>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
}

impl Database {
    pub async fn open(config: &Config) -> Result<Self> {
        Self::open_path(
            config.paths.state_db.clone(),
            RepositoryIdentity::new(&config.repo_id, config.repo_path.to_string_lossy()),
        )
        .await
    }

    async fn open_path(path: std::path::PathBuf, identity: RepositoryIdentity) -> Result<Self> {
        let reader = Arc::new(Reader {
            path: path.clone(),
            identity: identity.clone(),
            started: tokio::sync::OnceCell::new(),
            worker: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        let (send, mut receive) = mpsc::channel::<Message<Job>>(64);
        let (ready, started) = oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("wt-state".into())
            .spawn(move || {
                let mut store = match Store::open(path, identity) {
                    Ok(mut store) => {
                        // The read-only connection returns an older build's
                        // payload as it is. Migrate it here, before any read
                        // can start, so none sees a shape this build does
                        // not expect. A payload that cannot be migrated
                        // fails the reads that parse it, as it always did.
                        if let Err(error) = store.read_repository_state_json_migrated() {
                            tracing::debug!(%error, "state payload migration at open failed");
                        }
                        let _ = ready.send(Ok(()));
                        store
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                while let Some(message) = receive.blocking_recv() {
                    match message {
                        Message::Run(job) => job(&mut store),
                        Message::Shutdown => {
                            receive.close();
                        }
                    }
                }
            })
            .context("start state worker")?;
        match started.await.context("state worker exited during startup") {
            Ok(Ok(())) => {}
            result => {
                let _ = worker.join();
                result??;
                unreachable!("successful startup handled above");
            }
        }
        Ok(Self {
            send,
            worker: Arc::new(Mutex::new(Some(worker))),
            reader,
        })
    }

    pub async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (reply, result) = oneshot::channel();
        self.send
            .send(Message::Run(Box::new(move |store| {
                // One bad consumer must not silently terminate all future database
                // writes. Transaction RAII rolls back a panicking store operation.
                let value =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(store)))
                        .unwrap_or_else(|_| Err(anyhow!("state operation panicked")));
                let _ = reply.send(value);
            })))
            .await
            .map_err(|_| anyhow!("state worker unavailable"))?;
        result
            .await
            .context("state worker exited without replying")?
    }

    /// Run a read on the read-only connection, beside the write queue.
    ///
    /// It sees every write whose [`Database::call`] returned before this was
    /// called, and may or may not see one still queued or running. A read
    /// that must follow a write the caller has not awaited belongs in
    /// `call`. A read that decides a write belongs in that write's own `call`,
    /// where no other write can come between the two. The store refuses
    /// every write here with `StoreError::ReadOnly`. Statements of one read
    /// are not one snapshot: another connection may commit between them, as
    /// another process always could.
    pub async fn read<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let send = self
            .reader
            .started
            .get_or_try_init(|| self.reader.start())
            .await?;
        let (reply, result) = oneshot::channel();
        send.send(Message::Run(Box::new(move |store| {
            let value = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(store)))
                .unwrap_or_else(|_| Err(anyhow!("state read panicked")));
            let _ = reply.send(value);
        })))
        .await
        .map_err(|_| anyhow!("state reader unavailable"))?;
        result
            .await
            .context("state reader exited without replying")?
    }

    pub async fn shutdown(self) -> Result<()> {
        self.reader.closed.store(true, Ordering::SeqCst);
        if let Some(send) = self.reader.started.get() {
            let _ = send.send(Message::Shutdown).await;
        }
        // Accepted writes drain whatever became of the reader.
        let _ = self.send.send(Message::Shutdown).await;
        let reader = join_worker(&self.reader.worker, "state reader").await;
        let writer = join_worker(&self.worker, "state worker").await;
        writer.and(reader)
    }
}

async fn join_worker(worker: &Mutex<Option<JoinHandle<()>>>, name: &'static str) -> Result<()> {
    let worker = worker
        .lock()
        .map_err(|_| anyhow!("{name} join lock poisoned"))?
        .take();
    if let Some(worker) = worker {
        tokio::task::spawn_blocking(move || worker.join())
            .await
            .with_context(|| format!("join {name}"))?
            .map_err(|_| anyhow!("{name} panicked"))?;
    }
    Ok(())
}

impl Reader {
    async fn start(&self) -> Result<mpsc::Sender<Message<ReadJob>>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(anyhow!("state reader unavailable"));
        }
        let (send, mut receive) = mpsc::channel::<Message<ReadJob>>(64);
        let (ready, started) = oneshot::channel();
        let (path, identity) = (self.path.clone(), self.identity.clone());
        let worker = std::thread::Builder::new()
            .name("wt-state-read".into())
            .spawn(move || {
                // The writable connection opened first, so the file and its
                // schema exist and this never reads an empty stand-in.
                let mut store = match Store::open_read_only(path, identity) {
                    Ok(store) => {
                        let _ = ready.send(Ok(()));
                        store
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                while let Some(message) = receive.blocking_recv() {
                    match message {
                        Message::Run(job) => job(&mut store),
                        Message::Shutdown => receive.close(),
                    }
                }
            })
            .context("start state reader")?;
        match started.await.context("state reader exited during startup") {
            Ok(Ok(())) => {}
            result => {
                let _ = worker.join();
                result??;
                unreachable!("successful startup handled above");
            }
        }
        *self
            .worker
            .lock()
            .map_err(|_| anyhow!("state reader join lock poisoned"))? = Some(worker);
        Ok(send)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_read_does_not_queue_behind_a_stalled_write_and_sees_finished_ones() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open_path(
            directory.path().join("state.sqlite"),
            RepositoryIdentity::new("fixture", directory.path().to_string_lossy()),
        )
        .await
        .unwrap();
        database
            .call(|store| Ok(store.write_repository_state_json(r#"{"version":17,"n":1}"#)?))
            .await
            .unwrap();
        // A write held on the writable connection, as one is while it waits
        // out another process's lock.
        let (entered, started) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let stalled = {
            let database = database.clone();
            tokio::spawn(async move {
                database
                    .call(move |store| {
                        let _ = entered.send(());
                        gate.recv().unwrap();
                        Ok(store.write_repository_state_json(r#"{"version":17,"n":2}"#)?)
                    })
                    .await
            })
        };
        started.await.unwrap();
        let read = || database.read(|store| Ok(store.read_repository_state_json()?));
        let seen = tokio::time::timeout(std::time::Duration::from_secs(5), read())
            .await
            .expect("the read ran beside the stalled write")
            .unwrap();
        assert_eq!(seen.as_deref(), Some(r#"{"version":17,"n":1}"#));
        let queued = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            database.call(|store| Ok(store.read_repository_state_json()?)),
        )
        .await;
        assert!(
            queued.is_err(),
            "a read through the write queue still waits its turn"
        );
        release.send(()).unwrap();
        stalled.await.unwrap().unwrap();
        assert_eq!(
            read().await.unwrap().as_deref(),
            Some(r#"{"version":17,"n":2}"#),
            "a read sees every write that returned before it"
        );
        let handle = database.clone();
        database.shutdown().await.unwrap();
        assert!(handle.read(|_| Ok(())).await.is_err());
    }

    #[tokio::test]
    async fn a_read_gets_this_builds_state_shape_and_cannot_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite");
        let identity = RepositoryIdentity::new("fixture", directory.path().to_string_lossy());
        // An older build left its payload behind.
        let older = wt_store::CURRENT_WT_STATE_VERSION - 1;
        Store::open(&path, identity.clone())
            .unwrap()
            .write_repository_state_json(&format!(r#"{{"version":{older}}}"#))
            .unwrap();
        let database = Database::open_path(path, identity).await.unwrap();
        // The read-only connection cannot migrate; its first read must
        // still not see the older shape.
        let state = database
            .read(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(
            state["version"].as_u64(),
            Some(wt_store::CURRENT_WT_STATE_VERSION)
        );
        let refused = database
            .read(|store| Ok(store.write_repository_state_json("{}")?))
            .await;
        assert!(refused.is_err());
        database.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn the_writer_drains_and_stops_even_when_the_reader_cannot_be_joined() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite");
        let identity = RepositoryIdentity::new("fixture", directory.path().to_string_lossy());
        let database = Database::open_path(path.clone(), identity.clone())
            .await
            .unwrap();
        *database.reader.worker.lock().unwrap() = Some(
            std::thread::Builder::new()
                .spawn(|| panic!("fixture reader failure"))
                .unwrap(),
        );
        let writer = database.clone();
        writer
            .call(|store| Ok(store.write_repository_state_json(r#"{"version":17,"kept":true}"#)?))
            .await
            .unwrap();
        assert!(database.shutdown().await.is_err());
        assert!(
            writer.call(|_| Ok(())).await.is_err(),
            "the writer was shut down"
        );
        let store = Store::open_read_only(path, identity).unwrap();
        assert_eq!(
            store.read_repository_state_json().unwrap().as_deref(),
            Some(r#"{"version":17,"kept":true}"#)
        );
    }

    #[tokio::test]
    async fn a_failing_read_reports_its_error_and_keeps_the_reader_available() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open_path(
            directory.path().join("state.sqlite"),
            RepositoryIdentity::new("fixture", directory.path().to_string_lossy()),
        )
        .await
        .unwrap();
        let error = database
            .read::<()>(|_| panic!("consumer error"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("state read panicked"));
        assert!(
            !database
                .read(|store| Ok(store.has_repository_state()?))
                .await
                .unwrap()
        );
        database.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn accepted_write_finishes_when_caller_cancels_and_shutdown_drains_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite");
        let identity = RepositoryIdentity::new("fixture", directory.path().to_string_lossy());
        let database = Database::open_path(path.clone(), identity.clone())
            .await
            .unwrap();
        let (entered, started) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let request = {
            let database = database.clone();
            tokio::spawn(async move {
                database
                    .call(move |store| {
                        let _ = entered.send(());
                        gate.recv().unwrap();
                        store.write_repository_state_json(r#"{"version":17,"accepted":true}"#)?;
                        Ok(())
                    })
                    .await
            })
        };
        started.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        database.shutdown().await.unwrap();
        let store = Store::open_read_only(path, identity).unwrap();
        assert_eq!(
            store.read_repository_state_json().unwrap().as_deref(),
            Some(r#"{"version":17,"accepted":true}"#)
        );
    }

    #[tokio::test]
    async fn failing_consumer_reports_error_and_keeps_worker_available() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open_path(
            directory.path().join("state.sqlite"),
            RepositoryIdentity::new("fixture", directory.path().to_string_lossy()),
        )
        .await
        .unwrap();
        let error = database
            .call::<()>(|_| panic!("consumer error"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("state operation panicked"));
        assert!(
            !database
                .call(|store| Ok(store.has_repository_state()?))
                .await
                .unwrap()
        );
        database.shutdown().await.unwrap();
    }
}
