//! Event-driven terminal presentation. All repository and service work lives
//! outside this crate; the input owner receives only prepared snapshots.

mod actions;
mod badges;
mod cleanup;
mod editor;
mod glyphs;
mod help;
mod history;
mod menus;
mod model;
mod mouse;
mod mutations;
mod output;
mod refresh_wave;
mod render;
mod reviews;
mod terminal;
mod terminal_probe;
mod theme;

pub use actions::{
    ActionController, ActionSurface, CleanupTarget, ConfirmAction, CreatePlan, CreationPrepared,
    LiveSessionIdentity, PickerAction, PickerOption, PrPin, RemovalRevision, ReplyOutcome,
    ReviewerOption, Seen, SessionMode, SessionSelection, SessionTarget, TerminalHandoff,
    TextAction, UiAction, UiActions, UiModal, UiReply, UiRequest, UrlKind, action_channel,
};
pub use badges::harness_glyph;
pub use editor::LineEditor;
pub use menus::{LocalMenu, Menu, session_menu_key};
pub use model::{
    ActivityLine, AttentionLine, Board, BoardRow, BoardSection, BusyView, CheckState, DiffStat,
    DisplayPolicy, GitPresentation, HostChoice, Interaction, LandingKind, LiveSlotView, LogView,
    MergeQueueView, Model, PerfGroupView, PerfProcessView, PerfTone, PerfView, PrCommentView,
    PrPresentation, PreparedDetailGroup, RemovedHistoryRow, RemovedHistorySnapshot, ReviewBotView,
    ReviewRequestRow, ReviewState, SectionRollup, SessionMenuEntry, SessionView, TerminalSlotView,
    TitleSource, UsageItem, WorkPresentation, WorkRiskCount, WorkStateCount,
};
pub use mutations::{
    AUTO_MERGE_ACTION_ID, Effect, GithubChange, LayoutChange, LifecycleChange, MutationKind,
    RowChange, SETTLING_DOMAINS, UiMutation,
};
pub use terminal::{run, run_with_palette_probe};
#[doc(hidden)]
pub use terminal_probe::{decode_terminal_probe_bytes, decode_terminal_probe_chunks};
