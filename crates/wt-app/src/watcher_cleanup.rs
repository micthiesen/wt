//! Native watcher destruction can join OS threads. Keep that work off Tokio's
//! executor, but await it so a source is not drained before its watcher is.

pub(crate) async fn drop_watcher<T: Send + 'static>(watcher: T) {
    tokio::task::spawn_blocking(move || drop(watcher))
        .await
        .expect("native watcher cleanup panicked");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn watcher_cleanup_does_not_block_executor_and_is_joined() {
        struct BlockingDrop(
            tokio::sync::oneshot::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        );
        impl Drop for BlockingDrop {
            fn drop(&mut self) {
                let (unused, _) = tokio::sync::oneshot::channel();
                let _ = std::mem::replace(&mut self.0, unused).send(());
                // Fail boundedly even if cleanup accidentally runs on the
                // current-thread executor and prevents its release below.
                let _ = self.1.recv_timeout(Duration::from_secs(2));
            }
        }
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let before = Instant::now();
        let cleanup = tokio::spawn(drop_watcher(BlockingDrop(entered, held)));
        started.await.unwrap();
        let responsive = before.elapsed() < Duration::from_millis(500);
        assert!(
            !cleanup.is_finished(),
            "cleanup cannot finish before native drop"
        );
        let _ = release.send(());
        cleanup.await.unwrap();
        assert!(responsive, "native destructor blocked the async executor");
    }
}
