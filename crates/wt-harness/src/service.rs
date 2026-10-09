use std::path::PathBuf;

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_platform::process::ProcessRunner;
use wt_tmux::TmuxClient;

use crate::{
    ClaudeHarness, ClaudeHarnessError, ClaudeInjector, ClaudeMessageOutcome, ClaudeMessenger,
    ClaudePaths, ClaudeSessionManager, ClaudeSessionManagerError, ClaudeSessionTarget,
    CodexHarness, CodexHarnessError, CodexMessageOutcome, CodexMessageTarget, CodexPaths,
    DiscoveryRequest, HarnessId, HarnessSession, HarnessSpawnRequest, OpenCodeError,
    OpenCodeHarness, OpenCodePaths, OpenCodeSendOutcome, SpawnCommand,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessTarget {
    pub id: HarnessId,
    pub slug: String,
    pub cwd: PathBuf,
    pub managed_name: Option<String>,
    pub sender: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HarnessMessageOutcome {
    Claude(ClaudeMessageOutcome),
    Codex(CodexMessageOutcome),
    OpenCode(OpenCodeSendOutcome),
}

#[derive(Debug, Error)]
pub enum HarnessServiceError {
    #[error("{operation} cancelled")]
    Cancelled { operation: &'static str },
    #[error("message is empty")]
    EmptyMessage,
    #[error(transparent)]
    ClaudeDiscovery(#[from] ClaudeHarnessError),
    #[error(transparent)]
    Claude(#[from] ClaudeSessionManagerError),
    #[error(transparent)]
    Codex(#[from] CodexHarnessError),
    #[error(transparent)]
    OpenCode(#[from] OpenCodeError),
}

/// Shared adapter surface for callers that select a harness dynamically.
/// SQLite and JSONL discovery remain synchronous inside each adapter and the
/// async methods here use their own bounded blocking workers where needed.
#[derive(Clone)]
pub struct HarnessService {
    claude: ClaudeHarness,
    claude_sessions: ClaudeSessionManager,
    claude_messenger: ClaudeMessenger,
    codex: CodexHarness,
    opencode: OpenCodeHarness,
}

impl HarnessService {
    pub fn new(
        claude_paths: ClaudePaths,
        codex_paths: CodexPaths,
        opencode_paths: OpenCodePaths,
        runner: ProcessRunner,
        tmux: TmuxClient,
    ) -> Self {
        let claude = ClaudeHarness::new(claude_paths);
        let claude_sessions = ClaudeSessionManager::new(claude.clone(), tmux.clone());
        let claude_messenger = ClaudeMessenger::new(
            claude_sessions.clone(),
            ClaudeInjector::new(claude.paths().cache_dir.clone()),
        );
        Self {
            claude,
            claude_sessions,
            claude_messenger,
            codex: CodexHarness::new(codex_paths, runner.clone(), tmux.clone()),
            opencode: OpenCodeHarness::new(opencode_paths, runner, tmux),
        }
    }

    pub fn build_spawn_command(
        &self,
        id: HarnessId,
        request: &HarnessSpawnRequest,
    ) -> SpawnCommand {
        match id {
            HarnessId::Claude => self.claude.build_spawn_command(request),
            HarnessId::Codex => self.codex.build_spawn_command(request),
            HarnessId::Opencode => self.opencode.build_spawn_command(request),
        }
    }

    pub fn claude_session_id(&self, request: &HarnessSpawnRequest) -> String {
        self.claude
            .session_id(&request.worktree_path, request.managed_name.as_deref())
    }

    pub fn claude_tmux_session_name(&self, request: &HarnessSpawnRequest) -> String {
        self.claude
            .tmux_session_name(&request.slug, request.managed_name.as_deref())
    }

    pub async fn codex_app_server_info(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<crate::CodexAppServerInfo>, HarnessServiceError> {
        Ok(self.codex.app_server_info(cancel).await?)
    }

    pub async fn discover(
        &self,
        id: HarnessId,
        request: &DiscoveryRequest,
        live_tmux_names: &[String],
        cancel: &CancellationToken,
    ) -> Result<Vec<HarnessSession>, HarnessServiceError> {
        match id {
            HarnessId::Claude => {
                let harness = self.claude.clone();
                let request = request.clone();
                let live = live_tmux_names.to_vec();
                let task = tokio::task::spawn_blocking(move || harness.discover(&request, &live));
                tokio::select! {
                    _ = cancel.cancelled() => Err(HarnessServiceError::Cancelled { operation: "Claude discovery" }),
                    result = task => Ok(result.map_err(|error| HarnessServiceError::ClaudeDiscovery(
                        ClaudeHarnessError::Io(std::io::Error::other(error.to_string()))
                    ))??),
                }
            }
            HarnessId::Codex => Ok(self
                .codex
                .discover(request, request.live_session_id.as_deref(), cancel)
                .await?),
            HarnessId::Opencode => Ok(self.opencode.discover(request, cancel).await?),
        }
    }

    /// Ensure the selected harness has a live slot. Returns true only when
    /// this call created it, false when an existing slot was adopted.
    pub async fn ensure_started(
        &self,
        id: HarnessId,
        request: &HarnessSpawnRequest,
        cancel: &CancellationToken,
    ) -> Result<bool, HarnessServiceError> {
        match id {
            HarnessId::Claude => {
                let target = ClaudeSessionTarget {
                    slug: request.slug.clone(),
                    cwd: request.worktree_path.clone(),
                    managed_name: request.managed_name.clone(),
                };
                let (_, started) = self.claude_sessions.ensure(&target, cancel).await?;
                Ok(started)
            }
            HarnessId::Codex => Ok(self.codex.start(request, cancel).await?),
            HarnessId::Opencode => Ok(self.opencode.start(request, cancel).await?),
        }
    }

    pub async fn send(
        &self,
        target: &HarnessTarget,
        cancel: &CancellationToken,
    ) -> Result<HarnessMessageOutcome, HarnessServiceError> {
        if target.text.trim().is_empty() {
            return Err(HarnessServiceError::EmptyMessage);
        }
        let text = stamp_sender(&target.text, target.sender.as_deref());
        match target.id {
            HarnessId::Claude => Ok(HarnessMessageOutcome::Claude(
                self.claude_messenger
                    .send_with_sender(
                        &ClaudeSessionTarget {
                            slug: target.slug.clone(),
                            cwd: target.cwd.clone(),
                            managed_name: target.managed_name.clone(),
                        },
                        &target.text,
                        target.sender.as_deref(),
                        cancel,
                    )
                    .await?,
            )),
            HarnessId::Codex => Ok(HarnessMessageOutcome::Codex(
                self.codex
                    .send_message(
                        &CodexMessageTarget {
                            slug: target.slug.clone(),
                            cwd: target.cwd.clone(),
                            managed_name: target.managed_name.clone(),
                            text: text.clone(),
                        },
                        cancel,
                    )
                    .await?,
            )),
            HarnessId::Opencode => Ok(HarnessMessageOutcome::OpenCode(
                self.opencode
                    .send_message(&target.slug, &target.cwd, &text, cancel)
                    .await?,
            )),
        }
    }
}

fn stamp_sender(text: &str, sender: Option<&str>) -> String {
    let Some(agent) = sender.map(str::trim).filter(|sender| !sender.is_empty()) else {
        return text.to_owned();
    };
    if text.trim_start().starts_with('[') || is_harness_command(text) {
        return text.to_owned();
    }
    format!("[{agent}] {text}")
}

fn is_harness_command(text: &str) -> bool {
    let first = text.split_whitespace().next().unwrap_or_default();
    let Some(command) = first.strip_prefix('/').or_else(|| first.strip_prefix('$')) else {
        return false;
    };
    !command.is_empty()
        && command
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase())
        && command
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_tagging_preserves_commands_and_existing_attribution() {
        assert!(is_harness_command("  /compact"));
        assert!(is_harness_command("$start now"));
        assert!(!is_harness_command("/Users/mike/file"));
        assert!(!is_harness_command("/$path"));
        assert_eq!(
            stamp_sender("[re: branch] hello", Some("agent")),
            "[re: branch] hello"
        );
        assert_eq!(stamp_sender("hello", None), "hello");
        assert_eq!(stamp_sender("hello", Some("agent")), "[agent] hello");
    }
}
