mod activity;
mod identity;
mod injection;
mod inspector;
mod lifecycle;
mod messaging;
mod names;
mod registry;
mod sessions;
mod shims;
mod summaries;
mod transcript;
mod trust;
mod usage;

pub use activity::{ActivityDelta, ActivityKind, ActivityLine, ActivityPatch, ClaudeEventParser};
pub use identity::{claude_session_id, project_dir, session_jsonl_path};
pub use injection::{
    ClaudeInjectFailureKind, ClaudeInjectOutcome, ClaudeInjector, ClaudeSelftestOutcome,
};
pub use inspector::{InspectorClient, InspectorError, inspector_socket_path};
pub use lifecycle::{
    ClaudeSessionInfo, ClaudeSessionManager, ClaudeSessionManagerError, ClaudeSessionTarget,
};
pub use messaging::{ClaudeMessageOutcome, ClaudeMessageTransport, ClaudeMessenger};
pub use names::{
    ClaudeSessionPickerEntry, add_claude_name, build_claude_session_entries, list_claude_names,
    next_auto_name, reap_claude_names, remove_claude_name, validate_session_name,
};
pub use registry::{RegistrySession, RegistryStatus};
pub use sessions::{ClaudeHarness, ClaudeHarnessError, ClaudePaths};
pub use shims::{ensure_inspect_shims, stale_harness_shims};
pub use summaries::read_session_summaries;
pub use transcript::{ClaudeStatus, LastEntryKind, SessionTail, TranscriptFollower};
pub use trust::{
    TrustError, ensure_trusted_paths, ensure_trusted_paths_retry, sibling_paths_to_repair,
};
pub use usage::{
    ClaudeUsage, UsagePeriod, parse_claude_usage, read_claude_usage, usage_window_key,
};

pub fn claude_tmux_name(slug: &str, managed_name: Option<&str>) -> String {
    match managed_name {
        Some(name) => format!("{slug}~{name}"),
        None => slug.to_owned(),
    }
}

pub fn parse_claude_tmux_name(name: &str, slug: &str) -> Option<Option<String>> {
    if name == slug {
        return Some(None);
    }
    name.strip_prefix(&format!("{slug}~"))
        .map(|managed| Some(managed.to_owned()))
}
