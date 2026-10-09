use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use tokio::sync::{mpsc, oneshot};
use wt_config::Config;
use wt_store::{RepositoryIdentity, Store};

type Job = Box<dyn FnOnce(&mut Store) + Send>;

enum Message {
    Run(Job),
    Shutdown,
}

/// One owner keeps SQLite blocking work out of both the input thread and the
/// async worker pool. Accepted writes drain before shutdown even if a caller
/// stops waiting for its result; queue admission is bounded.
#[derive(Clone)]
pub struct Database {
    send: mpsc::Sender<Message>,
    worker: Arc<Mutex<Option<JoinHandle<()>>>>,
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
        let (send, mut receive) = mpsc::channel::<Message>(64);
        let (ready, started) = oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("wt-state".into())
            .spawn(move || {
                let mut store = match Store::open(path, identity) {
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

    pub async fn shutdown(self) -> Result<()> {
        let _ = self.send.send(Message::Shutdown).await;
        let worker = self
            .worker
            .lock()
            .map_err(|_| anyhow!("state worker join lock poisoned"))?
            .take();
        if let Some(worker) = worker {
            tokio::task::spawn_blocking(move || worker.join())
                .await
                .context("join state worker")?
                .map_err(|_| anyhow!("state worker panicked"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
