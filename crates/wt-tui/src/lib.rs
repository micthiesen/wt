//! Event-driven terminal presentation. All repository and service work lives
//! outside this crate; the input owner receives only prepared snapshots.

mod actions;
mod editor;
mod help;
mod history;
mod model;
mod mouse;
mod output;
mod render;
mod reviews;
mod terminal;
mod terminal_probe;

pub use actions::{
    ActionController, ActionSurface, ConfirmAction, PickerAction, PickerOption, RemovalRevision,
    ReviewerOption, SessionMode, SessionSelection, SessionTarget, TerminalHandoff, TextAction,
    UiAction, UiActions, UiModal, UiReply, UiRequest, UrlKind, action_channel,
};
pub use editor::LineEditor;
pub use model::{
    ActivityLine, AttentionLine, Board, BoardRow, BoardSection, GitPresentation, HostChoice,
    Interaction, LandingKind, LogView, Model, PrPresentation, PreparedDetailGroup,
    RemovedHistoryRow, RemovedHistorySnapshot, ReviewRequestRow, SectionRollup, SessionView,
    WorkPresentation, WorkRiskCount, WorkStateCount,
};
pub use terminal::{run, run_with_palette_probe};
#[doc(hidden)]
pub use terminal_probe::{decode_terminal_probe_bytes, decode_terminal_probe_chunks};
