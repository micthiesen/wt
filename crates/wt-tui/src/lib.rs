//! Event-driven terminal presentation. All repository and service work lives
//! outside this crate; the input owner receives only prepared snapshots.

mod actions;
mod editor;
mod model;
mod render;
mod terminal;

pub use actions::{
    ActionController, ConfirmAction, PickerAction, PickerOption, RemovalRevision, SessionTarget,
    TerminalHandoff, TextAction, UiAction, UiActions, UiModal, UiReply, UrlKind, action_channel,
};
pub use editor::LineEditor;
pub use model::{Board, BoardRow, BoardSection, Interaction, Model};
pub use terminal::run;
