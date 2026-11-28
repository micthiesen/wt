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

/// Room for a whole frame's escape sequences; a full repaint of a large
/// terminal is a few tens of kilobytes.
const FRAME_BUFFER_BYTES: usize = 64 * 1024;

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

/// What the loop does after one input event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Batch {
    /// Take the next event that has already arrived, if any.
    More,
    /// Draw before any later input is applied.
    Frame,
    Quit,
}

/// Apply `first` and then every input event that has already arrived, so
/// the frame that follows shows where the last of them left the model. A
/// frame per event would let input queue behind drawing whenever a frame
/// costs more than the gap between events (a held key, a wheel flick, a
/// loaded machine), and the cursor would trail the keyboard. Never waits
/// for an event. Returns how the batch ended and how many events it applied.
async fn apply_waiting_input<S>(
    first: Event,
    replay: &mut VecDeque<Event>,
    events: &mut S,
    mut apply: impl FnMut(Event) -> io::Result<Batch>,
) -> io::Result<(Batch, usize)>
where
    S: futures_util::Stream<Item = io::Result<Event>> + Unpin,
{
    let mut event = first;
    let mut applied = 0;
    loop {
        applied += 1;
        let batch = apply(event)?;
        if batch != Batch::More {
            return Ok((batch, applied));
        }
        let next = match replay.pop_front() {
            Some(event) => Some(Ok(event)),
            // Polled with this task's own waker, never a throwaway one:
            // crossterm keeps the waker of the first poll that found
            // nothing and ignores later ones until an event arrives.
            None => tokio::select! {
                biased;
                next = events.next() => next,
                () = std::future::ready(()) => None,
            },
        };
        match next {
            Some(next) => event = next?,
            None => return Ok((Batch::More, applied)),
        }
    }
}

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
    // One write per frame: stdout's own buffer would hand the terminal a
    // frame in 1 KiB pieces, each a chance to block on a busy terminal.
    let mut terminal = Terminal::new(CrosstermBackend::new(io::BufWriter::with_capacity(
        FRAME_BUFFER_BYTES,
        io::stdout(),
    )))?;
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
    let mut coalesced = 0;
    let mut metric_at = Instant::now();
    let mut controller_open = true;
    let mut toast_until = None;
    let mut log_refresh = LogRefresh::default();
    let outbox = Outbox {
        requests: &actions.requests,
        background: &actions.background,
        views: &actions.views,
    };
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
        log_refresh.follow(&model, tokio::time::Instant::now());
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
                if outbox
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
                        "actions are busy; selection not copied".into(),
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
                    log_latencies(&mut latencies, &mut coalesced);
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
                let mut apply = |event: Event| -> io::Result<Batch> {
                    match event {
                        Event::Key(key) if key.kind != KeyEventKind::Release => {
                            let received = Instant::now();
                            let toast_before = model.toast.clone();
                            let result = model.input(key, terminal.size()?.height.saturating_sub(4) as usize);
                            send_background(&mut model, outbox);
                            // A palette entry the board cannot offer yet asks
                            // the sources to read again.
                            if std::mem::take(&mut model.refresh_requested) {
                                source.refresh();
                            }
                            if model.toast.is_some() && model.toast != toast_before {
                                // Input-side feedback ("nothing needs you") expires
                                // like any other toast.
                                toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                            }
                            match result {
                                InputResult::Quit => return Ok(Batch::Quit),
                                InputResult::Refresh => {
                                    source.refresh();
                                    model.toast = Some(("refreshing…".into(), ToastTone::Info));
                                    toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                                    dirty = true;
                                    input_at.get_or_insert(received);
                                    return Ok(Batch::Frame);
                                }
                                InputResult::Draw => { dirty = true; input_at.get_or_insert(received); }
                                InputResult::Unchanged => {}
                                // A view opening or closing is state the
                                // controller follows, not a request to wait on.
                                InputResult::Action(action) if publish_view(outbox, &action) => {
                                    model.request_not_sent();
                                    dirty = true;
                                    input_at.get_or_insert(received);
                                    return Ok(Batch::Frame);
                                }
                                InputResult::Action(action) => {
                                    let id = model.next_op_id();
                                    let action = model.resolve(action);
                                    let feedback = action.clone();
                                    let cancellation = action.is_cancellable().then(CancellationToken::new);
                                    match outbox.requests.try_send(crate::UiRequest { id, generation: model.ui_generation, action, cancellation: cancellation.clone() }) {
                                        Ok(()) => {
                                            model.begin_action(id, &feedback);
                                            if let Some(request) = &mut model.pending_request {
                                                request._cancel_on_drop = cancellation.map(CancellationToken::drop_guard);
                                            }
                                            if let Some(text) = model.sent_feedback() { model.toast = Some((text.into(), ToastTone::Info)); }
                                        },
                                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => { model.request_not_sent(); model.toast = Some(("actions are busy; try again when one finishes".into(), ToastTone::Failed)) },
                                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => { model.request_not_sent(); model.toast = Some(("action worker stopped; restart wt".into(), ToastTone::Failed)) },
                                    }
                                    dirty = true;
                                    toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(4));
                                    input_at.get_or_insert(received);
                                    return Ok(Batch::Frame);
                                }
                            }
                        }
                        Event::Resize(_, _) => dirty = true,
                        Event::Mouse(mouse) => {
                            let area = terminal.size()?.into();
                            dirty |= crate::mouse::select(&mut model, mouse, area);
                            dirty |= crate::mouse::scroll(&mut model, mouse, area);
                            // A click can open a menu that queued a background read.
                            send_background(&mut model, outbox);
                            // A released drag copies what the next frame
                            // draws under it, so nothing later may change it.
                            if model.mouse_selection.is_some_and(|selection| selection.finished) {
                                return Ok(Batch::Frame);
                            }
                        }
                        Event::Paste(text) => dirty |= model.paste(&text),
                        _ => {}
                    }
                    Ok(Batch::More)
                };
                let (batch, applied) =
                    apply_waiting_input(event?, &mut pending_events, &mut events, &mut apply).await?;
                coalesced += applied - 1;
                if batch == Batch::Quit {
                    break;
                }
            }
            reply = actions.replies.recv(), if controller_open => {
                if let Some(reply) = reply {
                    let handoff = apply_reply(&mut model, reply, outbox, &source, &mut log_refresh);
                    toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(4));
                    dirty = true;
                    if let Some(ticket) = handoff {
                        drop(events);
                        let suspended = suspend_for_handoff(
                            ticket,
                            &mut guard,
                            &cancel,
                            &mut actions.replies,
                            |reply| {
                                // No key can ask for a session while one holds
                                // the terminal. Dropping a ticket releases
                                // its controller.
                                drop(apply_reply(&mut model, reply, outbox, &source, &mut log_refresh));
                            },
                        )
                        .await?;
                        model.restart_confirm_settle();
                        // The board kept changing while the session held the
                        // terminal. The first frame back shows it as it is
                        // now, not as it was when the session began.
                        if snapshots.has_changed().unwrap_or(false) {
                            model.apply(snapshots.borrow_and_update().clone());
                            send_background(&mut model, outbox);
                            send_follow_up(&mut model, outbox);
                        }
                        match suspended {
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
                    // Nothing will fill an overlay that is still loading.
                    model.close_loading();
                    model.pending_request = None;
                    model.toast = Some(("action worker stopped; restart wt".into(), ToastTone::Failed));
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
                match log_refresh.due {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => log_refresh.send(&mut model, outbox),
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
                send_background(&mut model, outbox);
                if model.toast != before {
                    toast_until = Some(tokio::time::Instant::now() + Duration::from_secs(4));
                }
                dirty = true;
            }
            changed = snapshots.changed() => {
                if changed.is_err() { break; }
                let before = model.toast.clone();
                model.apply(snapshots.borrow_and_update().clone());
                send_background(&mut model, outbox);
                send_follow_up(&mut model, outbox);
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
    log_latencies(&mut latencies, &mut coalesced);
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

/// The terminal's sending side. The loop polls the reply receiver apart
/// from it.
#[derive(Clone, Copy)]
struct Outbox<'a> {
    requests: &'a tokio::sync::mpsc::Sender<crate::UiRequest>,
    background: &'a tokio::sync::mpsc::Sender<crate::UiRequest>,
    views: &'a tokio::sync::watch::Sender<crate::ViewState>,
}

/// The refresh of an open Log overlay: at most one request in flight, the
/// next timed from that one's reply, so a slow host is asked no faster than
/// it answers and nothing queues up behind a hung one.
#[derive(Default)]
struct LogRefresh {
    /// When the next request is due. Unset while one is in flight.
    due: Option<tokio::time::Instant>,
    /// The request in flight. Dropping its guard cancels it.
    in_flight: Option<(u64, tokio_util::sync::DropGuard)>,
}

impl LogRefresh {
    /// Follow the overlay: schedule while it is open, and when it closes
    /// stop and cancel the request in flight.
    fn follow(&mut self, model: &Model, now: tokio::time::Instant) {
        if model.log_refresh.is_none() {
            *self = Self::default();
        } else if self.due.is_none() && self.in_flight.is_none() {
            self.due = Some(now + Duration::from_secs(1));
        }
    }

    /// Send the due request. A full queue skips this tick.
    fn send(&mut self, model: &mut Model, outbox: Outbox<'_>) {
        self.due = None;
        if self.in_flight.is_some() {
            return;
        }
        let Some(action) = model.log_refresh.clone() else {
            return;
        };
        let id = model.next_op_id();
        let cancellation = CancellationToken::new();
        let request = crate::UiRequest {
            id,
            cancellation: Some(cancellation.clone()),
            generation: model.ui_generation,
            action,
        };
        if outbox.background.try_send(request).is_ok() {
            self.in_flight = Some((id, cancellation.drop_guard()));
        }
    }

    /// Note a reply. The request's own answer lets the next be scheduled;
    /// its progress, or another request's reply, does not.
    fn answered(&mut self, reply: &crate::UiReply) {
        if reply.progress.is_none()
            && self
                .in_flight
                .as_ref()
                .is_some_and(|(id, _)| Some(*id) == reply.operation_id)
            && let Some((_, cancel)) = self.in_flight.take()
        {
            // Nothing is left to cancel.
            drop(cancel.disarm());
        }
    }
}

/// Publish a view change. Returns false for an action that is a request.
fn publish_view(outbox: Outbox<'_>, action: &crate::UiAction) -> bool {
    let mut view = false;
    outbox.views.send_if_modified(|state| {
        let before = *state;
        view = state.apply(action);
        *state != before
    });
    view
}

/// Apply a host reply and send what it queued. Returns the session the
/// terminal should be handed to, if the reply carries one.
fn apply_reply(
    model: &mut Model,
    reply: crate::UiReply,
    outbox: Outbox<'_>,
    source: &SourceHandle<Board>,
    log_refresh: &mut LogRefresh,
) -> Option<TerminalHandoff> {
    log_refresh.answered(&reply);
    let history_was_open = model.history.active;
    let perf_was_open = model.show_perf;
    let handoff = model.reply(reply);
    send_background(model, outbox);
    if std::mem::take(&mut model.refresh_requested) {
        source.refresh();
    }
    send_follow_up(model, outbox);
    if history_was_open && !model.history.active {
        publish_view(outbox, &crate::UiAction::SetHistoryActive { active: false });
    }
    if perf_was_open && !model.show_perf {
        publish_view(
            outbox,
            &crate::UiAction::SetPerf {
                active: false,
                continuous: model.perf_continuous,
                refresh: false,
            },
        );
    }
    handoff
}

async fn suspend_for_handoff(
    ticket: TerminalHandoff,
    guard: &mut TerminalGuard,
    cancel: &CancellationToken,
    replies: &mut tokio::sync::mpsc::Receiver<crate::UiReply>,
    on_reply: impl FnMut(crate::UiReply),
) -> io::Result<HandoffResult> {
    guard.suspend()?;
    if ticket.ready.send(()).is_err() {
        guard.resume()?;
        return Ok(HandoffResult::Resumed(Err(
            "session handoff could not start".into(),
        )));
    }
    let result = wait_for_handoff(ticket.resumed, cancel, replies, on_reply).await;
    // Even cancellation restores the screen before the UI loop exits. Dropping
    // the receiver above tells the controller to terminate and reap its child.
    guard.resume()?;
    Ok(result)
}

/// Wait for the session to give the terminal back. Host replies are still
/// applied, undrawn: a creation does not start its checkout until the model
/// acknowledges it, and the user may stay in the session for hours.
async fn wait_for_handoff(
    resumed: tokio::sync::oneshot::Receiver<Result<(), String>>,
    cancel: &CancellationToken,
    replies: &mut tokio::sync::mpsc::Receiver<crate::UiReply>,
    mut on_reply: impl FnMut(crate::UiReply),
) -> HandoffResult {
    tokio::pin!(resumed);
    let mut controller_open = true;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return HandoffResult::Cancelled,
            result = &mut resumed => return HandoffResult::Resumed(
                result.unwrap_or_else(|_| Err("session handoff ended without a result".into()))
            ),
            reply = replies.recv(), if controller_open => match reply {
                Some(reply) => on_reply(reply),
                // The UI loop reports a stopped controller once it resumes.
                None => controller_open = false,
            },
        }
    }
}

/// `coalesced` counts input events that shared a frame with an earlier one.
fn log_latencies(samples: &mut Vec<f64>, coalesced: &mut usize) {
    if samples.is_empty() {
        return;
    }
    samples.sort_by(f64::total_cmp);
    tracing::info!(
        n = samples.len(),
        p50_ms = samples[samples.len() / 2],
        p90_ms = samples[(samples.len() * 9 / 10).min(samples.len() - 1)],
        max_ms = samples[samples.len() - 1],
        coalesced = *coalesced,
        "input-latency"
    );
    samples.clear();
    *coalesced = 0;
}

/// Send the action a reply or a new snapshot queued behind it.
fn send_follow_up(model: &mut Model, outbox: Outbox<'_>) {
    // A follow-up that waited for room was meant for the view of the reply
    // that queued it. A key has acted since: sending it now would act under
    // the person's hands, so it is dropped, and said.
    if let Some((kept_at, kept)) = model.follow_up_kept.take()
        && kept_at != model.ui_generation
        && model.follow_up.as_ref() == Some(&kept)
    {
        model.follow_up = None;
        model.toast = Some((
            "a queued action was not sent: actions were busy".into(),
            ToastTone::Notice,
        ));
    }
    // A move chosen for a row still being created is released only into a
    // free slot, so it waits where a key cannot orphan it.
    if model.follow_up.is_none() && outbox.requests.capacity() > 0 {
        model.release_creation_moves();
    }
    let Some(action) = model.follow_up.take() else {
        return;
    };
    if publish_view(outbox, &action) {
        return;
    }
    // Nobody is at hand to press a key again for a request the model
    // queued itself. A full queue keeps it for the next reply or snapshot,
    // each of which is when room appears.
    let Ok(permit) = outbox.requests.try_reserve() else {
        model.follow_up_kept = Some((model.ui_generation, action.clone()));
        model.follow_up = Some(action);
        return;
    };
    let id = model.next_op_id();
    let action = model.resolve(action);
    let feedback = action.clone();
    let cancellation = action.is_cancellable().then(CancellationToken::new);
    permit.send(crate::UiRequest {
        id,
        generation: model.ui_generation,
        action,
        cancellation: cancellation.clone(),
    });
    model.begin_action(id, &feedback);
    if let Some(request) = &mut model.pending_request {
        request._cancel_on_drop = cancellation.map(CancellationToken::drop_guard);
    }
    if let Some(text) = model.sent_feedback() {
        model.toast = Some((text.into(), ToastTone::Info));
    }
}

/// Send read-only warm-up requests a board menu asked for. They carry no
/// foreground feedback and never block input. They travel apart from the
/// user's requests, so neither can fill the other's queue. One that finds
/// the queue full is dropped; the menu asks again the next time it opens.
fn send_background(model: &mut Model, outbox: Outbox<'_>) {
    for action in std::mem::take(&mut model.background) {
        let _ = outbox.background.try_send(crate::UiRequest {
            id: 0,
            cancellation: None,
            generation: model.ui_generation,
            action,
        });
    }
}

#[cfg(test)]
mod input_batch_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use futures_util::{FutureExt, stream};

    use super::*;

    fn key(code: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE))
    }

    /// Events that have arrived, followed by a terminal with nothing more
    /// to say.
    fn arrived(codes: &str) -> impl futures_util::Stream<Item = io::Result<Event>> + Unpin + use<> {
        let waiting: Vec<_> = codes.chars().map(|code| Ok(key(code))).collect();
        stream::iter(waiting).chain(stream::pending())
    }

    #[tokio::test]
    async fn held_keys_that_have_arrived_all_move_the_cursor_before_one_frame() {
        let mut model = crate::render::frame_cost::busy_model(60);
        let before = model.selected.unwrap();
        let mut events = arrived(&"j".repeat(24));
        let (batch, applied) =
            apply_waiting_input(key('j'), &mut VecDeque::new(), &mut events, |event| {
                let Event::Key(key) = event else {
                    unreachable!()
                };
                assert!(matches!(model.input(key, 40), InputResult::Draw));
                Ok(Batch::More)
            })
            .await
            .unwrap();
        assert_eq!((batch, applied), (Batch::More, 25));
        assert_eq!(model.selected, Some(before + 25));
        // The one frame the loop now draws shows the row the last key chose.
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(160, 40)).unwrap();
        terminal.draw(|frame| render(frame, &mut model)).unwrap();
        let slug = model.selected_row().unwrap().slug.clone();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains(&slug), "details pane names {slug}");
    }

    #[tokio::test]
    async fn a_batch_never_waits_and_stops_where_a_frame_is_owed() {
        let mut seen = String::new();
        let mut note = |event: Event, stop: char| {
            let Event::Key(KeyEvent {
                code: KeyCode::Char(code),
                ..
            }) = event
            else {
                unreachable!()
            };
            seen.push(code);
            Ok(if code == stop {
                Batch::Frame
            } else {
                Batch::More
            })
        };
        // Replayed probe input goes first, then what the terminal holds.
        let mut replay = VecDeque::from([key('b')]);
        let mut events = arrived("cXde");
        let (batch, applied) =
            apply_waiting_input(key('a'), &mut replay, &mut events, |event| note(event, 'X'))
                .await
                .unwrap();
        assert_eq!((batch, applied), (Batch::Frame, 4));
        // What followed the stop is still the terminal's, in order.
        let (batch, applied) = apply_waiting_input(
            events.next().now_or_never().unwrap().unwrap().unwrap(),
            &mut replay,
            &mut events,
            |event| note(event, 'q'),
        )
        .await
        .unwrap();
        assert_eq!((batch, applied), (Batch::More, 2));
        assert_eq!(seen, "abcXde");
        assert!(events.next().now_or_never().is_none());
    }

    #[test]
    fn output_scrolls_that_share_a_frame_each_count() {
        let mut model = crate::render::frame_cost::busy_model(8);
        let view = |model: &mut Model| model.output_view(10, 80).1;
        let newest = view(&mut model);
        for _ in 0..3 {
            model.output.scroll(true);
        }
        let batched = view(&mut model);
        model.output.mark_seen();
        view(&mut model);
        for _ in 0..3 {
            model.output.scroll(true);
            view(&mut model);
        }
        assert_eq!(batched, view(&mut model));
        assert_ne!(batched, newest);
        // Back down past the end follows the newest lines again.
        for _ in 0..4 {
            model.output.scroll(false);
        }
        assert_eq!(view(&mut model), newest);
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;

    #[tokio::test]
    async fn a_creation_is_acknowledged_while_a_session_holds_the_terminal() {
        let (host, mut replies) = tokio::sync::mpsc::channel(16);
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let mut model = Model::default();
        model.begin_action(
            7,
            &crate::UiAction::Create {
                placement: None,
                input: "new".into(),
            },
        );
        // The host resolved the name after the user entered another session
        // and now waits for the model before it creates the checkout.
        let (ready, acknowledged) = tokio::sync::oneshot::channel();
        host.send(crate::UiReply {
            operation_id: Some(7),
            creation_prepared: Some(crate::CreationPrepared {
                key: "new".into(),
                already_exists: false,
                ready,
            }),
            ..Default::default()
        })
        .await
        .unwrap();
        let session = async {
            tokio::time::timeout(Duration::from_secs(2), acknowledged)
                .await
                .expect("the creation waited for the session to end")
                .unwrap();
            resume.send(Ok(())).unwrap();
        };
        let (result, ()) = tokio::join!(
            wait_for_handoff(resumed, &cancel, &mut replies, |reply| {
                drop(model.reply(reply));
            }),
            session
        );
        assert!(matches!(result, HandoffResult::Resumed(Ok(()))));
    }
}

