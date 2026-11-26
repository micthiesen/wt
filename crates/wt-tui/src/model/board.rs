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
    /// What the automations overlay lists: running, queued and recent fires.
    #[serde(default)]
    pub automations_view: AutomationsView,
    /// A newer wt release ("v0.1.40 available", "restart for v0.1.40").
    #[serde(default)]
    pub update_notice: Option<String>,
    /// Running wt and events daemon versions, shown in the help title
    /// ("wt v0.1.66 · events v0.1.66").
    #[serde(default)]
    pub versions: String,
    /// Provenance and observation stamps of each remote host's domains, keyed
    /// by `BoardRow::host`. The local host's come with the snapshot itself.
    #[serde(default)]
    pub host_domains: std::collections::BTreeMap<String, wt_runtime::DomainMap>,
    /// Changes shown before a fetch confirms them: host-side projections
    /// (e.g. an issue status an action is setting) and, in the model's
    /// presented board, the user's own pending requests.
    #[serde(default, rename = "pending")]
    pub pending_views: Vec<wt_runtime::PendingView>,
    /// Each row's recorded history, oldest first, keyed by `BoardRow::key`.
    /// Only the newest events of a row are carried; a row with none is absent.
    #[serde(default)]
    pub row_history: std::collections::BTreeMap<String, Vec<wt_core::WorktreeEvent>>,
    /// Controller-local day and clock labels for `row_history` timestamps,
    /// prepared off the render thread like `local_times`. A history whose
    /// timestamps are not all labelled is shown with relative ages instead.
    #[serde(default)]
    pub history_times: std::collections::BTreeMap<u64, LocalStamp>,
    /// Manually ordered section names (`sectionsOrder`), for the section menu.
    #[serde(default)]
    pub section_order: Vec<String>,
    /// Prepared session-picker entries keyed by [`crate::session_menu_key`]:
    /// cached conversation history joined with one live tmux inventory.
    /// Entries only nominate choices; attachment revalidates live identity.
    #[serde(default)]
    pub session_menus: std::collections::BTreeMap<String, Vec<SessionMenuEntry>>,
    /// Live shell and diff terminals keyed by row key, with the tmux identity
    /// a stop request must match.
    #[serde(default)]
    pub terminals: std::collections::BTreeMap<String, Vec<TerminalSlotView>>,
    /// Action palettes the owning host prepared while idle, keyed by
    /// [`crate::ActionSurface::menu_key`]. Absent or stale entries fall back
    /// to asking the host; a commit is always checked there again.
    #[serde(default)]
    pub palettes: std::collections::BTreeMap<String, PreparedPalette>,
}

impl Board {
    /// Whether the automation engine has anything to show or to pause:
    /// configured rules, fires it holds or ran, or a row that waits for
    /// something. Watches run under an implicit rule, so a repository with no
    /// `[[automations]]` still queues their fires.
    pub fn automations_shown(&self) -> bool {
        let view = &self.automations_view;
        self.display.automations
            || self.automations_pending > 0
            || !(view.running.is_empty() && view.queued.is_empty() && view.recent.is_empty())
            || self.rows.iter().any(|row| !row.watches.is_empty())
    }
}

/// One action palette prepared by the host for a surface.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreparedPalette {
    pub modal: crate::UiModal,
}

/// One session-picker entry prepared by the owning host.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMenuEntry {
    pub selection: crate::SessionSelection,
    pub display_name: String,
    pub state: Option<String>,
    pub is_live: bool,
    pub last_active_ms: Option<i64>,
}

/// A live shell or diff terminal and its exact tmux identity.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalSlotView {
    pub target: crate::SessionTarget,
    pub name: String,
    pub id: String,
    pub created_at: i64,
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

