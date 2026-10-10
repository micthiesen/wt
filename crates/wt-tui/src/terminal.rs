use std::collections::VecDeque;
use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};
use std::{future::Future, pin::Pin};

use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
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
    keyboard_supported: bool,
    keyboard_enhanced: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture,
            Hide
        ) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self {
            active: true,
            keyboard_supported: false,
            keyboard_enhanced: false,
        })
    }

    fn enable_keyboard_enhancements(&mut self, supported: bool) -> io::Result<()> {
        self.keyboard_supported = supported;
        if supported && !self.keyboard_enhanced {
            execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
            self.keyboard_enhanced = true;
        }
        Ok(())
    }

    fn suspend(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        disable_raw_mode()?;
        if self.keyboard_enhanced {
            execute!(io::stdout(), PopKeyboardEnhancementFlags)?;
            self.keyboard_enhanced = false;
        }
        if let Err(error) = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            Show,
            LeaveAlternateScreen
        ) {
            let _ = enable_raw_mode();
            let _ = execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableMouseCapture,
                Hide
            );
            let _ = self.enable_keyboard_enhancements(self.keyboard_supported);
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
            EnableMouseCapture,
            Hide
        ) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        if let Err(error) = self.enable_keyboard_enhancements(self.keyboard_supported) {
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                DisableBracketedPaste,
                DisableMouseCapture,
                Show,
                LeaveAlternateScreen
            );
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
        if self.keyboard_enhanced {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
            self.keyboard_enhanced = false;
        }
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            Show,
            LeaveAlternateScreen
        );
    }
}

/// Run on the runtime's calling thread. Background sources execute on runtime
/// workers. No timer drives rendering and no source is fetched from this loop.
pub async fn run(
    source: SourceHandle<Board>,
    actions: UiActions,
    cancel: CancellationToken,
) -> io::Result<()> {
    run_inner(source, actions, cancel, None).await
}

/// Run the TUI and query OSC 10/11 colors while wt owns the raw terminal.
/// Any ordinary key events received during the bounded query are replayed
/// before the event stream begins consuming new input.
pub async fn run_with_palette_probe<'a, F, Fut>(
    source: SourceHandle<Board>,
    actions: UiActions,
    cancel: CancellationToken,
    on_palette: F,
) -> io::Result<()>
where
    F: FnOnce(Option<(String, String)>) -> Fut + 'a,
    Fut: Future<Output = ()> + 'a,
{
    let callback: PaletteCallback<'a> = Box::new(move |colors| Box::pin(on_palette(colors)));
    run_inner(source, actions, cancel, Some(callback)).await
}

type PaletteCallback<'a> =
    Box<dyn FnOnce(Option<(String, String)>) -> Pin<Box<dyn Future<Output = ()> + 'a>> + 'a>;

