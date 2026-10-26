use std::cell::Cell;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::sync::Once;

use crate::model::ToastTone;
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

/// Terminal modes the UI thread has entered. They live in a thread-local,
/// not in the guard, so the panic hook can leave them before the panic
/// message prints: a message written to the alternate screen in raw mode is
/// lost when the screen is restored.
#[derive(Clone, Copy)]
struct Modes {
    active: bool,
    keyboard_supported: bool,
    keyboard_enhanced: bool,
}

thread_local! {
    static MODES: Cell<Modes> = const {
        Cell::new(Modes {
            active: false,
            keyboard_supported: false,
            keyboard_enhanced: false,
        })
    };
}

fn update_modes(change: impl FnOnce(&mut Modes)) {
    MODES.with(|cell| {
        let mut modes = cell.get();
        change(&mut modes);
        cell.set(modes);
    });
}

/// Leave every mode this thread entered. A no-op when nothing is active, so
/// the panic hook and the guard's drop never restore twice (a second
/// `LeaveAlternateScreen` would move the cursor back over the panic message).
fn restore_terminal() {
    let modes = MODES.get();
    if !modes.active {
        return;
    }
    let _ = disable_raw_mode();
    if modes.keyboard_enhanced {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        Show,
        LeaveAlternateScreen
    );
    update_modes(|modes| {
        modes.active = false;
        modes.keyboard_enhanced = false;
    });
}

/// Restore the terminal before the previous hook prints a panic raised on the
/// UI thread. Other threads' panics leave the live screen alone: Tokio
/// catches worker panics and the TUI keeps running.
fn install_panic_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}

