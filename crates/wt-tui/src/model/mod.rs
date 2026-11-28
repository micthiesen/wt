//! The input owner's presentation state: the prepared board it shows, the
//! cursor, open prompts, and the user's pending requests.

mod board;
mod input;
mod interaction;
mod local_menu;
mod palette;
mod pending;
mod reply;
mod selection;

#[cfg(test)]
mod interaction_tests;
#[cfg(test)]
mod overlay_tests;
#[cfg(test)]
mod palette_tests;
#[cfg(test)]
mod reorder_tests;
#[cfg(test)]
mod request_tests;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use wt_runtime::SourceState;

use crate::UiAction;

pub(crate) use board::{ARCHIVED_SECTION, INBOX_SECTION, REMOVING_SECTION};
pub use board::{
    ActivityLine, AttentionLine, AutomationQueueView, AutomationRecordView, AutomationRunView,
    AutomationsView, Board, BoardRow, BoardSection, BusyView, CheckState, ContributorView,
    DiffStat, DisplayPolicy, GitPresentation, GuideView, HostChoice, LANDED_SECTION, LANDED_TITLE,
    LandingKind, LiveSlotView, LocalStamp, LogView, MergeQueueView, OWED_BLOCK_PREFIX,
    OWED_SECTIONS_OPTION, OtherPrView, OwedBlock, OwedView, PerfGroupView, PerfProcessView,
    PerfTone, PerfView, PrCommentView, PrPresentation, PreparedDetailGroup, RemovalView,
    RemovedHistoryRow, RemovedHistorySnapshot, ReviewBotView, ReviewRequestRow, ReviewState,
    ReviewerPool, SectionRollup, SessionMenuEntry, SessionView, SuggestedReviewerView,
    TerminalSlotView, TitleSource, UsageItem, WatchActionKind, WatchConditionView, WatchTruth,
    WatchView, WorkPresentation, WorkRiskCount, WorkStateCount, owed_block_refusal,
};
pub(crate) use input::REMOVAL_IN_PROGRESS;
pub use interaction::{
    ConfirmPrompt, Interaction, LoadingPrompt, PickerPrompt, ReviewerPrompt, TextPrompt,
    TimelinePrompt, TitlePrompt,
};
pub(crate) use interaction::{digit_rows, overlay_scroll};
pub(crate) use selection::{ListAnchor, VisualItem};

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
    pub action: UiAction,
    /// Dropping the request cancels a read or a session entry on its host.
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
    /// A `follow_up` that found the request queue full, with the input
    /// generation it was queued at. It is sent when room appears only if no
    /// key has acted since: after one, nobody is waiting for it any more.
    pub(crate) follow_up_kept: Option<(u64, UiAction)>,
    /// Until when keys are ignored, and why. A key that normally opens a
    /// menu or prompt answered with a notice instead, or a confirmation
    /// just closed: the person may still be typing for the layer they
    /// believed was on top, and those keys must not act on the board.
    pub(crate) keys_settle: Option<(tokio::time::Instant, &'static str)>,
    /// The key being dispatched asked for a menu and got a notice.
    pub(crate) menu_refused: bool,
    /// The request of a loading overlay that something else replaced. It
    /// was cancelled, but its reply may already be on its way and must not
    /// open the content nobody is waiting for.
    pub(crate) abandoned_overlay: Option<u64>,
    /// Sections chosen for rows still being created, by creation request.
    pub(crate) creation_moves: std::collections::BTreeMap<u64, pending::CreationMove>,
    /// The request whose board-built menu is open.
    pub(crate) local_menu: Option<UiAction>,
    /// The open board menu's default choice, while the user has not moved.
    pub(crate) local_menu_default: Option<local_menu::OptionIdentity>,
    /// A rebuild removed the option under the picker's cursor, which now
    /// rests on one the user has not chosen. The next key may move or cancel
    /// but not commit.
    pub(crate) picker_choice_lost: bool,
    /// A board menu whose commit was just produced, until it is sent.
    pub(crate) committed_menu: Option<UiAction>,
    /// The board menu behind the request in flight, reopened if it is stale.
    pub(crate) last_menu: Option<(u64, UiAction)>,
    /// Read-only requests sent without foreground feedback.
    pub(crate) background: Vec<UiAction>,
    /// A palette entry waiting for GitHub to confirm its row's branch.
    pub(crate) palette_wait: Option<palette::PaletteWait>,
    /// The sources should refresh because a commit found changed state.
    pub(crate) refresh_requested: bool,
    pub board: Arc<Board>,
    pub source_state: SourceState,
    pub selected: Option<usize>,
    pub offset: usize,
    pub details_scroll: u16,
    pub(crate) output: crate::output::OutputState,
    pub(crate) ui_generation: u64,
    pub(crate) pending_request: Option<PendingRequest>,
    /// Requests begun whose final reply has not arrived, oldest first. Only
    /// their replies may speak over the request in flight; a background
    /// read's never does.
    pub(crate) begun: std::collections::VecDeque<u64>,
    /// The failure of an earlier request, said while another was in flight
    /// and kept beside that request's feedback until the user acts again.
    pub(crate) earlier_failure: Option<String>,
    pub(crate) pr_chord: Option<(std::time::Instant, String, bool)>,
    pub help: bool,
    pub(crate) help_scroll: usize,
    /// Furthest useful help scroll for the last drawn frame, published by
    /// the renderer because wrapping and the glyph legend set the length.
    pub(crate) help_max_scroll: usize,
    pub(crate) help_query: crate::LineEditor,
    pub(crate) help_searching: bool,
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
    /// When the open confirmation starts taking the keys that commit it.
    /// Set for one a reply opened without the user asking for it.
    pub(crate) confirm_settles_at: Option<tokio::time::Instant>,
    /// Each removal review keeps its own checkout witness and host.
    pub(crate) removal_reviews: std::collections::VecDeque<(ConfirmPrompt, Option<String>)>,
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
    /// Written by each list draw and dropped by any key or wheel event, so
    /// it is only ever used to redraw a board that changed on its own.
    pub(crate) list_anchor: Option<ListAnchor>,
    /// A refreshed board arrived since the last draw: hold the anchor's line.
    pub(crate) list_reanchor: bool,
    /// The cursor just opened a section: bring its rows into view.
    pub(crate) list_reveal: bool,
    /// Lines the list pane had in the last drawn frame; 0 before the first.
    pub(crate) list_height: usize,
    /// The furthest the last drawn list could scroll, unbounded before the
    /// first draw. Wheel events that share a frame clamp to it, so a
    /// reversal at the end still counts.
    pub(crate) list_max_offset: usize,
    /// Mouse drag selection, in screen cells, while a button is held.
    pub(crate) mouse_selection: Option<crate::mouse::Selection>,
    /// Screen cells the last frame showed as links. Written by the driver
    /// after each draw, because a link escape inside a buffer cell would
    /// count as display width.
    pub(crate) hyperlinks: Vec<crate::render::Hyperlink>,
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
            follow_up_kept: None,
            abandoned_overlay: None,
            keys_settle: None,
            menu_refused: false,
            creation_moves: Default::default(),
            local_menu: None,
            local_menu_default: None,
            picker_choice_lost: false,
            committed_menu: None,
            last_menu: None,
            background: Vec::new(),
            palette_wait: None,
            refresh_requested: false,
            source_state: SourceState::Empty,
            selected: None,
            offset: 0,
            details_scroll: 0,
            output: Default::default(),
            ui_generation: 0,
            pending_request: None,
            begun: Default::default(),
            earlier_failure: None,
            pr_chord: None,
            help: false,
            help_scroll: 0,
            help_max_scroll: 0,
            help_query: crate::LineEditor::default(),
            help_searching: false,
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
            confirm_settles_at: None,
            removal_reviews: Default::default(),
            last_create_host: None,
            yank: None,
            toast: None,
            pending_selection: None,
            last_section_target: None,
            items: Vec::new(),
            log_close_key: None,
            log_refresh: None,
            list_free_scroll: false,
            list_anchor: None,
            list_reanchor: false,
            list_reveal: false,
            list_height: 0,
            list_max_offset: usize::MAX,
            mouse_selection: None,
            hyperlinks: Vec::new(),
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
