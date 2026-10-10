//! AI coding harness session discovery and adapters.
//!
//! Paths and process execution are explicit so callers can isolate test homes,
//! caches, and tmux servers. This crate does not read wt's global configuration.

mod claude;
mod codex;
mod opencode;
mod output;
mod persist;
mod service;
mod status;
mod types;

pub use claude::{
    ActivityDelta, ActivityKind, ActivityLine, ActivityPatch, ClaudeEventParser, ClaudeHarness,
    ClaudeHarnessError, ClaudeInjectFailureKind, ClaudeInjectOutcome, ClaudeInjector,
    ClaudeMessageOutcome, ClaudeMessageTransport, ClaudeMessenger, ClaudePaths,
    ClaudeSelftestOutcome, ClaudeSessionInfo, ClaudeSessionManager, ClaudeSessionManagerError,
    ClaudeSessionPickerEntry, ClaudeSessionTarget, ClaudeStatus, ClaudeUsage, InspectorClient,
    InspectorError, LastEntryKind, RegistrySession, RegistryStatus, SessionTail,
    TranscriptFollower, TrustError, UsagePeriod, add_claude_name, build_claude_session_entries,
    claude_session_id, claude_tmux_name, ensure_inspect_shims, ensure_trusted_paths,
    ensure_trusted_paths_retry, inspector_socket_path, list_claude_names, next_auto_name,
    parse_claude_tmux_name, parse_claude_usage, project_dir, read_claude_usage,
    read_session_summaries, reap_claude_names, remove_claude_name, session_jsonl_path,
    sibling_paths_to_repair, stale_harness_shims, usage_window_key, validate_session_name,
};
pub use codex::{
    CodexActivityBatch, CodexActivityTracker, CodexAppServerError, CodexAppServerFailureKind,
    CodexAppServerInfo, CodexEvent, CodexEventLevel, CodexHarness, CodexHarnessError,
    CodexMessageOutcome, CodexMessageTarget, CodexMessenger, CodexOutputTracker, CodexPaths,
    CodexQueueDelivery, CodexQueueSubmission, CodexTail, CodexThreadStatus, CodexUsage,
    PendingInteraction, read_codex_tail, read_codex_usage,
};
pub use opencode::{
    OpenCodeActivityBatch, OpenCodeActivityTracker, OpenCodeCost, OpenCodeError, OpenCodeEvent,
    OpenCodeEventLevel, OpenCodeHarness, OpenCodeOutputTracker, OpenCodePaths, OpenCodeSendOutcome,
    opencode_display_title,
};
pub use output::{HarnessOutputKind, HarnessOutputLine, HarnessOutputTarget, HarnessOutputUpdate};
pub use service::{HarnessMessageOutcome, HarnessService, HarnessServiceError, HarnessTarget};
pub use status::{DerivedState, derive_session_state, registry_status_to_state};
pub use types::{
    DiscoveryRequest, HarnessExtras, HarnessSession, HarnessSpawnRequest, SessionSummary,
    SessionSummarySource, SpawnCommand,
};
pub use wt_core::HarnessId;
