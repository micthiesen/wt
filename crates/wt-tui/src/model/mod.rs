//! The input owner's presentation state: the prepared board it shows, the
//! cursor, open prompts, and the user's pending requests.

mod board;
mod input;
mod interaction;
mod pending;
mod reply;
mod selection;

#[cfg(test)]
mod interaction_tests;
#[cfg(test)]
mod request_tests;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use wt_runtime::SourceState;

use crate::UiAction;

pub(crate) use board::{ARCHIVED_SECTION, INBOX_SECTION};
pub use board::{
    ActivityLine, AttentionLine, Board, BoardRow, BoardSection, BusyView, CheckState, DiffStat,
    DisplayPolicy, GitPresentation, HostChoice, LandingKind, LiveSlotView, LogView, MergeQueueView,
    PerfGroupView, PerfProcessView, PerfTone, PerfView, PrCommentView, PrPresentation,
    PreparedDetailGroup, RemovedHistoryRow, RemovedHistorySnapshot, ReviewBotView,
    ReviewRequestRow, ReviewState, SectionRollup, SessionView, TitleSource, UsageItem,
    WorkPresentation, WorkRiskCount, WorkStateCount,
};
pub use interaction::{
    ConfirmPrompt, Interaction, PickerPrompt, ReviewerPrompt, TextPrompt, TitlePrompt,
};
pub(crate) use interaction::{digit_rows, overlay_scroll};
pub(crate) use selection::VisualItem;

/// How a footer toast reads: plain feedback, a notice that the key had
/// nothing to act on (yellow), or a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastTone {
    Info,
    Notice,
    Failed,
}

/// The request currently owning foreground feedback. This tracks transport
/// completion, not optimistic board data (which belongs exclusively to Pending).
pub(crate) struct PendingRequest {
    pub id: u64,
    pub generation: u64,
    pub preparation: bool,
    /// The key that requested this menu; pressing it again cancels it.
    pub opener: Option<crossterm::event::KeyEvent>,
    /// Keys typed while the menu loads, replayed into it when it opens. They
    /// never reach the board underneath.
    pub typeahead: Vec<(crossterm::event::KeyEvent, usize)>,
    pub started: tokio::time::Instant,
    pub action: UiAction,
    pub _cancel_on_drop: Option<tokio_util::sync::DropGuard>,
}
/// Everything the terminal shows and the keys act on: the presented board
/// (source board with pending changes applied), cursor, panes and prompts.
pub struct Model {
    /// Requested changes shown over `source_board` until a fetch confirms
    /// them. The only optimistic state in the TUI.
    pub(crate) pending: wt_runtime::Pending<Board, crate::UiMutation>,
    pub(crate) next_op: u64,
    /// Provenance and fetch-start stamps of the local host's domains.
    pub(crate) local_domains: wt_runtime::DomainMap,
    pub(crate) source_board: Arc<Board>,
    pub(crate) follow_up: Option<UiAction>,
    /// Typeahead left after a replayed key requested another menu.
    pub(crate) carry_typeahead: Vec<(crossterm::event::KeyEvent, usize)>,
    /// The key being dispatched, recorded as the opener of a requested menu.
    pub(crate) last_key: Option<crossterm::event::KeyEvent>,
    pub board: Arc<Board>,
    pub source_state: SourceState,
    pub selected: Option<usize>,
    pub offset: usize,
    pub details_scroll: u16,
    pub(crate) output: crate::output::OutputState,
    pub(crate) ui_generation: u64,
    pub(crate) pending_request: Option<PendingRequest>,
    pub(crate) pr_chord: Option<(std::time::Instant, String, bool)>,
    pub help: bool,
    pub(crate) help_scroll: usize,
    /// Furthest useful help scroll for the last drawn frame, published by
    /// the renderer because wrapping and the glyph legend set the length.
    pub(crate) help_max_scroll: usize,
    pub(crate) help_query: crate::LineEditor,
    pub(crate) help_searching: bool,
    pub(crate) show_verification: bool,
    pub show_perf: bool,
    pub(crate) perf_continuous: bool,
    pub(crate) perf_scroll: usize,
    /// Furthest useful perf scroll for the last drawn frame, published by
    /// the renderer because the prepared sample's layout sets the length.
    pub(crate) perf_max_scroll: usize,
    pub(crate) history: crate::history::HistoryView,
    pub(crate) reviews_folded: bool,
    pub frame_count: u64,
    pub last_frame_micros: u128,
    pub title_prompt: Option<TitlePrompt>,
    pub interaction: Interaction,
    pub(crate) interaction_host: Option<String>,
    /// Host chosen with Tab in the last new-worktree prompt; `None` is local.
    pub(crate) last_create_host: Option<String>,
    pub yank: Option<usize>,
    pub toast: Option<(String, ToastTone)>,
    pub pending_selection: Option<String>,
    pub(crate) last_section_target: Option<Option<String>>,
    pub(crate) items: Vec<VisualItem>,
    /// Extra close key and periodic refresh request of the open Log overlay.
    pub(crate) log_close_key: Option<char>,
    pub(crate) log_refresh: Option<UiAction>,
    /// The list viewport was scrolled independently of the cursor (mouse
    /// wheel, or `j`/`k` pressed at an edge). Any cursor move clears it.
    pub list_free_scroll: bool,
    /// Mouse drag selection, in screen cells, while a button is held.
    pub(crate) mouse_selection: Option<crate::mouse::Selection>,
    pub(crate) refresh_wave: crate::refresh_wave::RefreshWave,
}

/// What a key press asks the terminal loop to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputResult {
    Unchanged,
    Draw,
    Refresh,
    Quit,
    Action(UiAction),
}

impl Default for Model {
    fn default() -> Self {
        Self {
            board: Arc::new(Board::default()),
            source_board: Arc::new(Board::default()),
            pending: Default::default(),
            next_op: 0,
            local_domains: Default::default(),
            follow_up: None,
            carry_typeahead: Vec::new(),
            last_key: None,
            source_state: SourceState::Empty,
            selected: None,
            offset: 0,
            details_scroll: 0,
            output: Default::default(),
            ui_generation: 0,
            pending_request: None,
            pr_chord: None,
            help: false,
            help_scroll: 0,
            help_max_scroll: 0,
            help_query: crate::LineEditor::default(),
            help_searching: false,
            show_verification: false,
            show_perf: false,
            perf_continuous: false,
            perf_scroll: 0,
            perf_max_scroll: 0,
            history: Default::default(),
            reviews_folded: false,
            frame_count: 0,
            last_frame_micros: 0,
            title_prompt: None,
            interaction: Interaction::None,
            interaction_host: None,
            last_create_host: None,
            yank: None,
            toast: None,
            pending_selection: None,
            last_section_target: None,
            items: Vec::new(),
            log_close_key: None,
            log_refresh: None,
            list_free_scroll: false,
            mouse_selection: None,
            refresh_wave: Default::default(),
        }
    }
}

impl Model {
    /// The primary harness to display (a just-cycled one is projected by its
    /// pending change); an unset board means the default, Claude.
    pub(crate) fn primary_harness(&self) -> &str {
        match self.board.display.primary_harness.as_str() {
            "" => "Claude",
            primary => primary,
        }
    }
}