#[cfg(test)]
mod request_channel_tests {
    use super::*;
    use crate::UiAction;

    #[tokio::test]
    async fn view_changes_are_published_as_state_and_never_queued() {
        let (actions, mut controller) = crate::action_channel();
        let outbox = Outbox {
            requests: &actions.requests,
            background: &actions.background,
            views: &actions.views,
        };
        assert!(publish_view(
            outbox,
            &UiAction::SetHistoryActive { active: true }
        ));
        let perf = |refresh| UiAction::SetPerf {
            active: true,
            continuous: true,
            refresh,
        };
        assert!(publish_view(outbox, &perf(false)));
        assert!(publish_view(outbox, &perf(true)));
        assert!(controller.views.has_changed().unwrap());
        assert_eq!(
            *controller.views.borrow_and_update(),
            crate::ViewState {
                history: true,
                perf: true,
                perf_continuous: true,
                perf_revision: 1,
            }
        );
        // Repeating the state wakes nobody; a request is not a view.
        assert!(publish_view(outbox, &perf(false)));
        assert!(!controller.views.has_changed().unwrap());
        assert!(!publish_view(outbox, &UiAction::HardRefresh));
        assert!(controller.requests.try_recv().is_err());
        assert!(controller.background.try_recv().is_err());
    }

