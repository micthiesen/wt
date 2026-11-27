//! A small pool for blocking jobs that run for seconds.
//!
//! The runtime's blocking pool is deliberately small and serves every
//! `tokio::fs` call and board composition. One multi-second hash or directory
//! walk per thread stalls all of them, so those jobs run here instead, on
//! threads nothing short ever queues behind.

use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex, OnceLock, PoisonError},
};

use thiserror::Error;
use tokio::sync::oneshot;

/// Long jobs are CPU and disk bound. Two at once keeps a fleet-wide plan
/// moving without competing with the subprocess budget for every core.
const THREADS: usize = 2;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LongJobError {
    #[error("long blocking job panicked")]
    Panicked,
    #[error("long blocking job could not start: {0}")]
    Spawn(String),
}

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Pool {
    state: Mutex<PoolState>,
    wake: Condvar,
}

#[derive(Default)]
struct PoolState {
    queue: VecDeque<Job>,
    threads: usize,
    idle: usize,
}

static POOL: OnceLock<Pool> = OnceLock::new();

/// Run a blocking job that may take seconds (hashing a checkout's untracked
/// files, walking or deleting a large directory) and wait for its value.
/// Jobs start in submission order, at most two at a time for the process. A
/// job whose caller stopped waiting before it started is skipped; one that
/// has started runs to its end, as `spawn_blocking` jobs do.
pub async fn run_long<T, F>(job: F) -> Result<T, LongJobError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (reply, result) = oneshot::channel();
    POOL.get_or_init(Pool::default).submit(Box::new(move || {
        if reply.is_closed() {
            return;
        }
        let value = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
            .map_err(|_| LongJobError::Panicked);
        let _ = reply.send(value);
    }))?;
    // The sender is dropped unsent only by a skipped job, which requires
    // this receiver to be gone already.
    result.await.unwrap_or(Err(LongJobError::Panicked))
}

impl Pool {
    fn submit(&'static self, job: Job) -> Result<(), LongJobError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        // An idle thread that was woken for an earlier job may not have taken
        // it yet, so compare with the whole queue, not with this job alone.
        if state.queue.len() >= state.idle && state.threads < THREADS {
            std::thread::Builder::new()
                .name("wt-long".into())
                .spawn(move || self.work())
                .map_err(|error| LongJobError::Spawn(error.to_string()))?;
            state.threads += 1;
        }
        state.queue.push_back(job);
        drop(state);
        self.wake.notify_one();
        Ok(())
    }

    fn work(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(job) = state.queue.pop_front() {
                drop(state);
                job();
                state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            } else {
                state.idle += 1;
                state = self
                    .wake
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
                state.idle -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, atomic::AtomicUsize, atomic::Ordering};
    use std::time::Duration;

    /// The pool is one per process and tests run in parallel, so tests that
    /// occupy its threads take turns.
    static EXCLUSIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn a_job_returns_its_value_and_a_panic_is_an_error_not_a_dead_pool() {
        let _exclusive = EXCLUSIVE.lock().await;
        assert_eq!(run_long(|| 6 * 7).await, Ok(42));
        assert_eq!(
            run_long(|| -> u8 { panic!("fixture") }).await,
            Err(LongJobError::Panicked)
        );
        assert_eq!(run_long(|| "still running").await, Ok("still running"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn long_jobs_are_bounded_and_leave_the_runtime_blocking_pool_free() {
        let _exclusive = EXCLUSIVE.lock().await;
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Barrier::new(THREADS + 1));
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let jobs: Vec<_> = (0..THREADS + 2)
            .map(|index| {
                let (running, most) = (running.clone(), most.clone());
                let (entered, gate) = (entered.clone(), gate.clone());
                tokio::spawn(run_long(move || {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    if index < THREADS {
                        entered.wait();
                    }
                    let _ = gate.lock().unwrap().recv();
                    running.fetch_sub(1, Ordering::SeqCst);
                    index
                }))
            })
            .collect();
        tokio::task::spawn_blocking(move || entered.wait())
            .await
            .unwrap();
        // Every long thread is now held. A short blocking call still runs.
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(|| std::fs::metadata(".").is_ok()),
        )
        .await
        .expect("the runtime's blocking pool is not behind the long jobs")
        .unwrap();
        for _ in 0..THREADS + 2 {
            release.send(()).unwrap();
        }
        for (index, job) in jobs.into_iter().enumerate() {
            assert_eq!(job.await.unwrap(), Ok(index));
        }
        assert_eq!(most.load(Ordering::SeqCst), THREADS);
    }

    #[tokio::test]
    async fn a_job_nobody_waits_for_any_more_is_skipped() {
        let _exclusive = EXCLUSIVE.lock().await;
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let holders: Vec<_> = (0..THREADS)
            .map(|_| {
                let gate = gate.clone();
                tokio::spawn(run_long(move || {
                    let _ = gate.lock().unwrap().recv();
                }))
            })
            .collect();
        let ran = Arc::new(AtomicUsize::new(0));
        let abandoned = tokio::spawn(run_long({
            let ran = ran.clone();
            move || ran.fetch_add(1, Ordering::SeqCst)
        }));
        // Let the abandoned job reach the queue behind the holders.
        tokio::time::sleep(Duration::from_millis(50)).await;
        abandoned.abort();
        let _ = abandoned.await;
        for _ in 0..THREADS {
            release.send(()).unwrap();
        }
        for holder in holders {
            holder.await.unwrap().unwrap();
        }
        assert_eq!(run_long(|| 1).await, Ok(1));
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }
}
