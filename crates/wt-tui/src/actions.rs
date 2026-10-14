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
    ResolveCreation {
        input: String,
    },
    CreateResolved {
        plan: CreatePlan,
    },
    /// Sign in on a managed host through an interactive SSH session, then
    /// continue the request that needed it.
    ManagedSignIn {
        provider: String,
        then: Option<Box<UiAction>>,
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
        #[serde(default)]
        pin: Option<PrPin>,
    },
    GithubSetAutoMerge {
        key: String,
        enable: bool,
        #[serde(default)]
        pin: Option<PrPin>,
    },
    GithubShip {
        key: String,
        #[serde(default)]
        pin: Option<PrPin>,
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
    CleanupTargets {
        targets: Vec<CleanupTarget>,
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

impl UiAction {
    pub const RENAME_ID: &str = "__rename__";

    /// Resolve built-in aliases before pending state, cancellation and host
    /// scheduling classify the request. AI title generation is an accepted
    /// command, not a cancellable menu preparation.
    pub fn resolve_palette_alias(self) -> Self {
        match self {
            Self::OnHost { host, action } => Self::OnHost {
                host,
                action: Box::new(action.resolve_palette_alias()),
            },
            Self::PrepareAction {
                surface: ActionSurface::Row { key },
                id,
                ..
            } if id == Self::RENAME_ID => Self::GenerateTitle { key },
            action => action,
        }
    }

    /// Requests that acquire the interactive terminal. They serialize with
    /// each other, independently of unrelated background work.
    pub fn is_terminal_handoff(&self) -> bool {
        match self {
            Self::OnHost { action, .. } => action.is_terminal_handoff(),
            Self::Session { .. }
            | Self::SelectSession { .. }
            | Self::EnterHarness { .. }
            | Self::PerfInvestigate { .. } => true,
            _ => false,
        }
    }

    /// Whether this request prepares a prompt, picker, or other presentation
    /// from read-only data. Controllers may cancel these requests during
    /// shutdown without cancelling an accepted mutation.
    pub fn is_preparation(&self) -> bool {
        match self {
            Self::OnHost { action, .. } => action.is_preparation(),
            Self::PrepareCreate { .. }
            | Self::PrepareHardRefresh
            | Self::ResolveCreation { .. }
            | Self::PrepareRestoreRemoved { .. }
            | Self::PrepareGithub { .. }
            | Self::PrepareReviewers { .. }
            | Self::PrepareReviewCheckout { .. }
            | Self::GithubFailedChecks { .. }
            | Self::PrepareActions { .. }
            | Self::PrepareAction { .. }
            | Self::PrepareSection { .. }
            | Self::PrepareRemove { .. }
            | Self::PrepareCleanup
            | Self::PrepareStatus { .. }
            | Self::PrepareBase { .. }
            | Self::PrepareSessions { .. }
            | Self::PrepareHarnesses { .. }
            | Self::PrepareStopTerminal { .. } => true,
            Self::ManagedSignIn { .. }
            | Self::HardRefresh
            | Self::SetAttentionSeen { .. }
            | Self::ToggleAutomations { .. }
            | Self::CancelAutomations
            | Self::SetHistoryActive { .. }
            | Self::SetPerf { .. }
            | Self::RestoreRemoved { .. }
            | Self::ToggleRemovedAutomations { .. }
            | Self::OpenLink { .. }
            | Self::OpenPrLink { .. }
            | Self::OpenPrDefault { .. }
            | Self::OpenSlotEditor { .. }
            | Self::PerfInvestigate { .. }
            | Self::Restack { .. }
            | Self::ReviewCheckout { .. }
            | Self::DismissReviewRequest { .. }
            | Self::SubmitReviewers { .. }
            | Self::GithubMarkReady { .. }
            | Self::GithubSetAutoMerge { .. }
            | Self::GithubShip { .. }
            | Self::KillAction { .. }
            | Self::RunAction { .. }
            | Self::FoldSection { .. }
            | Self::MoveSection { .. }
            | Self::Reorder { .. }
            | Self::RenameSection { .. }
            | Self::CyclePrimary
            | Self::SetTitle { .. }
            | Self::GenerateTitle { .. }
            | Self::Copy { .. }
            | Self::Create { .. }
            | Self::CreateResolved { .. }
            | Self::OpenEditor { .. }
            | Self::Remove { .. }
            | Self::Cleanup { .. }
            | Self::CleanupTargets { .. }
            | Self::ToggleArchive { .. }
            | Self::SetStatus { .. }
            | Self::SetBase { .. }
            | Self::SetIssueOverride { .. }
            | Self::OpenUrl { .. }
            | Self::Session { .. }
            | Self::SelectSession { .. }
            | Self::KillSession { .. }
            | Self::EnterHarness { .. }
            | Self::StopTerminal { .. }
            | Self::StopSession { .. } => false,
        }
    }
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

/// The pull request the user saw when asking for a GitHub write. The host
/// refuses the write when its confirmed data names a different PR or head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrPin {
    pub number: u64,
    pub head_sha: Option<String>,
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

/// A row selected from prepared facts. The host must check its current hazards
/// and capture a removal revision before it can queue removal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupTarget {
    pub key: String,
    pub path: String,
    pub branch: String,
    pub head: String,
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
        #[serde(default)]
        pin: Option<PrPin>,
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
    ManagedSignIn {
        provider: String,
        then: Option<Box<UiAction>>,
    },
    CleanupTargets {
        targets: Vec<CleanupTarget>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionMode {
    New,
    Resume,
}

/// Physical session captured by the picker. This is not a conversation UUID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSessionIdentity {
    pub id: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSelection {
    pub key: Option<String>,
    pub target: SessionTarget,
    pub harness: wt_core::HarnessId,
    pub session_id: Option<String>,
    #[serde(default)]
    pub live_slot: Option<LiveSessionIdentity>,
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
    /// Read-only host preparation; never starts a checkout by itself.
    #[serde(default)]
    pub creation_plan: Option<CreatePlan>,
    /// The controller waits for presentation to bind the optimistic row to
    /// its exact host-owned key before a checkout can enter inventory.
    #[serde(skip)]
    pub creation_prepared: Option<CreationPrepared>,
    #[serde(skip)]
    pub operation_id: Option<u64>,
    #[serde(skip)]
    pub ui_generation: Option<u64>,
    pub message: String,
    pub failed: bool,
    /// The key had nothing to act on; the toast reads as a notice.
    #[serde(default)]
    pub notice: bool,
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
    /// What happened to a write. `None` on a reply that the host has not
    /// classified; the host service fills it for every executed action.
    #[serde(default)]
    pub outcome: Option<ReplyOutcome>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePlan {
    pub key: String,
    pub branch: String,
    pub base: Option<String>,
    pub install: bool,
    pub open: bool,
    pub github_issue: Option<u64>,
}

#[derive(Debug)]
pub struct CreationPrepared {
    pub key: String,
    pub ready: oneshot::Sender<()>,
}

impl UiReply {
    /// Plain feedback; for a write the host fills in `Accepted`.
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }

    /// The request failed; any pending change is rolled back.
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            failed: true,
            ..Self::message(message)
        }
    }

    /// The write happened at `at` on the executing host's clock.
    pub fn accepted(message: impl Into<String>, at: wt_runtime::Stamp) -> Self {
        Self {
            outcome: Some(ReplyOutcome::Accepted(at)),
            ..Self::message(message)
        }
    }

    /// Nothing needed to change.
    pub fn no_change(message: impl Into<String>) -> Self {
        Self {
            outcome: Some(ReplyOutcome::NoChange),
            ..Self::message(message)
        }
    }

    /// The host will act later; the board shows it through its own data.
    pub fn deferred(message: impl Into<String>) -> Self {
        Self {
            outcome: Some(ReplyOutcome::Deferred),
            ..Self::message(message)
        }
    }

    /// The write may or may not have happened and is never resent.
    pub fn uncertain(message: impl Into<String>) -> Self {
        Self {
            notice: true,
            outcome: Some(ReplyOutcome::Uncertain),
            ..Self::message(message)
        }
    }
}

/// How a pending mutation ends or continues after the host answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplyOutcome {
    /// The write happened at this moment on the executing host's clock. Only
    /// a fetch that started at or after it can confirm the change.
    Accepted(wt_runtime::Stamp),
    /// Nothing needed to change; stop showing the request.
    NoChange,
    /// The host will act later (e.g. arm once checks report); the board shows
    /// that through its own data, not this request.
    Deferred,
    /// The write may or may not have happened. Shown until its deadline and
    /// never resent automatically.
    Uncertain,
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
    pub id: u64,
    pub generation: u64,
    pub action: UiAction,
    /// Only read-only preparations may be abandoned by their presentation owner.
    pub cancellation: Option<tokio_util::sync::CancellationToken>,
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