    /// A request the model queued behind a reply has nobody to press its
    /// key again. One that finds the queue full waits for room.
    #[tokio::test]
    async fn a_follow_up_that_finds_the_queue_full_is_kept_until_there_is_room() {
        let (actions, mut controller) = crate::action_channel();
        let outbox = Outbox {
            requests: &actions.requests,
            background: &actions.background,
            views: &actions.views,
        };
        while actions
            .requests
            .try_send(crate::UiRequest {
                id: 0,
                generation: 0,
                action: UiAction::HardRefresh,
                cancellation: None,
            })
            .is_ok()
        {}
        let mut model = Model::default();
        let queued = UiAction::FoldSection {
            key: "Batch".into(),
            folded: true,
        };
        model.follow_up = Some(queued.clone());
        send_follow_up(&mut model, outbox);
        assert_eq!(
            model.follow_up,
            Some(queued.clone()),
            "the request was dropped"
        );
        assert!(model.pending_request.is_none());
        assert!(model.toast.is_none());
        // Still no room, and no key since: it keeps waiting.
        send_follow_up(&mut model, outbox);
        assert_eq!(model.follow_up, Some(queued.clone()));
        // The next reply or snapshot finds room and sends it.
        controller.requests.try_recv().unwrap();
        send_follow_up(&mut model, outbox);
        assert!(model.follow_up.is_none());
        assert!(model.pending_request.is_some());
        let mut sent = None;
        while let Ok(request) = controller.requests.try_recv() {
            sent = Some(request.action);
        }
        assert_eq!(sent, Some(queued));
    }

