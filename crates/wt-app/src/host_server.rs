//! SSH invokes this host-local service, never a second implementation of the
//! TUI's features. A disconnected reader closes admission, not accepted work.
use crate::{
    context::AppContext,
    host_protocol::{self as protocol, ClientFrame, HostSnapshot, ServerFrame},
    host_service::HostService,
};
use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
use wt_runtime::TaskScope;
use wt_tui::{UiAction, UiReply};

pub async fn run(context: &AppContext) -> Result<i32> {
    if context.config.instance.role != wt_config::InstanceRole::Worker {
        bail!("host service requires [instance] role = \"worker\"");
    }
    serve(
        context,
        crate::host_stdio::Pipe::input()?,
        crate::host_stdio::Pipe::output()?,
    )
    .await?;
    Ok(0)
}

pub async fn serve<R, W>(context: &AppContext, input: R, mut output: W) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut input = BufReader::new(input);
    let hello = tokio::select! {
        _ = context.cancellation.cancelled() => bail!("host handshake cancelled"),
        hello = tokio::time::timeout(Duration::from_secs(10), protocol::read::<ClientFrame, _>(&mut input)) => hello??,
    };
    if !matches!(
        hello,
        Some(ClientFrame::Hello {
            protocol: protocol::HOST_PROTOCOL
        })
    ) {
        bail!("host service protocol mismatch");
    }
    output
        .write_all(&protocol::encode(&ServerFrame::Hello {
            protocol: protocol::HOST_PROTOCOL,
            build: env!("WT_BUILD_ID").into(),
        })?)
        .await?;
    let scope = TaskScope::new();
    let commands = CancellationToken::new();
    let host = Arc::new(HostService::start(
        &scope,
        context.clone(),
        commands.clone(),
    ));
    let disconnected = CancellationToken::new();
    let (replies, mut pending) = tokio::sync::mpsc::channel::<ServerFrame>(8);
    let writer_host = host.clone();
    let writer_closed = disconnected.clone();
    let mut writer = tokio::spawn(async move {
        let result: Result<()> = async {
            let mut updates = writer_host.sources.board.subscribe();
            updates.mark_changed();
            let mut previous = None;
            loop {
                let frame = tokio::select! {
                    biased;
                    reply = pending.recv() => match reply { Some(reply) => reply, None => break },
                    changed = updates.changed() => {
                        if changed.is_err() { break; }
                        updates.borrow_and_update();
                        let snapshot = HostSnapshot::capture(&writer_host);
                        // A refetch that changes nothing sends no frame, unless
                        // a write is settling and needs its newer stamps.
                        if previous.as_ref().is_some_and(|previous: &HostSnapshot| previous.same_frame(&snapshot)) { continue; }
                        previous = Some(snapshot.clone());
                        ServerFrame::Snapshot(snapshot)
                    }
                };
                tokio::time::timeout(
                    Duration::from_secs(15),
                    output.write_all(&protocol::encode(&frame)?),
                )
                .await
                .context("host peer stopped reading")??;
            }
            output.shutdown().await?;
            Ok(())
        }
        .await;
        writer_closed.cancel();
        result
    });
    // Keep frame reads in one owning task: select cancellation must never
    // discard a partially consumed length-prefixed frame.
    let (frames, mut received) = tokio::sync::mpsc::channel(32);
    let reader = tokio::spawn(async move {
        loop {
            let frame = protocol::read::<ClientFrame, _>(&mut input).await;
            let terminal = !matches!(&frame, Ok(Some(_)));
            if frames.send(frame).await.is_err() || terminal {
                break;
            }
        }
    });
    let mut work = tokio::task::JoinSet::new();
    let mut queued = VecDeque::<(crate::controller::Lane, (u64, UiAction))>::new();
    let mut busy = BTreeSet::new();
    let mut running = BTreeMap::new();
    let mut last_id = 0;
    let mut perf_revision = 0;
    let mut result = Ok(());
    let mut closed = false;
    let shutdown_wait = CancellationToken::new();
    loop {
        if closed {
            shutdown_wait.cancel();
            // Read preparations may stop. Every admitted mutation, including
            // queued work, retains ownership and drains before command teardown.
            let mut mutations = VecDeque::new();
            while let Some((lane, (id, action))) = queued.pop_front() {
                if action.is_preparation() {
                    send_reply(
                        &replies,
                        &disconnected,
                        id,
                        UiReply::no_change("Read-only request cancelled during worker shutdown"),
                    )
                    .await;
                } else {
                    mutations.push_back((lane, (id, action)));
                }
            }
            queued = mutations;
            for (_, _, cancellation) in running.values() {
                if let Some(cancellation) = cancellation {
                    CancellationToken::cancel(cancellation);
                }
            }
        }
        while let Some((lane, (id, action))) = crate::controller::take_available(&mut queued, &busy)
        {
            busy.insert(lane.clone());
            let host = host.clone();
            let cancellation = action.is_preparation().then(CancellationToken::new);
            let token = cancellation.clone();
            let shutdown_wait = shutdown_wait.clone();
            let task = work.spawn(async move {
                let outcome = if let Some(token) = token {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => Ok(UiReply::no_change("Read-only request cancelled")),
                        outcome = host.execute(action) => outcome,
                    }
                } else { host.execute_with_shutdown(action, Some(&shutdown_wait)).await };
                outcome.unwrap_or_else(|error| crate::controller::error_reply(&error))
            });
            running.insert(task.id(), (id, lane, cancellation));
        }
        if closed && work.is_empty() && queued.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            _ = context.cancellation.cancelled(), if !closed => closed = true,
            _ = disconnected.cancelled(), if !closed => closed = true,
            completed = work.join_next_with_id(), if !work.is_empty() => {
                let completed = completed.expect("nonempty worker tasks");
                let task_id = match &completed { Ok((id, _)) => *id, Err(error) => error.id() };
                if let Some((id, lane, _)) = running.remove(&task_id) {
                    busy.remove(&lane);
                    let reply = match completed {
                        Ok((_, reply)) => reply,
                        Err(error) => {
                            // A panicked mutation may have changed state. Close
                            // admission; never replay it or release conflicting queued work.
                            result = Err(anyhow::anyhow!("host command worker failed: {error}"));
                            let mut retained = VecDeque::new();
                            while let Some((other, (queued_id, action))) = queued.pop_front() {
                                if crate::controller::lanes_conflict(&lane, &other) {
                                    send_reply(&replies, &disconnected, queued_id, UiReply::failure("Conflicting worker failed; queued command was not started")).await;
                                } else { retained.push_back((other, (queued_id, action))); }
                            }
                            queued = retained;
                            closed = true;
                            UiReply::uncertain(format!("Host command worker failed; its result is unknown: {error}"))
                        }
                    };
                    send_reply(&replies, &disconnected, id, reply).await;
                }
            },
            frame = received.recv(), if !closed => {
                let frame = match frame {
                    Some(Ok(Some(frame))) => frame,
                    Some(Err(error)) => { result = Err(error); closed = true; continue; },
                    Some(Ok(None)) | None => { closed = true; continue; },
                };
                match frame {
                    ClientFrame::Hello { .. } => { result = Err(anyhow::anyhow!("duplicate host handshake")); closed = true; },
                    ClientFrame::Refresh => { host.sources.board.refresh(); },
                    ClientFrame::Shutdown => closed = true,
                    ClientFrame::Views(views) => {
                        host.sources.history.set_active(views.history);
                        host.sources.perf.set_active(views.perf);
                        host.sources.perf.set_continuous(views.perf_continuous);
                        if views.perf_revision != perf_revision { perf_revision = views.perf_revision; host.sources.perf.refresh(); }
                    },
                    ClientFrame::CancelPreparation { id } => {
                        if let Some(position) = queued.iter().position(|(_, (queued_id, action))| *queued_id == id && action.is_preparation()) {
                            queued.remove(position);
                            send_reply(&replies, &disconnected, id, UiReply::no_change("Read-only request cancelled")).await;
                        }
                        for (running_id, _, token) in running.values() {
                            if *running_id == id && let Some(token) = token { token.cancel(); }
                        }
                    },
                    ClientFrame::Command { id, action } => {
                        let action = crate::host_dispatch::rename_as_title(action);
                        if id <= last_id { result = Err(anyhow::anyhow!("host command IDs must increase; replay is refused")); closed = true; continue; }
                        last_id = id;
                        let lane = if crate::host_routing::controller_owned(&action) || matches!(action, UiAction::OnHost { .. }) {
                            Err(anyhow::anyhow!("controller-owned action cannot run on the worker"))
                        } else if queued.len() >= 32 {
                            Err(anyhow::anyhow!("worker command queue is full; command was not started"))
                        } else { crate::controller::lane_for(&action) };
                        match lane {
                            Ok(lane) => queued.push_back((lane, (id, action))),
                            Err(error) => { send_reply(&replies, &disconnected, id, UiReply::failure(format!("{error:#}"))).await; },
                        }
                    },
                }
            }
        }
    }
    reader.abort();
    let _ = reader.await;
    drop(replies);
    scope.cancel();
    // Detached actions and the user's tmux sessions outlive this host stream.
    let automations_finished = host.shutdown_automations().await;
    commands.cancel();
    let write_result = match tokio::time::timeout(Duration::from_secs(5), &mut writer).await {
        Ok(result) => result.context("host writer exited")?,
        Err(_) => {
            writer.abort();
            let _ = writer.await;
            Err(anyhow::anyhow!("host writer drain timed out"))
        }
    };
    let stopped = scope.shutdown(Duration::from_secs(5)).await;
    result?;
    automations_finished?;
    write_result?;
    stopped?;
    Ok(())
}

