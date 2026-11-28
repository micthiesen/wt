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
    ActionController, ActionSurface, CleanupTarget, ConfirmAction, CreatePlacement, CreatePlan,
    CreationPlacementAccepted, CreationPrepared, CreationTitle, FailedRemovalSeen,
    LiveSessionIdentity, NotifyChange, PickerAction, PickerOption, PrPin, RemovalRevision,
    RemovalSeen, ReplyOutcome, ReviewerOption, Seen, SessionMode, SessionSelection, SessionTarget,
    StatusClaim, TerminalHandoff, TextAction, UiAction, UiActions, UiModal, UiReply, UiRequest,
    UrlKind, ViewState, action_channel,
};
pub use badges::harness_glyph;
pub use editor::LineEditor;
pub use menus::palette::{
    ActionCatalog, ActionDelivery, ActionEntry, ActionFacts, ActionRequirement, ActionRowSeen,
    ActionSeen, ActionState, ActiveRunView, ArgValue, Blocker, CUSTOM_ACTION_ID,
    DEV_LOGS_ACTION_ID, OPEN_EDITOR_ACTION_ID, PrFact, blocker, slot_name,
};
pub use menus::sessions::{harness_label, session_value, terminal_label};
pub use menus::{LoadingOverlay, LocalMenu, Menu, session_menu_key};
pub use model::{
    ActivityLine, AttentionLine, AutomationQueueView, AutomationRecordView, AutomationRunView,
    AutomationsView, Board, BoardRow, BoardSection, BusyView, CheckState, ContributorView,
    DiffStat, DisplayPolicy, GitPresentation, GuideView, HostChoice, Interaction, LANDED_SECTION,
    LANDED_TITLE, LandingKind, LiveSlotView, LocalStamp, LogView, MergeQueueView, Model,
    OWED_BLOCK_PREFIX, OWED_SECTIONS_OPTION, OtherPrView, OwedBlock, OwedView, PerfGroupView,
    PerfProcessView, PerfTone, PerfView, PrCommentView, PrPresentation, PreparedDetailGroup,
    RemovalView, RemovedHistoryRow, RemovedHistorySnapshot, ReviewBotView, ReviewRequestRow,
    ReviewState, ReviewerPool, SectionRollup, SessionMenuEntry, SessionView, SuggestedReviewerView,
    TerminalSlotView, TitleSource, UsageItem, WatchActionKind, WatchConditionView, WatchTruth,
    WatchView, WorkPresentation, WorkRiskCount, WorkStateCount, owed_block_refusal,
};
pub use mutations::{
    AUTO_MERGE_ACTION_ID, Effect, GithubChange, LayoutChange, LifecycleChange, MutationKind,
    ReorderStep, RowChange, SETTLING_DOMAINS, STATUS_SORT_PINS, UiMutation, plan_reorder,
    reorder_refusal, unit_at_creation_position, unit_rank,
};
pub use terminal::{run, run_with_palette_probe};
#[doc(hidden)]
pub use terminal_probe::{decode_terminal_probe_bytes, decode_terminal_probe_chunks};
