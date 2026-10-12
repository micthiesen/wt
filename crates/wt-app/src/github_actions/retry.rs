//! The retry owner: one scoped actor that keeps re-sending a merge-when-ready
//! arm while required checks have not reported, bounded in count and time.

use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::{Instant, sleep},
};
use tokio_util::sync::CancellationToken;
use wt_github::{GithubClient, PrMergeTarget};
use wt_runtime::TaskScope;

use super::writes::{GithubWrite, WriteLedger};

const RETRY_EVERY: Duration = Duration::from_secs(30);
const RETRY_LIMIT: Duration = Duration::from_secs(20 * 60);
const MAX_RETRIES: usize = 4;

enum RetryCommand {
    Start {
        target: PrMergeTarget,
        branch: String,
        reply: oneshot::Sender<RetryStart>,
    },
    IsPending {
        number: u64,
        reply: oneshot::Sender<bool>,
    },
    Cancel {
        number: u64,
        reply: oneshot::Sender<RetryCancel>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetryStart {
    Started,
    AlreadyRunning,
    AtCapacity,
}

struct RetryFinished {
    number: u64,
    result: RetryOutcome,
}

enum RetryOutcome {
    Armed,
    Failed(String),
    GaveUp,
    Cancelled,
}

#[derive(Clone, Debug)]
pub(super) enum RetryCancel {
    NotPending,
    Cancelled,
    Armed,
    Failed(String),
    GaveUp,
}

fn retry_cancel_result(outcome: &RetryOutcome) -> RetryCancel {
    match outcome {
        RetryOutcome::Armed => RetryCancel::Armed,
        RetryOutcome::Failed(error) => RetryCancel::Failed(error.clone()),
        RetryOutcome::GaveUp => RetryCancel::GaveUp,
        RetryOutcome::Cancelled => RetryCancel::Cancelled,
    }
}

/// Commands to the retry owner.
#[derive(Clone)]
pub(super) struct Retries {
    commands: mpsc::Sender<RetryCommand>,
}

impl Retries {
    /// Starts the single scoped retry owner.
    pub(super) fn start(scope: &TaskScope, client: GithubClient, ledger: WriteLedger) -> Self {
        let (commands, receiver) = mpsc::channel(16);
        spawn_retry_owner(scope, client, ledger, scope.token(), receiver);
        Self { commands }
    }

    /// A handle whose owner has already stopped.
    #[cfg(test)]
    pub(super) fn detached() -> Self {
        let (commands, _) = mpsc::channel(2);
        Self { commands }
    }

    async fn send(&self, command: RetryCommand) -> Result<()> {
        self.commands
            .send(command)
            .await
            .map_err(|_| anyhow::anyhow!("GitHub retry owner has stopped"))
    }

    pub(super) async fn start_arm(
        &self,
        target: PrMergeTarget,
        branch: String,
    ) -> Result<RetryStart> {
        let (reply, receiver) = oneshot::channel();
        self.send(RetryCommand::Start {
            target,
            branch,
            reply,
        })
        .await?;
        Ok(receiver.await.unwrap_or(RetryStart::AtCapacity))
    }

    pub(super) async fn is_pending(&self, number: u64) -> Result<bool> {
        let (reply, receiver) = oneshot::channel();
        self.send(RetryCommand::IsPending { number, reply }).await?;
        Ok(receiver.await.unwrap_or(false))
    }

    pub(super) async fn cancel(&self, number: u64) -> Result<RetryCancel> {
        let (reply, receiver) = oneshot::channel();
        self.send(RetryCommand::Cancel { number, reply }).await?;
        Ok(receiver.await.unwrap_or(RetryCancel::NotPending))
    }
}

fn spawn_retry_owner(
    scope: &TaskScope,
    client: GithubClient,
    ledger: WriteLedger,
    cancellation: CancellationToken,
    mut commands: mpsc::Receiver<RetryCommand>,
) {
    scope.spawn(async move {
        let mut jobs: JoinSet<RetryFinished> = JoinSet::new();
        let mut active: HashMap<u64, CancellationToken> = HashMap::new();
        let mut cancel_waiters: HashMap<u64, Vec<oneshot::Sender<RetryCancel>>> = HashMap::new();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                command = commands.recv() => {
                    let Some(command) = command else { break; };
                    match command {
                        RetryCommand::Start { target, branch, reply } => {
                            let result = if active.contains_key(&target.number) {
                                RetryStart::AlreadyRunning
                            } else if active.len() >= MAX_RETRIES {
                                RetryStart::AtCapacity
                            } else {
                                let token = cancellation.child_token();
                                active.insert(target.number, token.clone());
                                let client = client.clone();
                                let ledger = ledger.clone();
                                jobs.spawn(async move {
                                    let result = retry_arm(client, ledger, target.clone(), branch, token).await;
                                    RetryFinished { number: target.number, result }
                                });
                                RetryStart::Started
                            };
                            let _ = reply.send(result);
                        }
                        RetryCommand::IsPending { number, reply } => {
                            let _ = reply.send(active.contains_key(&number));
                        }
                        RetryCommand::Cancel { number, reply } => {
                            if let Some(token) = active.get(&number) {
                                token.cancel();
                                cancel_waiters.entry(number).or_default().push(reply);
                            } else {
                                let _ = reply.send(RetryCancel::NotPending);
                            }
                        }
                    }
                }
                Some(joined) = jobs.join_next(), if !jobs.is_empty() => {
                    if let Ok(finished) = joined {
                        active.remove(&finished.number);
                        if let Some(waiters) = cancel_waiters.remove(&finished.number) {
                            let result = retry_cancel_result(&finished.result);
                            for waiter in waiters { let _ = waiter.send(result.clone()); }
                        }
                        match finished.result {
                            RetryOutcome::Armed => tracing::info!(pr = finished.number, "armed merge when ready after checks appeared"),
                            RetryOutcome::Failed(error) => tracing::warn!(pr = finished.number, %error, event_channel = "attention", event_text = %format_args!("#{}: merge when ready failed: {error}", finished.number), "background merge-when-ready retry failed"),
                            RetryOutcome::GaveUp => tracing::warn!(pr = finished.number, event_channel = "attention", event_text = %format_args!("#{}: merge when ready gave up waiting for required checks", finished.number), "gave up waiting for required checks to appear"),
                            RetryOutcome::Cancelled => tracing::info!(pr = finished.number, "cancelled pending merge-when-ready retry"),
                        }
                    }
                }
            }
        }
        for token in active.values() { token.cancel(); }
        while let Some(joined) = jobs.join_next().await {
            if let Ok(finished) = joined
                && let Some(waiters) = cancel_waiters.remove(&finished.number)
            {
                let result = retry_cancel_result(&finished.result);
                for waiter in waiters { let _ = waiter.send(result.clone()); }
            }
        }
        for waiters in cancel_waiters.into_values() {
            for waiter in waiters { let _ = waiter.send(RetryCancel::Cancelled); }
        }
    });
}

async fn retry_arm(
    client: GithubClient,
    ledger: WriteLedger,
    target: PrMergeTarget,
    branch: String,
    cancellation: CancellationToken,
) -> RetryOutcome {
    let started = Instant::now();
    loop {
        tokio::select! {
            _ = cancellation.cancelled() => return RetryOutcome::Cancelled,
            _ = sleep(RETRY_EVERY) => {}
        }
        if started.elapsed() >= RETRY_LIMIT {
            return RetryOutcome::GaveUp;
        }
        let result = client.enable_auto_merge(&target, &cancellation).await;
        if result.ok {
            // Recorded so a disarm before the next read still sends disable.
            ledger.accepted(GithubWrite::auto_merge(&branch, target.number, true));
            return RetryOutcome::Armed;
        }
        if result.retryable != Some(true) {
            if result.ambiguous || cancellation.is_cancelled() {
                // It may have armed: guards must not treat it as never sent.
                ledger.uncertain(GithubWrite::auto_merge(&branch, target.number, true));
            }
            return RetryOutcome::Failed(
                result.error.unwrap_or_else(|| "auto-merge failed".into()),
            );
        }
    }
}