/// One thing a worktree waits for, prepared by the host that stores it: a
/// gate that clears itself, a wake for its session, or a notification.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WatchView {
    /// Empty while the watch is only this controller's pending request.
    pub id: String,
    /// Who set it: a worktree slug, `manager` or `human`.
    pub by: String,
    /// All must hold.
    pub conditions: Vec<WatchConditionView>,
    /// What it still owes once they do, in delivery order.
    pub actions: Vec<WatchActionKind>,
    /// The first line of the wake prompt or the notification text.
    pub headline: Option<String>,
    /// Every condition held and its actions are being delivered.
    pub met: bool,
    /// Automations are paused for the row or everywhere, so a met watch is
    /// not delivered until they resume.
    pub held: bool,
}

impl WatchView {
    pub fn does(&self, action: WatchActionKind) -> bool {
        self.actions.contains(&action)
    }

    /// Whether it waits for one instant and nothing else: that reads "at
    /// 17:00" where anything else reads "when merged".
    pub fn timed(&self) -> bool {
        matches!(self.conditions.as_slice(), [only] if only.at_ms.is_some())
    }

    /// A watch that only tells the person, set by the person: what the
    /// notify menu offers to cancel.
    pub fn is_human_notify(&self) -> bool {
        self.by == "human" && self.actions == [WatchActionKind::Notify]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchActionKind {
    /// Clear the merge gate.
    Unblock,
    /// Tell the person.
    Notify,
    /// Send the worktree's session its prompt.
    Wake,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WatchConditionView {
    /// The condition as stored (`ready`, `#3806:production`): its identity
    /// within the watch, never shown.
    pub key: String,
    /// The condition in words, with a time in the host's local clock.
    pub text: String,
    pub state: WatchTruth,
    /// Why it is unknown, or what a holding condition still waits out.
    pub reason: Option<String>,
    /// A time condition's instant, UTC epoch milliseconds. The controller
    /// labels it in `Board::history_times`.
    pub at_ms: Option<u64>,
    /// How long until a time condition comes due, such as `2h10m`.
    pub due_in: Option<String>,
}

/// One thing a worktree owes, prepared by the host that stores it from
/// `wt_core::owed_items`: the merge gate, a check owed in a stage, or a step
/// the worktree comes back for. Every pane that says what a row owes reads
/// this list.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OwedView {
    /// What `wt done` takes for a due step; the watch's id for a waiting
    /// step or a gate something watches.
    #[serde(default)]
    pub id: Option<String>,
    pub kind: wt_core::OwedKind,
    pub state: wt_core::OwedState,
    /// A check's stage, or the stage of the row's own a step waits for.
    #[serde(default)]
    pub stage: Option<wt_core::VerifyStage>,
    /// The gate text, the check's steps or the step's prompt, in full.
    pub what: String,
    /// One line naming it, as `wt owed` prints it.
    pub headline: String,
    /// When it is due or cleared, in words with the host's clock.
    #[serde(default)]
    pub when: String,
    /// The conditions a gate's watch or a waiting step waits for, with each
    /// one's truth as the engine last recorded it.
    #[serde(default)]
    pub conditions: Vec<WatchConditionView>,
    /// When a step came due, UTC epoch milliseconds, and the host's label
    /// for it (`11:36`, `Nov 27 18:12`).
    #[serde(default)]
    pub due_at_ms: Option<u64>,
    #[serde(default)]
    pub due_at: Option<String>,
    /// How long a check that was put off still waits, such as `1h`.
    #[serde(default)]
    pub not_before_in: Option<String>,
    /// Who set a step.
    #[serde(default)]
    pub by: Option<String>,
    #[serde(default)]
    pub before_release: bool,
    #[serde(default)]
    pub overdue: bool,
    #[serde(default)]
    pub awaiting_deploy: bool,
    /// A due step whose wake was sent without proof it arrived.
    #[serde(default)]
    pub unconfirmed: bool,
    /// Its conditions held and delivery is under way.
    #[serde(default)]
    pub met: bool,
    /// Automations are paused for the row or everywhere, so nothing is
    /// delivered until they resume.
    #[serde(default)]
    pub held: bool,
    /// Its watch also wakes the row's session: a gate set with `--then`.
    #[serde(default)]
    pub wakes: bool,
}

impl OwedView {
    /// An item with nothing but its kind, state and text.
    pub fn new(kind: wt_core::OwedKind, state: wt_core::OwedState, what: &str) -> Self {
        let line = wt_core::watch::headline(what);
        Self {
            id: None,
            kind,
            state,
            stage: None,
            what: what.to_owned(),
            headline: match kind {
                wt_core::OwedKind::Check => format!("check: {line}"),
                wt_core::OwedKind::Gate | wt_core::OwedKind::Step => line,
            },
            when: String::new(),
            conditions: Vec::new(),
            due_at_ms: None,
            due_at: None,
            not_before_in: None,
            by: None,
            before_release: false,
            overdue: false,
            awaiting_deploy: false,
            unconfirmed: false,
            met: false,
            held: false,
            wakes: false,
        }
    }

    pub fn due(&self) -> bool {
        self.state == wt_core::OwedState::Due
    }

    /// Whether removing the worktree would lose it. A merge gate alone holds
    /// nothing once the work has landed; the wake that follows one does.
    pub fn holds_cleanup(&self) -> bool {
        self.kind != wt_core::OwedKind::Gate || self.wakes
    }

    /// A condition it waits for cannot be read, so it may never come due.
    pub fn unreadable(&self) -> bool {
        !self.met
            && self
                .conditions
                .iter()
                .any(|cond| cond.state == WatchTruth::Unknown)
    }
}

/// Key prefix of the blocks `[ui] owed_sections` derives for landed rows.
pub const OWED_BLOCK_PREFIX: &str = "\0owed:";
/// The option's name as notices print it.
pub const OWED_SECTIONS_OPTION: &str = "[ui] owed_sections";

/// Why a row or block under a derived block cannot be moved. `title` is the
/// block's.
pub fn owed_block_refusal(title: &str) -> String {
    format!(
        "{title} is derived from what landed rows owe; set {OWED_SECTIONS_OPTION} = false to file rows by hand"
    )
}

/// The derived block a landed row is shown in under `[ui] owed_sections`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OwedBlock {
    Due,
    WaitingStaging,
    WaitingProduction,
    Waiting,
    Done,
}

impl OwedBlock {
    pub const ALL: [Self; 5] = [
        Self::Due,
        Self::WaitingStaging,
        Self::WaitingProduction,
        Self::Waiting,
        Self::Done,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Due => "\0owed:due",
            Self::WaitingStaging => "\0owed:staging",
            Self::WaitingProduction => "\0owed:production",
            Self::Waiting => "\0owed:waiting",
            Self::Done => "\0owed:done",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Due => "Due",
            Self::WaitingStaging => "Waiting for staging",
            Self::WaitingProduction => "Waiting for production",
            Self::Waiting => "Waiting",
            Self::Done => "Done",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|block| block.key() == key)
    }

