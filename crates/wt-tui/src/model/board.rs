//! Prepared presentation data the host publishes: the board, its rows and
//! sections, and the facts each pane draws.

/// Prepared, immutable presentation data. Service code sanitizes terminal
/// controls and computes labels once when the underlying source changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Board {
    pub name: String,
    #[serde(default)]
    pub automations_paused: bool,
    #[serde(default)]
    pub full_width_activity: bool,
    pub rows: Vec<BoardRow>,
    pub activity: Vec<ActivityLine>,
    #[serde(default)]
    pub attention: Vec<AttentionLine>,
    #[serde(default)]
    pub attention_seen_ms: u64,
    /// Controller-local display labels prepared off the input/render thread.
    /// The source timestamps remain UTC epoch milliseconds.
    #[serde(default)]
    pub local_times: std::collections::BTreeMap<u64, String>,
    #[serde(default)]
    pub slot_logs: std::collections::BTreeMap<String, Vec<LogView>>,
    #[serde(default)]
    pub removed_history: RemovedHistorySnapshot,
    #[serde(default)]
    pub review_requests: Vec<ReviewRequestRow>,
    #[serde(default)]
    pub perf: Vec<String>,
    /// Typed local perf sample for the `P` overlay; `perf` stays the plain
    /// report that `i` hands to an agent and remote hosts forward.
    #[serde(default)]
    pub perf_view: Option<Box<PerfView>>,
    pub sections: Vec<BoardSection>,
    pub hosts: Vec<HostChoice>,
    /// Prepared host failures. Retained rows are not current host observations.
    #[serde(default)]
    pub host_errors: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub usage: Vec<UsageItem>,
    #[serde(default)]
    pub slot_sessions: std::collections::BTreeMap<String, Vec<SessionView>>,
    /// Physical managed terminals, independent of historical conversation IDs.
    /// Presentation only: never authorizes session routing or idle automation.
    #[serde(default)]
    pub live_slots: std::collections::BTreeMap<String, Vec<LiveSlotView>>,
    #[serde(default)]
    pub display: DisplayPolicy,
    /// Queued automation fires waiting to dispatch.
    #[serde(default)]
    pub automations_pending: usize,
    /// Provenance and observation stamps of each remote host's domains, keyed
    /// by `BoardRow::host`. The local host's come with the snapshot itself.
    #[serde(default)]
    pub host_domains: std::collections::BTreeMap<String, wt_runtime::DomainMap>,
    /// Changes shown before a fetch confirms them: host-side projections
    /// (e.g. an issue status an action is setting) and, in the model's
    /// presented board, the user's own pending requests.
    #[serde(default, rename = "pending")]
    pub pending_views: Vec<wt_runtime::PendingView>,
}

/// One prepared perf sample. CPU values are `ps` percentages (100 per core);
/// memory values are KiB of RSS unless named otherwise.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PerfView {
    /// Controller-local clock label of the sample, e.g. `14:03:22`.
    pub sampled_at: String,
    pub verdict: String,
    pub verdict_tone: PerfTone,
    pub cores: u32,
    pub system_cpu: f64,
    pub wt_cpu: f64,
    pub wt_rss_kb: u64,
    pub downstream_count: usize,
    pub load_average: Option<[f64; 3]>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub categories: Vec<PerfGroupView>,
    pub sessions: Vec<PerfGroupView>,
    pub top_downstream: Vec<PerfProcessView>,
    pub top_other: Vec<PerfProcessView>,
    pub orphans: Vec<PerfProcessView>,
    pub orphan_probe_available: bool,
    pub tmux_probe_available: bool,
    /// The latest resample failure while this older sample stays shown.
    pub error: Option<String>,
}