/// Owns the UI thread's terminal modes. It is `!Send`: the modes it restores
/// belong to the thread that entered them.
struct TerminalGuard {
    _thread: PhantomData<*const ()>,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        install_panic_hook();
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
        MODES.set(Modes {
            active: true,
            keyboard_supported: false,
            keyboard_enhanced: false,
        });
        Ok(Self {
            _thread: PhantomData,
        })
    }

    fn enable_keyboard_enhancements(&mut self, supported: bool) -> io::Result<()> {
        update_modes(|modes| modes.keyboard_supported = supported);
        if supported && !MODES.get().keyboard_enhanced {
            execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
            update_modes(|modes| modes.keyboard_enhanced = true);
        }
        Ok(())
    }

    fn suspend(&mut self) -> io::Result<()> {
        if !MODES.get().active {
            return Ok(());
        }
        disable_raw_mode()?;
        if MODES.get().keyboard_enhanced {
            execute!(io::stdout(), PopKeyboardEnhancementFlags)?;
            update_modes(|modes| modes.keyboard_enhanced = false);
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
            let _ = self.enable_keyboard_enhancements(MODES.get().keyboard_supported);
            return Err(error);
        }
        update_modes(|modes| modes.active = false);
        Ok(())
    }

    fn resume(&mut self) -> io::Result<()> {
        if MODES.get().active {
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
        update_modes(|modes| modes.active = true);
        if let Err(error) = self.enable_keyboard_enhancements(MODES.get().keyboard_supported) {
            restore_terminal();
            return Err(error);
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
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
    let mut wave_at: Option<tokio::time::Instant> = None;
    let mut screen: Option<ratatui::buffer::Buffer> = None;
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
            let drawn = terminal.draw(|frame| {
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
            screen = Some(drawn.buffer.clone());
            if let Some(buffer) = &screen {
                write_hyperlinks(terminal.backend_mut(), buffer, &model.hyperlinks)?;
            }
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
                        id: 0,
                        cancellation: None,
                        generation: model.ui_generation,
                        action,
                    })
                    .is_err()
                {
                    model.toast = Some((
                        "Actions are busy; selection not copied".into(),
                        ToastTone::Failed,
                    ));
                    toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(4));
                    screen = Some(
                        terminal
                            .draw(|frame| render(frame, &mut model))?
                            .buffer
                            .clone(),
                    );
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
        let wave_count = crate::render::refreshing_count(&model);
        let wave_visible = crate::render::wave_visible(&model)
            && screen
                .as_ref()
                .is_some_and(|screen| screen.area.width >= 20 && screen.area.height >= 5);
        if wave_visible && model.refresh_wave.active(wave_count) {
            wave_at
                .get_or_insert_with(|| tokio::time::Instant::now() + crate::refresh_wave::PERIOD);
        } else {
            wave_at = None;
        }
        let pending_deadline = model.next_deadline();
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
                        send_background(&mut model, &actions.requests);
                        if model.toast.is_some() && model.toast != toast_before {
                            // Input-side feedback ("nothing needs you") expires
                            // like any other toast.
                            toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                        }
                        match result {
                            InputResult::Quit => break,
                            InputResult::Refresh => {
                                source.refresh();
                                model.toast = Some(("Refreshing…".into(), ToastTone::Info));
                                toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                                dirty = true;
                                input_at = Some(received);
                            }
                            InputResult::Draw => { dirty = true; input_at = Some(received); }
                            InputResult::Unchanged => {}
                            InputResult::Action(action) => {
                                let id = model.next_op_id();
                                let action = model.resolve(action);
                                let feedback = action.clone();
                                let cancellation = action.is_cancellable().then(CancellationToken::new);
                                match actions.requests.try_send(crate::UiRequest { id, generation: model.ui_generation, action, cancellation: cancellation.clone() }) {
                                    Ok(()) => {
                                        model.begin_action(id, &feedback);
                                        if let Some(request) = &mut model.pending_request {
                                            request._cancel_on_drop = cancellation.map(CancellationToken::drop_guard);
                                        }
                                        model.toast = Some((if model.pending_request.as_ref().is_some_and(|request| request.preparation) { "Loading menu… Esc cancels" } else { "Working…" }.into(), ToastTone::Info));
                                    },
                                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => { model.request_not_sent(); model.toast = Some(("Actions are busy; try again when one finishes".into(), ToastTone::Failed)) },
                                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => { model.request_not_sent(); model.toast = Some(("Action worker stopped; restart wt".into(), ToastTone::Failed)) },
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
                        // A click can move the selection; plan its prefetch.
                        send_background(&mut model, &actions.requests);
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
                    send_background(&mut model, &actions.requests);
                    if std::mem::take(&mut model.refresh_requested) {
                        source.refresh();
                    }
                    if let Some(action) = model.follow_up.take() {
                        let id = model.next_op_id();
                        let action = model.resolve(action);
                        let feedback = action.clone();
                        let cancellation = action.is_cancellable().then(CancellationToken::new);
                        if actions.requests.try_send(crate::UiRequest { id, generation: model.ui_generation, action, cancellation: cancellation.clone() }).is_err() {
                            model.request_not_sent();
                            model.toast = Some(("Action queue is busy; retry the action".into(), ToastTone::Failed));
                        } else {
                            model.begin_action(id, &feedback);
                            if let Some(request) = &mut model.pending_request {
                                request._cancel_on_drop = cancellation.map(CancellationToken::drop_guard);
                            }
                            model.toast = Some((if model.pending_request.as_ref().is_some_and(|request| request.preparation) { "Loading menu… Esc cancels" } else { "Working…" }.into(), ToastTone::Info));
                        }
                    }
                    if history_was_open && !model.history.active {
                        let _ = actions.requests.try_send(crate::UiRequest {
                            id: 0,
                        cancellation: None,
                            generation: model.ui_generation,
                            action: crate::UiAction::SetHistoryActive { active: false },
                        });
                    }
                    if perf_was_open && !model.show_perf {
                        let _ = actions.requests.try_send(crate::UiRequest {
                            id: 0,
                        cancellation: None,
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
                                model.toast = Some((message, ToastTone::Failed));
                                dirty = true;
                            }
                            HandoffResult::Cancelled => {
                                break;
                            }
                        }
                    }
                } else {
                    controller_open = false;
                    model.pending_request = None;
                    model.toast = Some(("Action worker stopped; restart wt".into(), ToastTone::Failed));
                    dirty = true;
                }
            }
            _ = async {
                if let Some(deadline) = toast_until { tokio::time::sleep_until(deadline).await; }
                else { std::future::pending::<()>().await; }
            } => {
                model.expire_toast();
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
                        id: 0,
                        cancellation: None,
                        generation: model.ui_generation,
                        action,
                    });
                }
            }
            // A pending change reached its deadline: stop showing it, and say
            // so when the confirmed data still disagrees.
            _ = async {
                match pending_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let before = model.toast.clone();
                model.tick(tokio::time::Instant::now());
                send_background(&mut model, &actions.requests);
                if model.toast != before {
                    toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(4));
                }
                dirty = true;
            }
            changed = snapshots.changed() => {
                if changed.is_err() { break; }
                let before = model.toast.clone();
                model.apply(snapshots.borrow_and_update().clone());
                send_background(&mut model, &actions.requests);
                if model.toast != before {
                    toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(4));
                }
                dirty = true;
            }
            // No timer at rest. Motion reuses the last complete screen and
            // only rebuilds its one-row header; sources and pane models are
            // untouched. Scheduling from now skips missed frames after a busy
            // input burst or terminal handoff rather than catching up.
            _ = async {
                match wave_at {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                wave_at = None;
                model.refresh_wave.tick(wave_count);
                if let Some(retained) = &screen {
                    let mut resized = false;
                    let drawn = terminal.draw(|frame| {
                        if frame.area() == retained.area {
                            frame.buffer_mut().clone_from(retained);
                            crate::render::refresh_header(frame, &model);
                        } else {
                            // A resize can precede its input event.
                            resized = true;
                            render(frame, &mut model);
                        }
                    })?;
                    if resized {
                        screen = Some(drawn.buffer.clone());
                    }
                    if let Some(buffer) = &screen {
                        write_hyperlinks(terminal.backend_mut(), buffer, &model.hyperlinks)?;
                    }
                    model.frame_count += 1;
                }
            }
        }
    }
    log_latencies(&mut latencies);
    tracing::info!(frames = model.frame_count, "terminal stopped");
    Ok(())
}

