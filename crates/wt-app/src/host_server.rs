//! SSH invokes this host-local service, never a second implementation of the
//! TUI's features. A disconnected reader closes admission, not accepted work.
use crate::{
    context::AppContext,
    host_protocol::{self as protocol, ClientFrame, HostSnapshot, ServerFrame},
    host_service::HostService,
};
use anyhow::{Context, Result, bail};
use std::{sync::Arc, time::Duration};
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
                        if previous.as_ref() == Some(&snapshot) { continue; }
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
    let result: Result<()> = async {
        let mut last_id = 0;
        let mut perf_revision = 0;
        loop {
            // Only one read future exists at a time; a partial frame is never
            // abandoned merely because a snapshot changed.
            let frame = tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => break,
                _ = disconnected.cancelled() => break,
                frame = protocol::read::<ClientFrame, _>(&mut input) => frame?,
            };
            match frame {
                None => break,
                Some(ClientFrame::Hello { .. }) => bail!("duplicate host handshake"),
                Some(ClientFrame::Refresh) => {
                    host.sources.board.refresh();
                }
                Some(ClientFrame::Views(views)) => {
                    host.sources.history.set_active(views.history);
                    host.sources.perf.set_active(views.perf);
                    host.sources.perf.set_continuous(views.perf_continuous);
                    if views.perf_revision != perf_revision {
                        perf_revision = views.perf_revision;
                        host.sources.perf.refresh();
                    }
                }
                Some(ClientFrame::Command { id, action }) => {
                    if id <= last_id {
                        bail!("host command IDs must increase; replay is refused");
                    }
                    last_id = id;
                    let result = if crate::host_routing::controller_owned(&action)
                        || matches!(action, UiAction::OnHost { .. })
                    {
                        Err(anyhow::anyhow!(
                            "controller-owned action cannot run on the worker"
                        ))
                    } else {
                        host.execute(action).await
                    };
                    let reply = match result {
                        Ok(reply) => reply,
                        Err(error) => UiReply {
                            failed: true,
                            message: format!("{error:#}"),
                            ..Default::default()
                        },
                    };
                    if replies
                        .send(ServerFrame::Reply { id, reply })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    // No accepted write remains. Read sources can now stop independently of
    // detached actions and the user's tmux sessions.
    drop(replies);
    scope.cancel();
    // The command loop is sequential and has returned only after its accepted
    // command finished. Stop source fetches now instead of letting a slow
    // writer drain keep their I/O alive for the full writer grace period.
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
    write_result?;
    stopped?;
    Ok(())
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
        // The stream is a persistent protocol session. End it explicitly so
        // this test exercises the server's graceful disconnect path instead
        // of waiting forever for a second command.
        drop(input);
        drop(output);
        // The server allows five seconds for owned source cleanup. The test
        // must not impose a shorter deadline while watchers/children drain.
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
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
        // Drop both halves only after the full frame was flushed. The worker
        // must finish accepted work even though it can no longer reply.
        drop(client_input);
        drop(client_output);
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