// Sampled values come from parsed `ps` decimals and sums of them, never NaN,
// so equality is total for the snapshot deduplication `Board` relies on.
impl Eq for PerfView {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfTone {
    /// The machine is not busy.
    #[default]
    Calm,
    /// wt is most of the load.
    Ours,
    /// The machine is busy, mostly outside wt.
    Elsewhere,
}

/// A category or session total.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PerfGroupView {
    pub label: String,
    pub cpu: f64,
    pub rss_kb: u64,
    pub count: usize,
    /// Session composition such as `agents + shells×2`; empty for categories.
    pub summary: String,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PerfProcessView {
    pub pid: u32,
    pub cpu: f64,
    pub rss_kb: u64,
    pub elapsed: String,
    /// Command with the directory of argv[0] removed.
    pub command: String,
    pub session: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActivityLine {
    pub at_ms: u64,
    pub level: String,
    pub channel: String,
    pub source: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttentionLine {
    pub at_ms: u64,
    pub source: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveSlotView {
    pub harness: String,
    pub state: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionView {
    pub id: String,
    pub harness: String,
    pub name: String,
    pub state: String,
    pub live: bool,
    pub queued: u32,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub context_percent: Option<u8>,
    pub output: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogView {
    pub id: String,
    pub title: String,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemovedHistorySnapshot {
    pub rows: Vec<RemovedHistoryRow>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemovedHistoryRow {
    pub key: String,
    pub host: Option<String>,
    pub slug: String,
    pub branch: String,
    pub title: String,
    pub removed_at: String,
    #[serde(default)]
    pub day_label: Option<String>,
    #[serde(default)]
    pub age: Option<String>,
    pub details: Vec<String>,
    pub issue_url: Option<String>,
    pub pr_url: Option<String>,
    pub issue_status: Option<String>,
    pub production_landed: Option<bool>,
    #[serde(default)]
    pub landed_on: Option<LandingKind>,
    pub automations_paused: bool,
    /// Saved work state at removal, such as `ready`.
    #[serde(default)]
    pub work_state: Option<String>,
    #[serde(default)]
    pub blocked_on: Option<String>,
    /// Saved verify-after-merge steps, and whether they were still owed.
    #[serde(default)]
    pub verify_steps: Option<String>,
    #[serde(default)]
    pub verify_owed: bool,
    /// Git and PR outcome recorded at removal, such as `merged`.
    #[serde(default)]
    pub git_state: Option<String>,
    #[serde(default)]
    pub pr_state: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostChoice {
    pub id: Option<String>,
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewRequestRow {
    pub host: Option<String>,
    pub url: String,
    pub updated_at: String,
    pub branch: String,
    pub title: String,
    pub number: u64,
    pub author: String,
    pub details: Vec<String>,
    pub issue_url: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub checks: CheckState,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BoardSection {
    pub key: String,
    pub title: String,
    pub folded: bool,
    /// Indices into the prepared board's rows, already sorted by the source.
    pub rows: Vec<usize>,
    #[serde(default)]
    pub rollup: SectionRollup,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SectionRollup {
    pub states: Vec<WorkStateCount>,
    pub risks: Vec<WorkRiskCount>,
    pub blocked_notes: Vec<String>,
    pub stale_statuses: usize,
    pub verification_owed: usize,
    pub verification_overdue: usize,
    pub dirty_worktrees: Option<usize>,
    pub unknown_git: usize,
    pub upstream_ahead: usize,
    pub upstream_behind: usize,
    pub rebasing: usize,
    pub conflicted: usize,
    pub open_prs: usize,
    pub draft_prs: usize,
    pub queued_prs: usize,
    pub failing_checks: usize,
    pub paused_automations: usize,
    pub needs_attention: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkStateCount {
    pub state: Option<wt_core::WorkState>,
    pub count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkRiskCount {
    pub risk: wt_core::WorkRisk,
    pub count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandingKind {
    Base,
    Production,
}

/// Git facts prepared by the host source. Missing fields mean unknown, not clean.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GitPresentation {
    pub head_sha: Option<String>,
    pub error: Option<String>,
    pub tracked_changes: Option<u32>,
    pub untracked_files: Option<u32>,
    pub upstream: Option<String>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub landed_on: Option<LandingKind>,
    pub rebasing: bool,
    /// Unmerged paths in the working tree (a rebase or merge stopped on them).
    pub conflict_files: Vec<String>,
    /// Pre-flight: files that would conflict if HEAD were rebased onto its
    /// base now. `Some(empty)` when the probe ran clean; `None` when it has
    /// not run or its result is unknown (never render that as clean).
    pub base_conflicts: Option<Vec<String>>,
    pub pr_title: Option<String>,
    pub first_commit_title: Option<String>,
    /// Work since the fork point, committed and uncommitted together.
    pub diff: Option<DiffStat>,
    /// UTC epoch milliseconds of the newest commit on HEAD.
    pub last_commit_ms: Option<u64>,
    /// UTC epoch milliseconds when the checkout was created.
    pub created_ms: Option<u64>,
    /// Commits ahead of and behind the row's base branch.
    pub base_ahead: Option<u32>,
    pub base_behind: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DiffStat {
    pub files: u32,
    pub added: u32,
    pub removed: u32,
}

/// Persisted status plus display-time derivations. This never authorizes actions.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WorkPresentation {
    pub record: Option<wt_core::WorkStatusRecord>,
    pub effective_state: Option<wt_core::WorkState>,
    pub derived: bool,
    pub age: Option<String>,
    pub stale: Option<bool>,
    pub verification_owed: bool,
    pub verification_overdue: bool,
    pub blocked: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PrPresentation {
    pub head_sha: Option<String>,
    pub number: Option<u64>,
    pub url: Option<String>,
    pub title: Option<String>,
    /// GitHub's PR state, upper case: `OPEN`, `MERGED`, or `CLOSED`.
    pub state: Option<String>,
    pub draft: bool,
    pub base_branch: Option<String>,
    pub checks: CheckState,
    pub failed_checks: Vec<String>,
    pub review: ReviewState,
    pub reviewers: Vec<String>,
    pub review_bot: Option<ReviewBotView>,
    pub unresolved_threads: u32,
    pub merge_queue: Option<MergeQueueView>,
    pub auto_merge_armed: bool,
    pub comments: Vec<PrCommentView>,
    pub error: Option<String>,
}

impl PrPresentation {
    pub fn is_open(&self) -> bool {
        self.state.as_deref() == Some("OPEN")
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    #[default]
    None,
    Pass,
    Fail,
    Pending,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    #[default]
    None,
    Approved,
    ChangesRequested,
    Pending,
    Unrequested,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ReviewBotView {
    /// `clean`, `pending`, `unresolved`, or `none`.
    pub state: String,
    pub unresolved: u32,
    /// The review describes an older head commit.
    pub stale: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct MergeQueueView {
    pub position: u32,
    /// `MERGEABLE`, `AWAITING_CHECKS`, `QUEUED`, `UNMERGEABLE`, or `LOCKED`.
    pub state: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PrCommentView {
    pub author: String,
    pub body: String,
    /// Prepared relative age, such as `3h`.
    pub age: Option<String>,
}

/// Where the row's title came from. Shown dimly beside the details title.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    Manual,
    Llm,
    Pr,
    Commit,
    #[default]
    Slug,
}

impl TitleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Llm => "llm",
            Self::Pr => "pr",
            Self::Commit => "commit",
            Self::Slug => "slug",
        }
    }
}

/// An operation holding the worktree's lock, as reported by the lock owner.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BusyView {
    /// Lock operation, such as `remove`, `restack`, or `init`.
    pub op: String,
    /// Human label, such as `removing`.
    pub label: String,
    pub age: Option<String>,
}

/// One harness rate-limit window or spend figure for the title bar.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UsageItem {
    /// `Claude`, `Codex`, or `OpenCode`.
    pub harness: String,
    /// Window label, such as `5h` or `7d`.
    pub period: String,
    /// Utilization percent of the window.
    pub percent: Option<u8>,
    /// UTC epoch milliseconds when the window resets.
    pub resets_at_ms: Option<u64>,
    /// Spend, already formatted (`$1.20`), for harnesses that report cost.
    pub cost: Option<String>,
}

/// Render-time display policy prepared from configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DisplayPolicy {
    /// `[ui].hidden_badges` slot names.
    pub hidden_badges: Vec<String>,
    /// The configured review bot is CodeRabbit (carrot glyph).
    pub review_bot_carrot: bool,
    /// The review bot reports through a checklist and also reviews drafts.
    pub review_bot_checklist: bool,
    /// Human reviewer requests are configured.
    pub reviewers: bool,
    /// A production branch is configured, so release position owns the marker.
    pub production: bool,
    /// Automations are configured at all.
    pub automations: bool,
    /// Primary harness name: `Claude`, `Codex`, or `OpenCode`.
    pub primary_harness: String,
}

/// One explicitly identified detail pane group. `id` is matched against
/// `[ui].rows`; rendering never infers group identity from its text.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PreparedDetailGroup {
    pub id: String,
    pub label: String,
    pub lines: Vec<String>,
    pub error: Option<String>,
}

/// Board key of the inbox section, prepared by the host.
pub(crate) const INBOX_SECTION: &str = "\0inbox";
/// Board key of the archived block, prepared by the host.
pub(crate) const ARCHIVED_SECTION: &str = "\0archived";

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BoardRow {
    /// Optimistic creation has no actionable inventory identity yet. Only the
    /// pending projection sets this; sources publish real rows with false.
    #[serde(default)]
    pub pending_creation: bool,
    pub key: String,
    pub host: Option<String>,
    pub slug: String,
    pub title: String,
    pub branch: String,
    pub base_branch: Option<String>,
    pub path: String,
    pub badge: String,
    pub details: Vec<String>,
    pub needs_attention: bool,
    #[serde(default)]
    pub work_rank: u8,
    #[serde(default)]
    pub verify_steps: Option<String>,
    pub issue_id: Option<String>,
    pub issue_status: Option<String>,
    pub issue_url: Option<String>,
    pub github_issue_url: Option<String>,
    pub pr_url: Option<String>,
    pub stage_url: Option<String>,
    /// Configured stage name for this row, offered by the yank picker.
    #[serde(default)]
    pub stage_name: Option<String>,
    pub dev_url: Option<String>,
    pub archived: bool,
    /// Stack rail, one character per column (`┌`, `├`, `│└`), empty for
    /// rows that draw no rail.
    pub stack_prefix: String,
    /// Parallel-lane index of the row's stack position; tints the rail.
    #[serde(default)]
    pub stack_lane: u8,
    /// The section holding this stacked row's parent when it differs from
    /// the row's own, so a split stack can still be followed.
    #[serde(default)]
    pub split_parent_section: Option<String>,
    #[serde(default)]
    pub title_source: TitleSource,
    #[serde(default)]
    pub busy: Option<BusyView>,
    /// The checkout directory no longer exists.
    #[serde(default)]
    pub path_missing: bool,
    /// The branch was deleted from the remote.
    #[serde(default)]
    pub branch_gone: bool,
    /// A tracked headless action is running for this row.
    #[serde(default)]
    pub action_running: bool,
    /// A stage is deployed or a dev server is running for this row.
    #[serde(default)]
    pub environment_live: bool,
    /// Dev server summary for the details pane, such as `running :8701`.
    #[serde(default)]
    pub dev_status: Option<String>,
    #[serde(default)]
    pub work: Option<WorkPresentation>,
    #[serde(default)]
    pub git: GitPresentation,
    #[serde(default)]
    pub pr: Option<PrPresentation>,
    #[serde(default)]
    pub automations_paused: bool,
    #[serde(default)]
    pub detail_groups: Vec<PreparedDetailGroup>,
    #[serde(default)]
    pub sessions: Vec<SessionView>,
    /// Physical harness slots observed on this row's host. Display activity
    /// does not establish a transcript identity or authorize session actions.
    #[serde(default)]
    pub live_sessions: Vec<LiveSlotView>,
    #[serde(default)]
    pub logs: Vec<LogView>,
}