/// Rewrite each linked cell wrapped in an OSC 8 hyperlink, with the cell's
/// own colors and modifiers. ratatui cannot carry the escape in a cell (it
/// would count as display width), so this runs after every draw, inside a
/// saved cursor and attribute state.
fn write_hyperlinks(
    backend: &mut impl io::Write,
    buffer: &ratatui::buffer::Buffer,
    links: &[crate::render::Hyperlink],
) -> io::Result<()> {
    use crossterm::{
        cursor::{MoveTo, RestorePosition, SavePosition},
        queue,
        style::{Attribute, Print, SetAttribute, SetBackgroundColor, SetForegroundColor},
    };
    use ratatui::{backend::IntoCrossterm, style::Modifier};
    if links.is_empty() {
        return Ok(());
    }
    queue!(backend, SavePosition)?;
    for link in links {
        if link.url.chars().any(char::is_control) {
            continue;
        }
        let area = link.area.intersection(buffer.area);
        for x in area.left()..area.right() {
            let cell = &buffer[(x, area.y)];
            queue!(
                backend,
                MoveTo(x, area.y),
                SetAttribute(Attribute::Reset),
                SetForegroundColor(cell.fg.into_crossterm()),
                SetBackgroundColor(cell.bg.into_crossterm()),
            )?;
            if cell.modifier.contains(Modifier::UNDERLINED) {
                queue!(backend, SetAttribute(Attribute::Underlined))?;
            }
            if cell.modifier.contains(Modifier::BOLD) {
                queue!(backend, SetAttribute(Attribute::Bold))?;
            }
            queue!(
                backend,
                Print(format!(
                    "\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\",
                    link.url,
                    cell.symbol()
                ))
            )?;
        }
    }
    queue!(backend, SetAttribute(Attribute::Reset), RestorePosition)?;
    io::Write::flush(backend)
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

/// Queue slots background reads leave free for foreground requests.
const BACKGROUND_HEADROOM: usize = 3;

/// Send read-only warm-up requests a board menu asked for. They carry no
/// foreground feedback and never block input; a full queue drops them.
fn send_background(model: &mut Model, requests: &tokio::sync::mpsc::Sender<crate::UiRequest>) {
    let now = tokio::time::Instant::now();
    model.plan_palette_warm(now);
    model.fire_palette_warm(now);
    for action in std::mem::take(&mut model.background) {
        // Background reads keep headroom in the bounded queue for the keys
        // the user is pressing; a skipped prefetch is retried later.
        let sent = requests.capacity() > BACKGROUND_HEADROOM
            && requests
                .try_send(crate::UiRequest {
                    id: 0,
                    cancellation: None,
                    generation: model.ui_generation,
                    action: action.clone(),
                })
                .is_ok();
        if !sent {
            model.background_not_sent(&action, now);
        }
    }
}

#[cfg(test)]
mod hyperlink_tests {
    use super::*;

    #[test]
    fn links_are_written_cell_by_cell_and_restore_the_cursor() {
        let mut buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 10, 2));
        buffer.set_string(2, 1, "guide", ratatui::style::Style::new());
        let mut backend = Vec::new();
        write_hyperlinks(
            &mut backend,
            &buffer,
            &[crate::render::Hyperlink {
                area: ratatui::layout::Rect::new(2, 1, 5, 1),
                url: "https://guides.example/g".into(),
            }],
        )
        .unwrap();
        let out = String::from_utf8(backend).unwrap();
        assert!(out.starts_with("\x1b7"));
        assert!(out.ends_with("\x1b8"));
        let linked = out
            .split("\x1b]8;;https://guides.example/g\x1b\\")
            .skip(1)
            .map(|part| part.chars().next().unwrap())
            .collect::<String>();
        assert_eq!(linked, "guide");
    }
}