    /// Where a landed row with this phase and list belongs. A waiting row is
    /// filed by the first thing removal would lose.
    pub fn of(phase: wt_core::OwedPhase, owes: &[OwedView]) -> Self {
        match phase {
            wt_core::OwedPhase::Due => Self::Due,
            wt_core::OwedPhase::Done => Self::Done,
            wt_core::OwedPhase::Waiting => {
                match owes
                    .iter()
                    .find(|item| item.holds_cleanup())
                    .and_then(|item| item.stage)
                {
                    Some(wt_core::VerifyStage::Staging) => Self::WaitingStaging,
                    Some(wt_core::VerifyStage::Production) => Self::WaitingProduction,
                    None => Self::Waiting,
                }
            }
        }
    }
}

/// A condition's truth as the engine last recorded it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchTruth {
    Holds,
    #[default]
    NotYet,
    /// Not known now; never shown as holding or as not holding.
    Unknown,
}

/// One timestamp in the controller's timezone.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LocalStamp {
    /// The day heading it falls under: `today`, `yesterday`, `tomorrow`,
    /// or a date such as `Mon 23 Nov`.
    pub day: String,
    /// The clock time, `14:32`.
    pub clock: String,
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
    /// What the removed row still owes: its checks per stage and the steps
    /// that had come due.
    #[serde(default)]
    pub owes: Vec<OwedView>,
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
    /// Providers a managed host still needs a sign-in for, by label.
    #[serde(default)]
    pub sign_in_needed: Vec<String>,
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
}