async fn send_reply(
    replies: &tokio::sync::mpsc::Sender<ServerFrame>,
    disconnected: &CancellationToken,
    id: u64,
    reply: UiReply,
) {
    tokio::select! {
        biased;
        _ = disconnected.cancelled() => {},
        result = tokio::time::timeout(Duration::from_secs(15), replies.send(ServerFrame::Reply { id, reply })) => {
            if !matches!(result, Ok(Ok(()))) { disconnected.cancel(); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn worker_stream_uses_the_same_title_handler_and_publishes_the_result() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let context = fixture.ctx.clone();
        let (client, worker) = tokio::io::duplex(64 * 1024);
        let (input, output) = tokio::io::split(worker);
        let task = tokio::spawn(async move { serve(&context, input, output).await });
        let (input, mut output) = tokio::io::split(client);
        let mut input = BufReader::new(input);
        output
            .write_all(
                &protocol::encode(&ClientFrame::Hello {
                    protocol: protocol::HOST_PROTOCOL,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            protocol::read::<ServerFrame, _>(&mut input).await.unwrap(),
            Some(ServerFrame::Hello { .. })
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(ServerFrame::Snapshot(snapshot)) =
                    protocol::read::<ServerFrame, _>(&mut input).await.unwrap()
                    && snapshot
                        .board
                        .is_some_and(|board| board.rows.iter().any(|row| row.key == "one"))
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        output
            .write_all(
                &protocol::encode(&ClientFrame::Command {
                    id: 1,
                    action: UiAction::SetTitle {
                        key: "one".into(),
                        title: "Title through the host service".into(),
                    },
                })
                .unwrap(),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            let (mut replied, mut published) = (false, false);
            while !replied || !published {
                match protocol::read::<ServerFrame, _>(&mut input)
                    .await
                    .unwrap()
                    .unwrap()
                {
                    ServerFrame::Reply { id: 1, reply } => {
                        assert!(!reply.failed, "{}", reply.message);
                        replied = true;
                    }
                    ServerFrame::Snapshot(snapshot) => {
                        published |= snapshot.board.is_some_and(|board| {
                            board.rows.iter().any(|row| {
                                row.key == "one" && row.title == "Title through the host service"
                            })
                        });
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        // Graceful shutdown closes admission while keeping the reader alive
        // for any already-prepared snapshots. Dropping both halves instead
        // races those writes with BrokenPipe and is an abrupt disconnect.
        tokio::time::timeout(Duration::from_secs(10), async {
            output
                .write_all(&protocol::encode(&ClientFrame::Shutdown).unwrap())
                .await
                .unwrap();
            while protocol::read::<ServerFrame, _>(&mut input)
                .await
                .unwrap()
                .is_some()
            {}
            task.await.unwrap().unwrap();
        })
        .await
        .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(
            state["slugs"]["one"]["manualTitle"],
            "Title through the host service"
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn accepted_command_finishes_after_controller_disconnects() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let context = fixture.ctx.clone();
        let (client, worker) = tokio::io::duplex(64 * 1024);
        let (worker_input, worker_output) = tokio::io::split(worker);
        let task = tokio::spawn(async move { serve(&context, worker_input, worker_output).await });
        let (client_input, mut client_output) = tokio::io::split(client);
        let mut client_input = BufReader::new(client_input);
        client_output
            .write_all(
                &protocol::encode(&ClientFrame::Hello {
                    protocol: protocol::HOST_PROTOCOL,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            protocol::read::<ServerFrame, _>(&mut client_input)
                .await
                .unwrap(),
            Some(ServerFrame::Hello { .. })
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(ServerFrame::Snapshot(snapshot)) =
                    protocol::read::<ServerFrame, _>(&mut client_input)
                        .await
                        .unwrap()
                    && snapshot
                        .board
                        .is_some_and(|board| board.rows.iter().any(|row| row.key == "one"))
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        // Hold the real state actor, making the first mutation stay active.
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let database = fixture.ctx.database.clone();
        let blocker = tokio::spawn(async move {
            database
                .call(move |_| {
                    let _ = entered.send(());
                    held.recv_timeout(Duration::from_secs(10)).unwrap();
                    Ok(())
                })
                .await
                .unwrap();
        });
        entered_rx.await.unwrap();
        client_output
            .write_all(
                &protocol::encode(&ClientFrame::Command {
                    id: 1,
                    action: UiAction::SetTitle {
                        key: "one".into(),
                        title: "Completed after disconnect".into(),
                    },
                })
                .unwrap(),
            )
            .await
            .unwrap();
        // Cancellation cannot cancel a write; a second read-only prompt must
        // reply while that accepted write is still blocked.
        for frame in [
            ClientFrame::CancelPreparation { id: 1 },
            ClientFrame::Command {
                id: 2,
                action: UiAction::PrepareHardRefresh,
            },
        ] {
            client_output
                .write_all(&protocol::encode(&frame).unwrap())
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match protocol::read::<ServerFrame, _>(&mut client_input)
                    .await
                    .unwrap()
                    .unwrap()
                {
                    ServerFrame::Reply { id: 2, reply } => {
                        assert!(!reply.failed, "{}", reply.message);
                        assert!(reply.modal.is_some());
                        break;
                    }
                    ServerFrame::Reply { id: 1, .. } => {
                        panic!("held mutation must not complete before its independent menu")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        // A read blocked on the same state actor can be cancelled without
        // cancelling the accepted write or blocking the next preparation.
        for frame in [
            ClientFrame::Command {
                id: 3,
                action: UiAction::PrepareStatus { key: "one".into() },
            },
            ClientFrame::CancelPreparation { id: 3 },
            ClientFrame::Command {
                id: 4,
                action: UiAction::PrepareHardRefresh,
            },
        ] {
            client_output
                .write_all(&protocol::encode(&frame).unwrap())
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            let (mut cancelled, mut next_ready) = (false, false);
            while !cancelled || !next_ready {
                match protocol::read::<ServerFrame, _>(&mut client_input)
                    .await
                    .unwrap()
                    .unwrap()
                {
                    ServerFrame::Reply { id: 3, reply } => {
                        assert!(!reply.failed);
                        assert_eq!(reply.outcome, Some(wt_tui::ReplyOutcome::NoChange));
                        cancelled = true;
                    }
                    ServerFrame::Reply { id: 4, reply } => {
                        assert!(!reply.failed);
                        assert!(reply.modal.is_some());
                        next_ready = true;
                    }
                    ServerFrame::Reply { id: 1, .. } => {
                        panic!("cancelling read preparation must not release the held mutation")
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        // Drop both halves only after the full frame was flushed. The worker
        // must finish accepted work even though it can no longer reply.
        drop(client_input);
        drop(client_output);
        release.send(()).unwrap();
        blocker.await.unwrap();
        let serve_result = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            serve_result.is_err(),
            "a closed reply pipe must be reported"
        );
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(
            state["slugs"]["one"]["manualTitle"],
            "Completed after disconnect"
        );
        fixture.close().await.unwrap();
    }
}
