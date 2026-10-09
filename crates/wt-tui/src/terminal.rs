use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};

use crossterm::{
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio_util::sync::CancellationToken;
use wt_runtime::SourceHandle;

use crate::{Board, Model, TerminalHandoff, UiActions, model::InputResult, render::render};

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        ) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self { active: true })
    }

    fn suspend(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        disable_raw_mode()?;
        if let Err(error) = execute!(
            io::stdout(),
            DisableBracketedPaste,
            Show,
            LeaveAlternateScreen
        ) {
            let _ = enable_raw_mode();
            let _ = execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableBracketedPaste,
                Hide
            );
            return Err(error);
        }
        self.active = false;
        Ok(())
    }

    fn resume(&mut self) -> io::Result<()> {
        if self.active {
            return Ok(());
        }
        enable_raw_mode()?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        ) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        self.active = true;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            Show,
            LeaveAlternateScreen
        );
    }
}

/// Run on the runtime's calling thread. Background sources execute on runtime
/// workers. No timer drives rendering and no source is fetched from this loop.
pub async fn run(
    source: SourceHandle<Board>,
    mut actions: UiActions,
    cancel: CancellationToken,
) -> io::Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Err(io::Error::other(
            "the TUI requires a terminal; use wt --help to list commands",
        ));
    }
    let mut guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut events = EventStream::new();
    let mut snapshots = source.subscribe();
    let mut model = Model::default();
    model.apply(snapshots.borrow_and_update().clone());
    let mut dirty = true;
    let mut input_at = None;
    let mut latencies = Vec::with_capacity(1024);
    let mut metric_at = Instant::now();
    let mut controller_open = true;
    let mut toast_until = None;
    source.refresh();
    loop {
        if dirty {
            let start = Instant::now();
            terminal.draw(|frame| render(frame, &mut model))?;
            model.last_frame_micros = start.elapsed().as_micros();
            model.frame_count += 1;
            if let Some(at) = input_at.take() {
                let elapsed: Duration = Instant::now().duration_since(at);
                latencies.push(elapsed.as_secs_f64() * 1000.0);
                if latencies.len() >= 1024 || metric_at.elapsed() >= Duration::from_secs(60) {
                    log_latencies(&mut latencies);
                    metric_at = Instant::now();
                }
            }
            dirty = false;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            event = events.next() => {
                let Some(event) = event else { break; };
                match event? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        let received = Instant::now();
                        match model.input(key, terminal.size()?.height.saturating_sub(4) as usize) {
                            InputResult::Quit => break,
                            InputResult::Refresh => { source.refresh(); }
                            InputResult::Draw => { dirty = true; input_at = Some(received); }
                            InputResult::Unchanged => {}
                            InputResult::Action(action) => {
                                match actions.requests.try_send(action) {
                                    Ok(()) => model.toast = Some(("Working…".into(), false)),
                                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => model.toast = Some(("Actions are busy; try again when one finishes".into(), true)),
                                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => model.toast = Some(("Action worker stopped; restart wt".into(), true)),
                                }
                                dirty = true;
                                toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(4));
                                input_at = Some(received);
                            }
                        }
                    }
                    Event::Resize(_, _) => dirty = true,
                    Event::Paste(text) => dirty |= model.paste(&text),
                    _ => {}
                }
            }
            reply = actions.replies.recv(), if controller_open => {
                if let Some(reply) = reply {
                    let handoff = model.reply(reply);
                    toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(4));
                    dirty = true;
                    if let Some(ticket) = handoff {
                        drop(events);
                        match suspend_for_handoff(ticket, &mut guard, &cancel).await? {
                            HandoffResult::Resumed(Ok(())) => {
                                // Fullscreen resize clears both render buffers
                                // without querying the terminal cursor. `clear`
                                // would synchronously read input after handoff.
                                terminal.resize(terminal.size()?.into())?;
                                events = EventStream::new();
                                dirty = true;
                            }
                            HandoffResult::Resumed(Err(message)) => {
                                terminal.resize(terminal.size()?.into())?;
                                events = EventStream::new();
                                model.toast = Some((message, true));
                                dirty = true;
                            }
                            HandoffResult::Cancelled => {
                                break;
                            }
                        }
                    }
                } else {
                    controller_open = false;
                }
            }
            _ = async {
                if let Some(deadline) = toast_until { tokio::time::sleep_until(deadline).await; }
                else { std::future::pending::<()>().await; }
            } => {
                model.toast = None;
                toast_until = None;
                dirty = true;
            }
            changed = snapshots.changed() => {
                if changed.is_err() { break; }
                model.apply(snapshots.borrow_and_update().clone());
                dirty = true;
            }
        }
    }
    log_latencies(&mut latencies);
    tracing::info!(frames = model.frame_count, "terminal stopped");
    Ok(())
}

enum HandoffResult {
    Resumed(Result<(), String>),
    Cancelled,
}

async fn suspend_for_handoff(
    ticket: TerminalHandoff,
    guard: &mut TerminalGuard,
    cancel: &CancellationToken,
) -> io::Result<HandoffResult> {
    guard.suspend()?;
    if ticket.ready.send(()).is_err() {
        guard.resume()?;
        return Ok(HandoffResult::Resumed(Err(
            "session handoff could not start".into(),
        )));
    }
    let result = tokio::select! {
        _ = cancel.cancelled() => HandoffResult::Cancelled,
        result = ticket.resumed => HandoffResult::Resumed(
            result.unwrap_or_else(|_| Err("session handoff ended without a result".into()))
        ),
    };
    // Even cancellation restores the screen before the UI loop exits. Dropping
    // the receiver above tells the controller to terminate and reap its child.
    guard.resume()?;
    Ok(result)
}

fn log_latencies(samples: &mut Vec<f64>) {
    if samples.is_empty() {
        return;
    }
    samples.sort_by(f64::total_cmp);
    tracing::info!(
        n = samples.len(),
        p50_ms = samples[samples.len() / 2],
        p90_ms = samples[(samples.len() * 9 / 10).min(samples.len() - 1)],
        max_ms = samples[samples.len() - 1],
        "input-latency"
    );
    samples.clear();
}
