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
    /// Open a pull request URL at the configured `[github].pr_target`.
    /// Used where the UI has only the URL, such as a requested review.
    OpenPrDefault {
        url: String,
    },
    /// Open a special slot's checkout (`O` = main clone, slot palette `z`)
    /// in the editor. Always on this machine.
    OpenSlotEditor {
        target: SessionTarget,
    },
    /// Perf overlay `i`: send the shown snapshot to the wt-source session
    /// as an investigation prompt, then enter that session.
    PerfInvestigate {
        report: Vec<String>,
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
    /// `; x`: kill a live session directly, or forget a dead Claude
    /// session's stored name. No confirmation, as in the TS picker.
    KillSession {
        selection: SessionSelection,
    },
    /// Shift+F12: list the visible harnesses for a worktree.
    PrepareHarnesses {
        key: String,
    },
    /// Harness picker commit: Claude spawns a new auto-named session; other
    /// harnesses attach their live slot or start it.
    EnterHarness {
        key: String,
        harness: wt_core::HarnessId,
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
    /// `; d`: close a live session gracefully (Claude: kill the tmux
    /// session; others: Ctrl+D twice). The reply says when nothing was live.
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
    /// A Claude `New` selection with no name gets the next automatic name.
    pub managed_name: Option<String>,
    pub mode: SessionMode,
    /// The session was live when the picker was prepared. Execution still
    /// revalidates the exact identity.
    #[serde(default)]
    pub live: bool,
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
    /// Shift+F12 harness picker. Option values are `HarnessId::as_str`.
    Harness {
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
    /// Dim right-aligned text: an action's kind and id, or why it is
    /// unavailable.
    #[serde(default)]
    pub detail: Option<String>,
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
        /// An extra key that closes the overlay, such as `l` for `! l`.
        #[serde(default)]
        close_key: Option<char>,
        /// Re-sent about once a second while the overlay stays open; the
        /// reply's Log replaces the lines and keeps the scroll position.
        #[serde(default)]
        refresh: Option<Box<UiAction>>,
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
