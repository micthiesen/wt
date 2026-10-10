use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    #[default]
    GitWorktree,
    Rift,
}

pub use wt_core::HarnessId;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceRole {
    #[default]
    Controller,
    Worker,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InstanceConfig {
    pub role: InstanceRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueStatusIcon {
    Circle,
    Backlog,
    Progress,
    Review,
    Completed,
    Cancelled,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueStatusStyle {
    pub icon: IssueStatusIcon,
    pub color: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SstConfig {
    pub state_bucket: String,
    pub state_prefix: String,
    pub aws_profile: String,
    pub auto_regen_paths: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IssueTrackerConfig {
    pub status_styles: BTreeMap<String, IssueStatusStyle>,
    pub status_command: Option<Vec<String>>,
    pub read_command: Option<Vec<String>>,
    pub url_template: Option<String>,
    pub prefix: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DevServerConfig {
    pub command: String,
    pub port_base: u16,
    pub port_range: u32,
    pub url: String,
    pub max_concurrent: Option<u32>,
    pub stop_command: Option<String>,
    pub reset_command: Option<String>,
    pub health_command: Option<String>,
}

impl Default for DevServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            port_base: 8100,
            port_range: 100,
            url: "http://localhost:{{port}}/".into(),
            max_concurrent: None,
            stop_command: None,
            reset_command: None,
            health_command: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewBotMode {
    #[default]
    Threads,
    Checklist,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewBotConfig {
    pub name: String,
    pub login: String,
    pub check_contexts: Vec<String>,
    pub unresolved_via: ReviewBotMode,
    /// Input accepts either a string or a string array; normalized to a list.
    pub summary_marker: Vec<String>,
    pub pending_marker: Option<String>,
    pub rerun_command: Option<String>,
}

impl Default for ReviewBotConfig {
    fn default() -> Self {
        Self {
            name: "CodeRabbit".into(),
            login: "coderabbitai".into(),
            check_contexts: vec!["CodeRabbit".into()],
            unresolved_via: ReviewBotMode::Threads,
            summary_marker: Vec::new(),
            pending_marker: None,
            rerun_command: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    pub host: String,
    pub label: String,
    pub wt_path: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubEventsConfig {
    pub port: u16,
    pub host: String,
    pub secret: Option<String>,
    pub secret_file: Option<String>,
    pub backstop_poll_ms: f64,
}

impl Default for GithubEventsConfig {
    fn default() -> Self {
        Self {
            port: 8765,
            host: "127.0.0.1".into(),
            secret: None,
            secret_file: None,
            backstop_poll_ms: 600_000.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PullRequestTarget {
    #[default]
    Github,
    Linear,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubConfig {
    pub reviewers: bool,
    pub ignored_review_repositories: Vec<String>,
    pub ignored_checks: Vec<String>,
    pub default_reviewer: Option<String>,
    pub pr_target: PullRequestTarget,
    pub events: Option<GithubEventsConfig>,
}

impl GithubConfig {
    pub fn default_with_reviewers() -> Self {
        Self {
            reviewers: true,
            ..Self::default()
        }
    }
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            reviewers: true,
            ignored_review_repositories: Vec::new(),
            ignored_checks: Vec::new(),
            default_reviewer: None,
            pr_target: PullRequestTarget::Github,
            events: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffConfig {
    pub command: String,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            command: "revdiff --vim-motion --compact {{base}}".into(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    pub command: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserConfig {
    pub chrome_profile: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamingReasoningEffort {
    Minimal,
    #[default]
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamingHarness {
    #[default]
    Primary,
    Claude,
    Codex,
    Opencode,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NamingConfig {
    pub auto_rename: bool,
    pub harness: NamingHarness,
    pub models: BTreeMap<String, String>,
    pub reasoning_effort: NamingReasoningEffort,
    pub max_input_tokens: f64,
    pub timeout_ms: f64,
}

impl Default for NamingConfig {
    fn default() -> Self {
        Self {
            auto_rename: true,
            harness: NamingHarness::Primary,
            models: BTreeMap::new(),
            reasoning_effort: NamingReasoningEffort::Low,
            max_input_tokens: 8000.0,
            timeout_ms: 120_000.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffectTag {
    Git,
    Github,
    Dev,
    Issue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequireTag {
    Pr,
    #[serde(rename = "pr.ready")]
    PrReady,
    Deployed,
    #[serde(rename = "issue.tracker")]
    IssueTracker,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionTarget {
    #[default]
    Headless,
    Session,
    Manager,
    Slot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Claude,
    Shell,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionArgPrompt {
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionDef {
    pub kind: ActionKind,
    pub id: String,
    pub name: String,
    pub prompt: Option<String>,
    pub shell: Option<String>,
    pub target: ActionTarget,
    pub affects: Option<Vec<EffectTag>>,
    pub requires: Vec<RequireTag>,
    pub issue_status: Option<String>,
    pub key: Option<String>,
    pub group: Option<String>,
    pub external: bool,
    pub arg_prompt: Option<ActionArgPrompt>,
    pub label_extract: Option<String>,
}

impl Default for ActionDef {
    fn default() -> Self {
        Self {
            kind: ActionKind::Claude,
            id: String::new(),
            name: String::new(),
            prompt: None,
            shell: None,
            target: ActionTarget::Headless,
            affects: None,
            requires: Vec::new(),
            issue_status: None,
            key: None,
            group: None,
            external: false,
            arg_prompt: None,
            label_extract: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AutomationTrigger {
    #[serde(rename = "pr.checks.failed")]
    PrChecksFailed,
    #[serde(rename = "review_bot.unresolved", alias = "rabbit.unresolved")]
    ReviewBotUnresolved,
    #[serde(rename = "review.changes_requested")]
    ReviewChangesRequested,
    #[serde(rename = "pr.conflict")]
    PrConflict,
    #[serde(rename = "wt.merged")]
    WtMerged,
    #[serde(rename = "wt.created")]
    WtCreated,
    #[serde(rename = "stack.parent_merged")]
    StackParentMerged,
    #[serde(rename = "status.needs_human", alias = "status.needs-human")]
    StatusNeedsHuman,
    #[serde(rename = "status.needs_testing", alias = "status.needs-testing")]
    StatusNeedsTesting,
    #[serde(rename = "status.ready")]
    StatusReady,
    #[serde(rename = "status.verification_overdue")]
    StatusVerificationOverdue,
    #[serde(rename = "branch.advanced")]
    BranchAdvanced,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutomationBusyPolicy {
    #[default]
    Queue,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutomationDef {
    pub id: String,
    pub on: AutomationTrigger,
    pub run: String,
    pub busy: AutomationBusyPolicy,
    pub cooldown_minutes: Option<f64>,
    pub after_days: f64,
    pub settle_seconds: f64,
    pub branch: Option<String>,
}

impl Default for AutomationDef {
    fn default() -> Self {
        Self {
            id: String::new(),
            on: AutomationTrigger::PrChecksFailed,
            run: String::new(),
            busy: AutomationBusyPolicy::Queue,
            cooldown_minutes: None,
            after_days: 2.0,
            settle_seconds: 120.0,
            branch: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    pub main_clone: PathBuf,
    pub worktree_root: PathBuf,
    pub log_dir: PathBuf,
    pub app_log_dir: PathBuf,
    pub lock_dir: PathBuf,
    pub cache_db: PathBuf,
    pub state_db: PathBuf,
    pub cache_root: PathBuf,
    pub wezterm_cli: Option<PathBuf>,
    pub dotfiles: PathBuf,
    /// Optional source checkout behind the developer's `wt` session slot.
    pub wt_source: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BranchConfig {
    pub prefix: String,
    pub base: String,
    pub id_pattern: String,
    pub slug_max_len: f64,
    pub keep_fresh: Vec<String>,
    pub production: Option<String>,
}

impl Default for BranchConfig {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            base: "main".into(),
            id_pattern: r"^[a-z]+-(\d+)(?:-|$)".into(),
            slug_max_len: 50.0,
            keep_fresh: Vec::new(),
            production: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StageConfig {
    pub prefix: String,
    pub default_personal: String,
    pub domain: Option<String>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LifecycleConfig {
    pub env_files_to_copy: Vec<String>,
    pub copy_globs: Vec<String>,
    pub install_command: Option<String>,
    pub destroy_command: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TmuxConfig {
    pub socket: String,
    pub terminal_config: Option<String>,
}
impl Default for TmuxConfig {
    fn default() -> Self {
        Self {
            socket: "wt".into(),
            terminal_config: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessConfig {
    pub primary: HarnessId,
    pub hidden: BTreeSet<HarnessId>,
}
impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            primary: HarnessId::Claude,
            hidden: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiSort {
    #[default]
    Status,
    Manual,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPane {
    #[default]
    Column,
    FullWidth,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub hide_terminal_apps: Vec<String>,
    pub action_groups_last: Vec<String>,
    pub rows: Vec<String>,
    pub hidden_badges: BTreeSet<String>,
    pub sort: UiSort,
    pub activity_pane: ActivityPane,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SkillsConfig {
    pub startup_check: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManagerConfig {
    pub wt_feedback: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateConfig {
    pub startup_check: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendConfig {
    pub kind: BackendKind,
}

/// Fully resolved, validated configuration. Optional integrations remain
/// `None` when their section is absent; generic settings always have a value.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Selected repository `.wt.toml`, when one was found.
    pub repository_config: Option<PathBuf>,
    pub instance: InstanceConfig,
    pub repo_id: String,
    pub repo_path: PathBuf,
    pub paths: PathsConfig,
    pub tmux: TmuxConfig,
    pub branch: BranchConfig,
    pub stage: StageConfig,
    pub lifecycle: LifecycleConfig,
    pub backend: BackendConfig,
    pub sst: Option<SstConfig>,
    pub issue_tracker: Option<IssueTrackerConfig>,
    pub review_bot: ReviewBotConfig,
    pub dev_server: Option<DevServerConfig>,
    pub remote: Option<RemoteConfig>,
    pub harness: HarnessConfig,
    pub naming: Option<NamingConfig>,
    pub diff: DiffConfig,
    pub editor: EditorConfig,
    pub browser: BrowserConfig,
    pub github: GithubConfig,
    pub actions: Vec<ActionDef>,
    pub automations: Vec<AutomationDef>,
    pub ui: UiConfig,
    pub skills: SkillsConfig,
    pub manager: ManagerConfig,
    pub update: UpdateConfig,
}
