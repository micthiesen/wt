use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

/// Requests cross the input boundary as data. The controller owns execution,
/// persistence and refresh; the terminal never waits for an action to finish.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiAction {
    /// Captured by a host chooser or a host-owned modal. Selection changes
    /// cannot redirect its later confirmation to a different machine.
    OnHost {
        host: Option<String>,
        action: Box<UiAction>,
    },
    PrepareCreate {
        initial: String,
    },
    PrepareHardRefresh,
    HardRefresh,
    SetAttentionSeen {
        at_ms: u64,
    },
    ToggleAutomations {
        key: Option<String>,
    },
    CancelAutomations,
    SetHistoryActive {
        active: bool,
    },
    SetPerf {
        active: bool,
        continuous: bool,
        refresh: bool,
    },
    PrepareRestoreRemoved {
        key: String,
    },
    RestoreRemoved {
        key: String,
        removed_at: String,
        branch: String,
    },
    ToggleRemovedAutomations {
        key: String,
    },
    OpenLink {
        url: String,
    },
    OpenPrLink {
        url: String,
        linear: bool,
    },
    Restack {
        key: String,
    },
    PrepareGithub {
        key: String,
        ship: bool,
    },
    PrepareReviewers {
        key: String,
    },
    PrepareReviewCheckout {
        url: String,
        updated_at: String,
        branch: String,
    },
    ReviewCheckout {
        url: String,
        updated_at: String,
        branch: String,
    },
    DismissReviewRequest {
        url: String,
        updated_at: String,
    },
    SubmitReviewers {
        key: String,
        pr_number: u64,
        original: Vec<String>,
        selected: Vec<String>,
    },
    GithubMarkReady {
        key: String,
    },
    GithubSetAutoMerge {
        key: String,
        enable: bool,
    },
    GithubShip {
        key: String,
    },
    GithubFailedChecks {
        key: String,
    },
    KillAction {
        action_key: String,
        run_id: String,
    },
    PrepareActions {
        surface: ActionSurface,
    },
    PrepareAction {
        surface: ActionSurface,
        id: String,
        arg: Option<String>,
    },
    RunAction {
        surface: ActionSurface,
        id: String,
        arg: Option<String>,
        extras: String,
    },
    FoldSection {
        key: String,
        folded: bool,
    },
    PrepareSection {
        key: String,
    },
    MoveSection {
        key: String,
        section: Option<String>,
    },
    Reorder {
        key: Option<String>,
        section: String,
        down: bool,
    },
    RenameSection {
        old: String,
        new: String,
    },
    CyclePrimary,
    SetTitle {
        key: String,
        title: String,
    },
    GenerateTitle {
        key: String,
    },
    Copy {
        value: String,
        label: String,
    },
    Create {
        input: String,
    },
    OpenEditor {
        key: String,
    },
    PrepareRemove {
        key: String,
    },
    Remove {
        key: String,
        force: bool,
        revision: RemovalRevision,
    },
    PrepareCleanup,
    Cleanup {
        revisions: Vec<RemovalRevision>,
    },
    ToggleArchive {
        key: String,
    },
    PrepareStatus {
        key: String,
    },
    SetStatus {
        key: String,
        state: Option<String>,
        note: Option<String>,
        /// `None` preserves the current obligation; `Some("")` clears it.
        verify_after_merge: Option<String>,
    },
    PrepareBase {
        key: String,
    },
    SetBase {
        key: String,
        base: Option<String>,
    },
    SetIssueOverride {
        key: String,
        issue_id: Option<String>,
    },
    OpenUrl {
        key: String,
        kind: UrlKind,
    },
    Session {
        key: Option<String>,
        target: SessionTarget,
    },
    PrepareSessions {
        key: Option<String>,
        target: SessionTarget,
    },
    SelectSession {
        selection: SessionSelection,
    },
    PrepareStopSession {
        selection: SessionSelection,
    },
    PrepareStopTerminal {
        key: String,
        target: SessionTarget,
    },
    StopTerminal {
        key: String,
        target: SessionTarget,
        session_id: String,
        created_at: i64,
    },
    StopSession {
        selection: SessionSelection,
    },
}