impl Board {
    /// The unarchived rows that belong to a named section, as indices in
    /// board order: those drawn in it, and those filed in it but drawn in a
    /// derived block. What is stored decides, because that is what deleting
    /// or counting the section acts on.
    pub fn section_members(&self, name: &str) -> Vec<usize> {
        let drawn: std::collections::BTreeSet<usize> = self
            .sections
            .iter()
            .filter(|section| section.key == name)
            .flat_map(|section| section.rows.iter().copied())
            .collect();
        self.rows
            .iter()
            .enumerate()
            .filter(|(index, row)| {
                !row.archived && (drawn.contains(index) || row.filed_in.as_deref() == Some(name))
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// The rows deleting a section would move: its members that are not
    /// being removed and not a pending creation. Sorted keys, so the TUI's
    /// witness and the host's check of the stored placement compare equal.
    pub fn section_witness(&self, name: &str) -> Vec<String> {
        let mut keys: Vec<String> = self
            .section_members(name)
            .into_iter()
            .map(|row| &self.rows[row])
            .filter(|row| row.removal.is_none() && !row.pending_creation)
            .map(|row| row.key.clone())
            .collect();
        keys.sort();
        keys
    }
}

impl BoardSection {
    /// Whether the list draws this section. A named section stays on screen
    /// (and is a cursor stop) while empty; a derived group such as the Inbox
    /// or the archive appears only with rows. Rendering, cursor stops and
    /// row moves all ask this, so a row never lands somewhere unseen.
    pub fn drawn(&self) -> bool {
        !self.rows.is_empty() || !self.key.starts_with('\0')
    }

    /// A block `[ui] owed_sections` derives from what landed rows owe.
    /// Nothing is filed in one: rows cannot be moved into, out of or inside
    /// it.
    pub fn owed_block(&self) -> bool {
        self.key.starts_with(OWED_BLOCK_PREFIX)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SectionRollup {
    pub states: Vec<WorkStateCount>,
    pub risks: Vec<WorkRiskCount>,
    pub blocked_notes: Vec<String>,
    pub stale_statuses: usize,
    /// Landed rows that still owe something.
    pub owed: usize,
    /// Of those, rows with something due now.
    pub owed_due: usize,
    pub owed_overdue: usize,
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

impl SectionRollup {
    /// Summarize the rows a section draws. Computed where it is drawn, from
    /// the rows on screen, so a summary can never describe an older board
    /// than its members.
    pub fn of(rows: &[&BoardRow]) -> Self {
        let mut states = Vec::new();
        for state in wt_core::WORK_STATES {
            let count = rows
                .iter()
                .filter(|row| {
                    row.work.as_ref().and_then(|work| work.effective_state) == Some(state)
                })
                .count();
            if count > 0 {
                states.push(WorkStateCount {
                    state: Some(state),
                    count,
                });
            }
        }
        let unset = rows
            .iter()
            .filter(|row| {
                row.work
                    .as_ref()
                    .and_then(|work| work.effective_state)
                    .is_none()
            })
            .count();
        if unset > 0 {
            states.push(WorkStateCount {
                state: None,
                count: unset,
            });
        }
        let risks = [
            wt_core::WorkRisk::High,
            wt_core::WorkRisk::Medium,
            wt_core::WorkRisk::Low,
        ]
        .into_iter()
        .filter_map(|risk| {
            let count = rows
                .iter()
                .filter(|row| {
                    row.work.as_ref().is_some_and(|work| {
                        work.record.as_ref().and_then(|record| record.risk) == Some(risk)
                    })
                })
                .count();
            (count > 0).then_some(WorkRiskCount { risk, count })
        })
        .collect();
        let unknown_git = rows
            .iter()
            .filter(|row| row.git.tracked_changes.is_none() || row.git.untracked_files.is_none())
            .count();
        let dirty_worktrees = rows
            .iter()
            .filter(|row| {
                row.git.tracked_changes.unwrap_or_default() > 0
                    || row.git.untracked_files.unwrap_or_default() > 0
            })
            .count();
        let blocked_notes = rows
            .iter()
            .filter_map(|row| {
                row.work
                    .as_ref()
                    .and_then(|work| work.record.as_ref())
                    .and_then(|record| record.blocked_on.as_deref())
                    .map(|note| format!("{}: {}", row.slug, note))
            })
            .collect();
        SectionRollup {
            states,
            risks,
            blocked_notes,
            stale_statuses: rows
                .iter()
                .filter(|row| {
                    row.work
                        .as_ref()
                        .is_some_and(|work| work.stale == Some(true))
                })
                .count(),
            owed: rows
                .iter()
                .filter(|row| {
                    matches!(
                        row.owed_phase,
                        Some(wt_core::OwedPhase::Due | wt_core::OwedPhase::Waiting)
                    )
                })
                .count(),
            owed_due: rows
                .iter()
                .filter(|row| row.owed_phase == Some(wt_core::OwedPhase::Due))
                .count(),
            owed_overdue: rows
                .iter()
                .filter(|row| row.owes.iter().any(|item| item.overdue))
                .count(),
            dirty_worktrees: Some(dirty_worktrees),
            unknown_git,
            upstream_ahead: rows
                .iter()
                .filter(|row| row.git.ahead.is_some_and(|count| count > 0))
                .count(),
            upstream_behind: rows
                .iter()
                .filter(|row| row.git.behind.is_some_and(|count| count > 0))
                .count(),
            rebasing: rows.iter().filter(|row| row.git.rebasing).count(),
            conflicted: rows
                .iter()
                .filter(|row| !row.git.conflict_files.is_empty())
                .count(),
            open_prs: rows
                .iter()
                .filter(|row| row.pr.as_ref().and_then(|pr| pr.state.as_deref()) == Some("OPEN"))
                .count(),
            draft_prs: rows
                .iter()
                .filter(|row| {
                    row.pr
                        .as_ref()
                        .is_some_and(|pr| pr.state.as_deref() == Some("OPEN") && pr.draft)
                })
                .count(),
            queued_prs: rows
                .iter()
                .filter(|row| row.pr.as_ref().is_some_and(|pr| pr.merge_queue.is_some()))
                .count(),
            failing_checks: rows
                .iter()
                .filter(|row| row.pr.as_ref().map(|pr| pr.checks) == Some(CheckState::Fail))
                .count(),
            paused_automations: rows.iter().filter(|row| row.automations_paused).count(),
            needs_attention: rows.iter().filter(|row| row.needs_attention).count(),
        }
    }
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
    /// Whether the landing is deployed, for each stage with a deploy probe.
    pub deploys: wt_core::StageDeploys,
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

/// The automation engine's queue and recent history, for display only.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AutomationsView {
    /// False when no engine runs here (no rules, or `WT_AUTOMATIONS=off`).
    pub enabled: bool,
    pub running: Vec<AutomationRunView>,
    pub queued: Vec<AutomationQueueView>,
    /// Newest first.
    pub recent: Vec<AutomationRecordView>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AutomationRunView {
    pub rule: String,
    pub target: String,
    pub detail: String,
    /// UTC epoch milliseconds when a session delivery started.
    pub since_ms: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AutomationQueueView {
    pub rule: String,
    pub target: String,
    pub detail: String,
    pub queued_at_ms: i64,
    /// UTC epoch milliseconds when its settle window (the delay) ends.
    pub settle_at_ms: i64,
    pub waiting_for: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AutomationRecordView {
    pub rule: String,
    pub target: String,
    /// delivered, skipped, cancelled, ambiguous or dispatched.
    pub state: String,
    pub at_ms: i64,
    pub reason: Option<String>,
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
/// The first four are titles set deliberately and stored (see
/// `wt_store::TitleSource`); the rest are display fallbacks for a worktree
/// nobody titled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    #[serde(alias = "manual")]
    User,
    Agent,
    Issue,
    Fallback,
    Pr,
    Commit,
    /// Also any source an older build sent.
    #[default]
    #[serde(other)]
    Slug,
}

impl TitleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Issue => "issue",
            Self::Fallback => "fallback",
            Self::Pr => "pr",
            Self::Commit => "commit",
            Self::Slug => "slug",
        }
    }

    /// Whether the title was set deliberately rather than read from the PR,
    /// a commit, or the slug.
    pub fn is_stored(self) -> bool {
        matches!(
            self,
            Self::User | Self::Agent | Self::Issue | Self::Fallback
        )
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
    /// UTC epoch milliseconds of the reading, when an outside fetch supplies
    /// it and may silently stop.
    pub observed_at_ms: Option<u64>,
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
    /// The persisted primary harness, for menus that preselect it.
    pub primary: Option<wt_core::HarnessId>,
    /// Harnesses not hidden by `[harness].hidden`, in `HarnessId::ALL` order.
    pub harnesses: Vec<wt_core::HarnessId>,
    /// The configured trunk branch (`[branch].base`).
    pub trunk: String,
    /// The reviewer `E` requests when human reviewer requests are configured.
    pub default_reviewer: Option<String>,
    /// `[ui] owed_sections`: landed rows are grouped by what they owe.
    pub owed_sections: bool,
    /// `[guides] enabled`: the details pane shows each row's change guide.
    pub guides: bool,
    /// Staging has a deploy probe (`[environments.staging]`): reaching it
    /// means deployed there, not only merged.
    pub staging_probe: bool,
    /// The same for `[environments.production]`.
    pub production_probe: bool,
}

/// A recorded change guide. `current` compares the HEAD it described with
/// the row's HEAD; `None` when either is unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GuideView {
    pub url: String,
    pub current: Option<bool>,
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
pub(crate) const REMOVING_SECTION: &str = "\0removing";

/// An accepted removal, read from durable state on the owning host.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemovalView {
    pub error: Option<String>,
    /// The accepted job this record belongs to.
    #[serde(default)]
    pub job_id: String,
    /// No inventory checkout matched the record, so there is nothing left to
    /// retry. A failed record in this state can only be dismissed.
    #[serde(default)]
    pub checkout_absent: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BoardRow {
    #[serde(default)]
    pub removal: Option<RemovalView>,
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
    /// The named section the row is filed in, whatever it is drawn under:
    /// a landed row under `[ui] owed_sections` is drawn in a derived block
    /// and stays filed where it was. `None` for the Inbox, and for a row
    /// whose placement its host has not prepared here.
    #[serde(default)]
    pub filed_in: Option<String>,
    /// The repository's main checkout.
    #[serde(default)]
    pub is_main: bool,
    /// The checkout has a detached HEAD.
    #[serde(default)]
    pub detached: bool,
    pub path: String,
    pub badge: String,
    pub details: Vec<String>,
    pub needs_attention: bool,
    #[serde(default)]
    pub work_rank: u8,
    /// Stored position of a row created from the board.
    #[serde(default)]
    pub creation_placement: Option<crate::CreatePlacement>,
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
    /// The dependency install started by creation is still running. Unlike
    /// `busy` it holds no worktree lock and blocks no action.
    #[serde(default)]
    pub installing: bool,
    /// The checkout directory no longer exists.
    #[serde(default)]
    pub path_missing: bool,
    /// The change guide the worktree's agent recorded with `wt guide`.
    #[serde(default)]
    pub guide: Option<GuideView>,
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
    /// What the worktree waits for, as its host stores it.
    #[serde(default)]
    pub watches: Vec<WatchView>,
    /// Everything the worktree owes, most urgent first: what is due, the
    /// merge gate, then what waits. Watches that only tell the person are
    /// not owed and stay in `watches`.
    #[serde(default)]
    pub owes: Vec<OwedView>,
    /// Where a landed row stands with what it owes; `None` until its own
    /// work has landed.
    #[serde(default)]
    pub owed_phase: Option<wt_core::OwedPhase>,
    /// The stored creation stamp of this worktree. A later worktree with the
    /// same slug has another, so a menu commit carries it as its witness.
    #[serde(default)]
    pub created_at: Option<String>,
}

#[cfg(test)]
mod rollup_tests {
    use wt_core::{WorkRisk, WorkState, WorkStatusRecord};

    use super::*;

    #[test]
    fn folded_rollups_include_state_risk_mechanical_counts_and_blocker_notes() {
        let board = Board {
            rows: vec![
                BoardRow {
                    slug: "ready-row".into(),
                    needs_attention: true,
                    work: Some(WorkPresentation {
                        effective_state: Some(WorkState::Ready),
                        stale: Some(true),
                        record: Some(WorkStatusRecord {
                            state: WorkState::Ready,
                            risk: Some(WorkRisk::High),
                            blocked_on: Some("release approval".into()),
                            at: "2026-10-09T00:00:00Z".into(),
                            ..WorkStatusRecord::new(WorkState::Ready, "2026-10-09T00:00:00Z")
                        }),
                        ..Default::default()
                    }),
                    git: GitPresentation {
                        tracked_changes: Some(1),
                        untracked_files: Some(0),
                        ahead: Some(2),
                        behind: Some(1),
                        rebasing: true,
                        conflict_files: vec!["Cargo.toml".into()],
                        ..Default::default()
                    },
                    pr: Some(PrPresentation {
                        state: Some("OPEN".into()),
                        draft: true,
                        checks: CheckState::Fail,
                        merge_queue: Some(MergeQueueView {
                            position: 4,
                            state: "AWAITING_CHECKS".into(),
                        }),
                        ..Default::default()
                    }),
                    automations_paused: true,
                    owed_phase: Some(wt_core::OwedPhase::Due),
                    owes: vec![{
                        let mut check = OwedView::new(
                            wt_core::OwedKind::Check,
                            wt_core::OwedState::Due,
                            "check prod",
                        );
                        check.overdue = true;
                        check
                    }],
                    ..Default::default()
                },
                BoardRow {
                    slug: "unknown-row".into(),
                    ..Default::default()
                },
            ],
            sections: vec![BoardSection {
                rows: vec![0, 1],
                ..Default::default()
            }],
            ..Default::default()
        };
        let summary = SectionRollup::of(&board.rows.iter().collect::<Vec<_>>());
        assert_eq!(
            summary
                .states
                .iter()
                .find(|entry| entry.state == Some(WorkState::Ready))
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            summary
                .states
                .iter()
                .find(|entry| entry.state.is_none())
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            summary.risks,
            [WorkRiskCount {
                risk: WorkRisk::High,
                count: 1
            }]
        );
        assert_eq!(summary.blocked_notes, ["ready-row: release approval"]);
        assert_eq!(summary.stale_statuses, 1);
        assert_eq!(
            (summary.owed, summary.owed_due, summary.owed_overdue),
            (1, 1, 1)
        );
        assert_eq!(summary.dirty_worktrees, Some(1));
        assert_eq!(summary.unknown_git, 1);
        assert_eq!(summary.upstream_ahead, 1);
        assert_eq!(summary.upstream_behind, 1);
        assert_eq!(summary.rebasing, 1);
        assert_eq!(summary.conflicted, 1);
        assert_eq!(summary.open_prs, 1);
        assert_eq!(summary.draft_prs, 1);
        assert_eq!(summary.queued_prs, 1);
        assert_eq!(summary.failing_checks, 1);
        assert_eq!(summary.paused_automations, 1);
        assert_eq!(summary.needs_attention, 1);
    }
}