async fn run_inner<'a>(
    source: SourceHandle<Board>,
    mut actions: UiActions,
    cancel: CancellationToken,
    on_palette: Option<PaletteCallback<'a>>,
) -> io::Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Err(io::Error::other(
            "the TUI requires a terminal; use wt --help to list commands",
        ));
    }
    let mut guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut pending_events = VecDeque::new();
    if let Some(on_palette) = on_palette {
        match crate::terminal_probe::query().await {
            Ok(probe) => {
                guard.enable_keyboard_enhancements(probe.keyboard_supported)?;
                on_palette(probe.palette).await;
                pending_events.extend(probe.events);
            }
            Err(error) => {
                tracing::debug!(%error, "terminal color probe failed");
                on_palette(None).await;
            }
        }
    }
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
    let mut log_refresh_at: Option<tokio::time::Instant> = None;
    source.refresh();
    loop {
        // A Log overlay replaced by another interaction leaves nothing to
        // refresh; stop waking for it and forget its close key.
        if !matches!(model.interaction, crate::Interaction::Log { .. }) {
            model.log_refresh = None;
            model.log_close_key = None;
        }
        if model.log_refresh.is_none() {
            log_refresh_at = None;
        } else if log_refresh_at.is_none() {
            log_refresh_at = Some(tokio::time::Instant::now() + Duration::from_secs(1));
        }
        if dirty {
            let start = Instant::now();
            let mut copied = None;
            terminal.draw(|frame| {
                render(frame, &mut model);
                // Drag selection: highlight while the button is held; on
                // release, copy the rendered text and clear the highlight.
                if let Some(selection) = model.mouse_selection {
                    if selection.finished {
                        copied = Some(crate::mouse::extract(frame.buffer_mut(), &selection));
                        model.mouse_selection = None;
                    } else {
                        crate::mouse::highlight(frame.buffer_mut(), &selection);
                    }
                }
            })?;
            if let Some(text) = copied.filter(|text| !text.trim().is_empty()) {
                let chars = text.chars().count();
                let lines = text.lines().count();
                let label = if lines > 1 {
                    format!("{chars} chars ({lines} lines)")
                } else {
                    format!("{chars} chars")
                };
                let action = crate::UiAction::Copy { value: text, label };
                if actions
                    .requests
                    .try_send(crate::UiRequest {
                        generation: model.ui_generation,
                        action,
                    })
                    .is_err()
                {
                    model.toast = Some(("Actions are busy; selection not copied".into(), true));
                    toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(4));
                    terminal.draw(|frame| render(frame, &mut model))?;
                }
            }
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
            event = async {
                if let Some(event) = pending_events.pop_front() {
                    Some(Ok(event))
                } else {
                    events.next().await
                }
            } => {
                let Some(event) = event else { break; };
                match event? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        let received = Instant::now();
                        let toast_before = model.toast.clone();
                        let result = model.input(key, terminal.size()?.height.saturating_sub(4) as usize);
                        if model.toast.is_some() && model.toast != toast_before {
                            // Input-side feedback ("nothing needs you") expires
                            // like any other toast.
                            toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                        }
                        match result {
                            InputResult::Quit => break,
                            InputResult::Refresh => {
                                source.refresh();
                                model.toast = Some(("Refreshing…".into(), false));
                                toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                                dirty = true;
                                input_at = Some(received);
                            }
                            InputResult::Draw => { dirty = true; input_at = Some(received); }
                            InputResult::Unchanged => {}
                            InputResult::Action(action) => {
                                match actions.requests.try_send(crate::UiRequest { generation: model.ui_generation, action }) {
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
                    Event::Mouse(mouse) => {
                        let area = terminal.size()?.into();
                        dirty |= crate::mouse::select(&mut model, mouse, area);
                        dirty |= crate::mouse::scroll(&mut model, mouse, area);
                    }
                    Event::Paste(text) => dirty |= model.paste(&text),
                    _ => {}
                }
            }
            reply = actions.replies.recv(), if controller_open => {
                if let Some(reply) = reply {
                    let history_was_open = model.history.active;
                    let perf_was_open = model.show_perf;
                    let handoff = model.reply(reply);
                    if history_was_open && !model.history.active {
                        let _ = actions.requests.try_send(crate::UiRequest {
                            generation: model.ui_generation,
                            action: crate::UiAction::SetHistoryActive { active: false },
                        });
                    }
                    if perf_was_open && !model.show_perf {
                        let _ = actions.requests.try_send(crate::UiRequest {
                            generation: model.ui_generation,
                            action: crate::UiAction::SetPerf {
                                active: false,
                                continuous: model.perf_continuous,
                                refresh: false,
                            },
                        });
                    }
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
            // A live Log overlay (`! l` dev logs) re-requests its lines about
            // once a second while it stays open. Its generation is the
            // current one, so any key pressed meanwhile drops the reply.
            _ = async {
                match log_refresh_at {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            }, if model.log_refresh.is_some() => {
                log_refresh_at = None;
                if let Some(action) = model.log_refresh.clone()
                    && matches!(model.interaction, crate::Interaction::Log { .. })
                {
                    let _ = actions.requests.try_send(crate::UiRequest {
                        generation: model.ui_generation,
                        action,
                    });
                }
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