    /// A follow-up was queued for the view its reply found. Once a key has
    /// acted, room appearing must not send it under the person's hands.
    #[tokio::test]
    async fn a_kept_follow_up_is_dropped_once_a_key_has_acted() {
        let (actions, mut controller) = crate::action_channel();
        let outbox = Outbox {
            requests: &actions.requests,
            background: &actions.background,
            views: &actions.views,
        };
        while actions
            .requests
            .try_send(crate::UiRequest {
                id: 0,
                generation: 0,
                action: UiAction::HardRefresh,
                cancellation: None,
            })
            .is_ok()
        {}
        let mut model = Model {
            follow_up: Some(UiAction::CyclePrimary),
            ..Model::default()
        };
        send_follow_up(&mut model, outbox);
        assert!(model.follow_up.is_some());
        // A key that does something: here, opening help.
        model.input(
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('?')),
            20,
        );
        while controller.requests.try_recv().is_ok() {}
        send_follow_up(&mut model, outbox);
        assert!(model.follow_up.is_none());
        assert!(model.pending_request.is_none());
        assert!(
            controller.requests.try_recv().is_err(),
            "the stale follow-up was sent"
        );
        assert_eq!(model.toast.as_ref().unwrap().1, ToastTone::Notice);
        // The next follow-up is its own: it is not judged by the old mark.
        model.follow_up = Some(UiAction::CyclePrimary);
        send_follow_up(&mut model, outbox);
        assert_eq!(
            controller.requests.try_recv().map(|request| request.action),
            Ok(UiAction::CyclePrimary)
        );
    }

    #[tokio::test]
    async fn background_reads_and_user_requests_cannot_fill_each_others_queue() {
        let (actions, mut controller) = crate::action_channel();
        let outbox = Outbox {
            requests: &actions.requests,
            background: &actions.background,
            views: &actions.views,
        };
        let request = |action| crate::UiRequest {
            id: 0,
            generation: 0,
            action,
            cancellation: None,
        };
        // A backlog of prefetches leaves every slot for the user's keys.
        while actions
            .background
            .try_send(request(UiAction::DiscoverSessions {
                key: None,
                target: crate::SessionTarget::Main,
            }))
            .is_ok()
        {}
        let free = actions.requests.capacity();
        assert_eq!(free, actions.requests.max_capacity());
        // And a backlog of the user's requests does not drop a prefetch.
        while actions
            .requests
            .try_send(request(UiAction::HardRefresh))
            .is_ok()
        {}
        while controller.background.try_recv().is_ok() {}
        let mut model = Model::default();
        let warm = UiAction::DiscoverSessions {
            key: Some("row".into()),
            target: crate::SessionTarget::Harness,
        };
        model.background.push(warm.clone());
        send_background(&mut model, outbox);
        assert!(model.background.is_empty());
        assert_eq!(controller.background.try_recv().unwrap().action, warm);
    }

    #[tokio::test]
    async fn an_overlay_refresh_is_single_and_is_cancelled_with_its_overlay() {
        let (actions, mut controller) = crate::action_channel();
        let outbox = Outbox {
            requests: &actions.requests,
            background: &actions.background,
            views: &actions.views,
        };
        let source = SourceHandle::constant(wt_runtime::SourceSnapshot::default());
        let mut model = Model::default();
        let refresh_action = UiAction::DiscoverSessions {
            key: None,
            target: crate::SessionTarget::Main,
        };
        let open = |model: &mut Model| {
            model.interaction = crate::Interaction::Log {
                title: "dev".into(),
                lines: Vec::new(),
                scroll: 0,
            };
            model.log_refresh = Some(refresh_action.clone());
        };
        let reply = |id, progress: Option<&str>| crate::UiReply {
            operation_id: Some(id),
            progress: progress.map(str::to_owned),
            ..Default::default()
        };
        let now = tokio::time::Instant::now();
        let mut refresh = LogRefresh::default();
        // Closed: nothing is scheduled.
        refresh.follow(&model, now);
        assert!(refresh.due.is_none());
        open(&mut model);
        refresh.follow(&model, now);
        assert_eq!(refresh.due, Some(now + Duration::from_secs(1)));
        refresh.send(&mut model, outbox);
        let first = controller.background.try_recv().expect("one request");
        assert_eq!(first.action, refresh_action);
        let first_cancel = first.cancellation.clone().expect("cancellable");
        // While it is unanswered, a slow host is not asked again, however
        // often the loop turns or a stray timer fires.
        for _ in 0..3 {
            refresh.follow(&model, now + Duration::from_secs(5));
            assert!(refresh.due.is_none());
            refresh.send(&mut model, outbox);
        }
        assert!(controller.background.try_recv().is_err());
        // Another request's reply, and progress for this one, are not its answer.
        drop(apply_reply(
            &mut model,
            reply(first.id + 100, None),
            outbox,
            &source,
            &mut refresh,
        ));
        drop(apply_reply(
            &mut model,
            reply(first.id, Some("still loading")),
            outbox,
            &source,
            &mut refresh,
        ));
        assert!(refresh.in_flight.is_some());
        drop(apply_reply(
            &mut model,
            reply(first.id, None),
            outbox,
            &source,
            &mut refresh,
        ));
        assert!(refresh.in_flight.is_none());
        assert!(
            !first_cancel.is_cancelled(),
            "an answered request is not cancelled"
        );
        // Answered: the next one is timed from now, and sent.
        open(&mut model);
        refresh.follow(&model, now);
        assert!(refresh.due.is_some());
        refresh.send(&mut model, outbox);
        let second = controller.background.try_recv().expect("the next request");
        let second_cancel = second.cancellation.clone().expect("cancellable");
        // The overlay closes with that request in flight: it is cancelled
        // and nothing more is scheduled.
        model.log_refresh = None;
        refresh.follow(&model, now);
        assert!(second_cancel.is_cancelled());
        assert!(refresh.due.is_none() && refresh.in_flight.is_none());
        // A full queue skips a tick instead of pretending one is in flight.
        open(&mut model);
        while actions
            .background
            .try_send(crate::UiRequest {
                id: 0,
                generation: 0,
                action: refresh_action.clone(),
                cancellation: None,
            })
            .is_ok()
        {}
        refresh.follow(&model, now);
        refresh.send(&mut model, outbox);
        assert!(refresh.in_flight.is_none());
        refresh.follow(&model, now);
        assert!(refresh.due.is_some());
    }

    #[test]
    fn an_earlier_requests_refusal_is_said_while_a_later_request_is_pending() {
        let archive = UiAction::ToggleArchive {
            key: "a".into(),
            seen: None,
        };
        let fold = UiAction::FoldSection {
            key: "inbox".into(),
            folded: true,
        };
        let mut model = Model::default();
        model.begin_action(1, &archive);
        model.begin_action(2, &fold);
        drop(model.reply(crate::UiReply {
            operation_id: Some(1),
            ..crate::UiReply::stale("row is already archived")
        }));
        assert_eq!(
            model.toast,
            Some(("row is already archived".into(), ToastTone::Failed))
        );
        // Its success would not interrupt the request the user now waits on.
        let mut model = Model::default();
        model.begin_action(1, &archive);
        model.begin_action(2, &fold);
        model.toast = Some(("working…".into(), ToastTone::Info));
        drop(model.reply(crate::UiReply {
            operation_id: Some(1),
            ..crate::UiReply::message("Archived")
        }));
        assert_eq!(model.toast, Some(("working…".into(), ToastTone::Info)));
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