/// Captured when opening a palette; moving the board later cannot retarget it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionSurface {
    Row { key: String },
    Manager { key: Option<String> },
    Slot { target: SessionTarget },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UrlKind {
    PullRequest,
    Issue,
    PrimaryIssue,
    StageOrDev,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionTarget {
    Harness,
    Shell,
    Diff,
    Manager,
    Main,
    WtSource,
    Dotfiles,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalRevision {
    pub key: String,
    pub path: String,
    pub branch: String,
    pub head: String,
    pub digest: String,
    pub hazards: Vec<String>,
    #[serde(default)]
    pub published_base: Option<Box<(String, String)>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfirmAction {
    HardRefresh,
    StopTerminal {
        key: String,
        target: SessionTarget,
        session_id: String,
        created_at: i64,
    },
    ReviewCheckout {
        url: String,
        updated_at: String,
        branch: String,
    },
    RestoreRemoved {
        key: String,
        removed_at: String,
        branch: String,
    },
    Github {
        key: String,
        ship: bool,
    },
    StopSession {
        selection: SessionSelection,
    },
    KillAction {
        action_key: String,
        run_id: String,
    },
    Remove {
        key: String,
        force: bool,
        revision: RemovalRevision,
    },
    Cleanup {
        revisions: Vec<RemovalRevision>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionMode {
    New,
    Resume,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSelection {
    pub key: Option<String>,
    pub target: SessionTarget,
    pub harness: wt_core::HarnessId,
    pub session_id: Option<String>,
    pub managed_name: Option<String>,
    pub mode: SessionMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PickerAction {
    Sessions {
        choices: Vec<SessionSelection>,
    },
    Output {
        choices: Vec<crate::output::OutputTarget>,
    },
    Host {
        action: Box<UiAction>,
    },
    Actions {
        surface: ActionSurface,
    },
    ActionArg {
        surface: ActionSurface,
        id: String,
    },
    Status {
        key: String,
    },
    Base {
        key: String,
    },
    Section {
        key: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextAction {
    SessionName {
        selection: SessionSelection,
    },
    ActionArg {
        surface: ActionSurface,
        id: String,
    },
    ActionExtras {
        surface: ActionSurface,
        id: String,
        arg: Option<String>,
    },
    Create,
    NewSection {
        key: String,
    },
    RenameSection {
        old: String,
    },
    IssueOverride {
        key: String,
    },
    StatusNote {
        key: String,
        state: String,
    },
    VerifyAfterMerge {
        key: String,
        state: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickerOption {
    pub value: Option<String>,
    pub label: String,
    pub chord: Option<char>,
    /// Current note used to prefill the `m` status-note prompt. Direct picker
    /// selection starts a fresh status assertion and does not carry this note.
    pub note: Option<String>,
    pub verify_after_merge: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerOption {
    pub login: String,
    pub label: String,
    pub selected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiModal {
    Reviewers {
        key: String,
        pr_number: u64,
        original: Vec<String>,
        candidates: Vec<ReviewerOption>,
    },
    Log {
        title: String,
        lines: Vec<String>,
    },
    Confirm {
        action: ConfirmAction,
        title: String,
        lines: Vec<String>,
        cancel_key: Option<char>,
    },
    Picker {
        action: PickerAction,
        title: String,
        options: Vec<PickerOption>,
        selected: usize,
    },
    Text {
        action: TextAction,
        prompt: String,
        initial: String,
        allow_empty: bool,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct UiReply {
    #[serde(skip)]
    pub ui_generation: Option<u64>,
    pub message: String,
    pub failed: bool,
    /// Creation can finish before the source publishes the new row. The model
    /// consumes this selection only once the actual row is visible.
    pub select_when_visible: Option<String>,
    /// Optional controller-prepared modal data. It never contains callbacks.
    pub modal: Option<UiModal>,
    /// Host-local keys in a returned modal remain host-local. The model wraps
    /// every resulting action in the captured host, including nested prompts.
    pub modal_host: Option<String>,
    /// A prepared child terminal session. The UI owns only terminal mode and
    /// input handoff; the controller owns the child process and its lifecycle.
    #[serde(skip)]
    pub handoff: Option<TerminalHandoff>,
    /// A primary harness the controller just persisted. The model shows it
    /// until the board's own value catches up.
    #[serde(default)]
    pub primary_harness: Option<String>,
}

#[derive(Debug)]
pub struct TerminalHandoff {
    pub ready: oneshot::Sender<()>,
    pub resumed: oneshot::Receiver<Result<(), String>>,
}

pub struct UiActions {
    pub requests: mpsc::Sender<UiRequest>,
    pub replies: mpsc::Receiver<UiReply>,
}

pub struct ActionController {
    pub requests: mpsc::Receiver<UiRequest>,
    pub replies: mpsc::Sender<UiReply>,
}

#[derive(Clone, Debug)]
pub struct UiRequest {
    pub generation: u64,
    pub action: UiAction,
}

pub fn action_channel() -> (UiActions, ActionController) {
    let (requests, incoming) = mpsc::channel(8);
    let (replies, outgoing) = mpsc::channel(16);
    (
        UiActions {
            requests,
            replies: outgoing,
        },
        ActionController {
            requests: incoming,
            replies,
        },
    )
}
