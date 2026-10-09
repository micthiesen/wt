use tokio::sync::{mpsc, oneshot};

/// Requests cross the input boundary as data. The controller owns execution,
/// persistence and refresh; the terminal never waits for an action to finish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiAction {
    CyclePrimary,
    SetTitle {
        key: String,
        title: String,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlKind {
    PullRequest,
    Issue,
    PrimaryIssue,
    StageOrDev,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTarget {
    Harness,
    Shell,
    Diff,
    Manager,
    Main,
    WtSource,
    Dotfiles,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalRevision {
    pub key: String,
    pub path: String,
    pub branch: String,
    pub head: String,
    pub digest: String,
    pub hazards: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmAction {
    Remove {
        key: String,
        force: bool,
        revision: RemovalRevision,
    },
    Cleanup {
        revisions: Vec<RemovalRevision>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerAction {
    Status { key: String },
    Base { key: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextAction {
    Create,
    IssueOverride { key: String },
    StatusNote { key: String, state: String },
    VerifyAfterMerge { key: String, state: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerOption {
    pub value: Option<String>,
    pub label: String,
    pub chord: Option<char>,
    /// Current note used to prefill the `m` status-note prompt. Direct picker
    /// selection starts a fresh status assertion and does not carry this note.
    pub note: Option<String>,
    pub verify_after_merge: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiModal {
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

#[derive(Debug, Default)]
pub struct UiReply {
    pub message: String,
    pub failed: bool,
    /// Creation can finish before the source publishes the new row. The model
    /// consumes this selection only once the actual row is visible.
    pub select_when_visible: Option<String>,
    /// Optional controller-prepared modal data. It never contains callbacks.
    pub modal: Option<UiModal>,
    /// A prepared child terminal session. The UI owns only terminal mode and
    /// input handoff; the controller owns the child process and its lifecycle.
    pub handoff: Option<TerminalHandoff>,
}

#[derive(Debug)]
pub struct TerminalHandoff {
    pub ready: oneshot::Sender<()>,
    pub resumed: oneshot::Receiver<Result<(), String>>,
}

pub struct UiActions {
    pub requests: mpsc::Sender<UiAction>,
    pub replies: mpsc::Receiver<UiReply>,
}

pub struct ActionController {
    pub requests: mpsc::Receiver<UiAction>,
    pub replies: mpsc::Sender<UiReply>,
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
