use std::{path::Path, path::PathBuf, time::Duration};

use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessError, ProcessRunner};
use wt_tmux::{OptionScope, PaneTarget, TmuxClient};

use crate::{DiscoveryRequest, HarnessSpawnRequest, persist::FileLock};

use super::{
    CodexAppServerError, CodexAppServerFailureKind, CodexHarness, CodexHarnessError, CodexPaths,
    CodexQueueDelivery, app_server, find_rollout, read_codex_tail,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodexMessageOutcome {
    /// Codex durably owns the message. `reconciled` means wt lost the
    /// queue/add reply and proved ownership through queue/list or thread/items.
    Queued(CodexQueueDelivery),
    CliQueued {
        thread_id: String,
    },
    Terminal {
        cold_started: bool,
        delivered: Option<bool>,
        reason: String,
    },
    /// The daemon/socket is absent or the installed protocol has no queue
    /// method. A higher-level caller may use terminal input after a fresh
    /// identity/readiness check under its shared injection lock.
    NeedsTerminalFallback {
        reason: String,
    },
    /// A write may have reached the server but no durable ownership proof was
    /// available. This is never eligible for terminal fallback or retry.
    Ambiguous {
        reason: String,
    },
    Failed {
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexMessageTarget {
    pub slug: String,
    pub cwd: PathBuf,
    pub managed_name: Option<String>,
    pub text: String,
}

#[derive(Clone)]
pub struct CodexMessenger {
    paths: CodexPaths,
    runner: ProcessRunner,
    tmux: TmuxClient,
}

impl CodexMessenger {
    pub fn new(paths: CodexPaths, runner: ProcessRunner, tmux: TmuxClient) -> Self {
        Self {
            paths,
            runner,
            tmux,
        }
    }

    pub async fn send(
        &mut self,
        thread_id: &str,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<CodexQueueDelivery, CodexAppServerError> {
        if is_codex_command(text) {
            return Err(CodexAppServerError {
                operation: "queue/add",
                kind: CodexAppServerFailureKind::Rejected,
                detail: "slash commands require an interactive target; use send_target".into(),
            });
        }
        app_server::queue_message(&self.paths.app_server_socket(), thread_id, text, cancel).await
    }

    pub async fn send_target(
        &mut self,
        target: &CodexMessageTarget,
        cancel: &CancellationToken,
    ) -> Result<CodexMessageOutcome, CodexHarnessError> {
        if target.text.trim().is_empty() {
            return Ok(CodexMessageOutcome::Failed {
                reason: "message is empty".into(),
            });
        }
        // Normalize at the harness boundary so palettes, CLI sends and
        // configured actions cannot accidentally turn compact into chat.
        let target = CodexMessageTarget {
            text: terminal_text(&target.text).to_owned(),
            ..target.clone()
        };
        if is_codex_command(&target.text) && target.text.chars().any(char::is_control) {
            return Ok(CodexMessageOutcome::Failed {
                reason: "Codex slash commands must be a single line without control characters"
                    .into(),
            });
        }
        let tmux_name = format!("{}-codex", target.slug);
        let _lock = FileLock::acquire_async(
            &self
                .paths
                .lock_dir
                .join(format!("__codex_send__{tmux_name}.lock")),
            cancel,
        )
        .await?;

        let initial_inventory = self.tmux.list_sessions(cancel).await?;
        let was_live = initial_inventory.iter().any(|s| s.name == tmux_name);
        let stamped_id = initial_inventory
            .iter()
            .find(|s| s.name == tmux_name)
            .and_then(|s| s.harness_session_id.clone());
        let harness = CodexHarness::new(self.paths.clone(), self.runner.clone(), self.tmux.clone());
        let request = DiscoveryRequest {
            slug: target.slug.clone(),
            worktree_path: target.cwd.clone(),
            live_session_id: None,
        };
        let discovered = harness.discover_sync(&request, stamped_id.as_deref())?;

        let mut session_id = if was_live {
            stamped_id
        } else {
            target
                .managed_name
                .as_deref()
                .and_then(|name| {
                    discovered
                        .iter()
                        .find(|s| s.extras.managed_name.as_deref() == Some(name))
                })
                .or_else(|| {
                    discovered
                        .iter()
                        .find(|s| s.extras.managed_name.as_deref() == Some("primary"))
                })
                .map(|s| s.session_id.clone())
        };
        if was_live && session_id.is_none() {
            session_id = self
                .recover_live_identity(&tmux_name, &discovered, cancel)
                .await?;
            if let Some(id) = session_id.as_deref() {
                let current = self.tmux.list_sessions(cancel).await?;
                let slot = initial_inventory.iter().find(|slot| slot.name == tmux_name);
                let same_slot = slot.filter(|slot| {
                    current.iter().any(|now| {
                        now.id == slot.id
                            && now.created_at == slot.created_at
                            && now.name == slot.name
                    })
                });
                if let Some(slot) = same_slot {
                    self.tmux
                        .set_option(
                            &OptionScope::Session(slot.id.clone()),
                            "@wt-harness-session-id",
                            Some(id),
                            cancel,
                        )
                        .await?;
                } else {
                    return Ok(CodexMessageOutcome::Failed {
                        reason: "Codex slot changed while recovering its conversation identity"
                            .into(),
                    });
                }
            }
        }

        let cold_started = if !was_live {
            let spawn = HarnessSpawnRequest {
                worktree_path: target.cwd.clone(),
                slug: target.slug.clone(),
                managed_name: None,
                resume_session_id: session_id.clone(),
                display_label: None,
            };
            harness.start(&spawn, cancel).await?
        } else {
            false
        };

        if let Some(id) = session_id
            .as_deref()
            .filter(|_| !is_codex_command(&target.text))
        {
            let native = self.send(id, &target.text, cancel).await;
            match native {
                Ok(delivery) => return Ok(CodexMessageOutcome::Queued(delivery)),
                Err(error)
                    if matches!(
                        error.kind,
                        CodexAppServerFailureKind::Ambiguous | CodexAppServerFailureKind::Cancelled
                    ) =>
                {
                    return Ok(CodexMessageOutcome::Ambiguous {
                        reason: format!("{}; wt did not retry or type the message", error),
                    });
                }
                Err(error)
                    if matches!(
                        error.kind,
                        CodexAppServerFailureKind::Absent
                            | CodexAppServerFailureKind::Unavailable
                            | CodexAppServerFailureKind::Unsupported
                    ) =>
                {
                    match self.cli_queue(id, &target.text, cancel).await {
                        CliQueueResult::Accepted => {
                            return Ok(CodexMessageOutcome::CliQueued {
                                thread_id: id.to_owned(),
                            });
                        }
                        CliQueueResult::Ambiguous(reason) => {
                            return Ok(CodexMessageOutcome::Ambiguous { reason });
                        }
                        CliQueueResult::Unsupported => {
                            return self
                                .terminal_fallback(
                                    &tmux_name,
                                    &target.cwd,
                                    &target.slug,
                                    Some(id),
                                    &target.text,
                                    cold_started,
                                    "Codex queue API is unavailable",
                                    cancel,
                                )
                                .await;
                        }
                    }
                }
                Err(error) => {
                    return Ok(CodexMessageOutcome::Failed {
                        reason: error.to_string(),
                    });
                }
            }
        }

        // A new Codex TUI has no thread UUID until it writes its first
        // session_meta. Terminal input is limited to an observed empty prompt.
        self.terminal_fallback(
            &tmux_name,
            &target.cwd,
            &target.slug,
            session_id.as_deref(),
            &target.text,
            cold_started,
            if is_codex_command(&target.text) {
                "Codex slash commands execute through the interactive terminal"
            } else if was_live {
                "live Codex slot has no recoverable thread UUID"
            } else {
                "new Codex thread has no UUID until its first prompt"
            },
            cancel,
        )
        .await
    }

    pub(super) async fn recover_live_identity(
        &self,
        tmux_name: &str,
        sessions: &[crate::HarnessSession],
        cancel: &CancellationToken,
    ) -> Result<Option<String>, CodexHarnessError> {
        if sessions.is_empty() {
            return Ok(None);
        }
        let Some(before) = self
            .tmux
            .list_sessions(cancel)
            .await?
            .into_iter()
            .find(|session| session.name == tmux_name)
        else {
            return Ok(None);
        };
        let panes = self.tmux.list_panes(&before.id, cancel).await?;
        if panes.len() != 1 {
            return Ok(None);
        }
        let Some(pid) = panes[0].pid else {
            return Ok(None);
        };
        let mut command =
            CommandSpec::new("lsof").args(["-nP", "-a", "-p", &pid.to_string(), "-Fn"]);
        command.cwd = Some(self.paths.home.clone());
        command.timeout = Duration::from_secs(2);
        let result = match self.runner.run(command, cancel).await {
            Ok(result) => result,
            Err(error @ ProcessError::Cancelled { .. }) => return Err(error.into()),
            // lsof is optional proof, not a requirement for reconnecting to
            // an explicitly selected physical tmux session.
            Err(_) => return Ok(None),
        };
        if !result.status.success() {
            return Ok(None);
        }
        let found = writer_identity(&result.stdout_text(), &self.paths.codex_home, sessions);
        if found.is_none() {
            return Ok(None);
        }
        let after = self.tmux.list_panes(&before.id, cancel).await?;
        let same_session = self
            .tmux
            .list_sessions(cancel)
            .await?
            .iter()
            .any(|session| {
                session.id == before.id
                    && session.created_at == before.created_at
                    && session.name == before.name
            });
        if same_session
            && after.len() == 1
            && after[0].id == panes[0].id
            && after[0].pid == Some(pid)
            && !after[0].dead
        {
            Ok(found)
        } else {
            Ok(None)
        }
    }

    async fn cli_queue(
        &self,
        thread: &str,
        text: &str,
        cancel: &CancellationToken,
    ) -> CliQueueResult {
        let mut command =
            CommandSpec::new("codex").args(["queue", "--thread", thread, "--message", text]);
        command.cwd = Some(self.paths.home.clone());
        command.timeout = Duration::from_secs(15);
        match self.runner.run(command, cancel).await {
            Ok(output) => {
                let stdout = output.stdout_text();
                let stderr = output.stderr_text();
                if output.status.success() && receipt(&stdout, thread) {
                    CliQueueResult::Accepted
                } else if unsupported(&stderr, &stdout) {
                    CliQueueResult::Unsupported
                } else {
                    CliQueueResult::Ambiguous(format!(
                        "codex queue did not confirm delivery; it may have accepted the message: {}",
                        compact(&stderr, &stdout)
                    ))
                }
            }
            Err(ProcessError::Io {
                operation: "spawn", ..
            }) => CliQueueResult::Unsupported,
            Err(error) => {
                CliQueueResult::Ambiguous(format!("codex queue outcome is unknown: {error}"))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn terminal_fallback(
        &self,
        name: &str,
        cwd: &Path,
        slug: &str,
        session_id: Option<&str>,
        text: &str,
        cold_started: bool,
        reason: &str,
        cancel: &CancellationToken,
    ) -> Result<CodexMessageOutcome, CodexHarnessError> {
        let _inject_lock = FileLock::acquire_async(
            &self.paths.lock_dir.join(format!("__inject__{name}.lock")),
            cancel,
        )
        .await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let target = PaneTarget::active_session_pane(name);
        loop {
            if cancel.is_cancelled() {
                return Ok(CodexMessageOutcome::Failed {
                    reason: "cancelled before terminal submission".into(),
                });
            }
            let inventory = self.tmux.list_sessions(cancel).await?;
            let Some(live) = inventory.iter().find(|s| s.name == name) else {
                return Ok(CodexMessageOutcome::Failed {
                    reason: format!("Codex slot {name} exited before terminal fallback"),
                });
            };
            let ready = if let Some(id) = session_id {
                // Ownership is rechecked under the cross-process injection
                // lock immediately before typing.
                if live.harness_session_id.as_deref() != Some(id) {
                    false
                } else if let Some(file) = find_rollout(&self.paths.sessions_dir(), cwd, slug, id)?
                {
                    let thread_ready = read_codex_tail(&file.path, file.mtime_ms, file.size)?
                        .is_some_and(|tail| {
                            closed_turn(tail.parse_complete, tail.last_task_event.as_deref())
                        });
                    thread_ready
                        && self
                            .tmux
                            .capture_pane(&target, Some(40), cancel)
                            .await
                            .map(|pane| codex_pane_is_idle(&pane))
                            .unwrap_or(false)
                } else {
                    false
                }
            } else {
                self.tmux
                    .capture_pane(&target, Some(40), cancel)
                    .await
                    .map(|pane| codex_pane_is_idle(&pane))
                    .unwrap_or(false)
            };
            if ready {
                let still_live = self
                    .tmux
                    .list_sessions(cancel)
                    .await?
                    .into_iter()
                    .find(|s| s.name == name);
                if let Some(live) = still_live
                    && (session_id.is_none() || live.harness_session_id.as_deref() == session_id)
                {
                    let command = is_codex_command(text);
                    let typed = if command {
                        self.tmux.type_literal(&target, text, cancel).await
                    } else {
                        self.tmux.send_literal(&target, text, cancel).await
                    };
                    if let Err(error) = typed {
                        return Ok(CodexMessageOutcome::Ambiguous {
                            reason: format!("terminal input may have reached Codex: {error}"),
                        });
                    }
                    // Let Codex finish the typed input burst. Commands take
                    // one submit key and never go through bracketed paste.
                    if command {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    let keys: &[&str] = if command {
                        &["Enter"]
                    } else {
                        &["Enter", "Enter"]
                    };
                    if let Err(error) = self.tmux.send_keys(&target, keys, cancel).await {
                        return Ok(CodexMessageOutcome::Ambiguous {
                            reason: format!("Codex input was submitted ambiguously: {error}"),
                        });
                    }
                    return Ok(CodexMessageOutcome::Terminal {
                        cold_started,
                        delivered: None,
                        reason: reason.to_owned(),
                    });
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(CodexMessageOutcome::Failed {
                    reason: format!(
                        "Codex terminal fallback refused because exact-thread readiness did not pass: {reason}"
                    ),
                });
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub async fn send_with_outcome(
        &mut self,
        thread_id: &str,
        text: &str,
        cancel: &CancellationToken,
    ) -> CodexMessageOutcome {
        match self.send(thread_id, text, cancel).await {
            Ok(delivery) => CodexMessageOutcome::Queued(delivery),
            Err(error)
                if error.kind == CodexAppServerFailureKind::Absent
                    || error.kind == CodexAppServerFailureKind::Unavailable
                    || error.kind == CodexAppServerFailureKind::Unsupported =>
            {
                CodexMessageOutcome::NeedsTerminalFallback {
                    reason: error.to_string(),
                }
            }
            Err(error) if error.kind == CodexAppServerFailureKind::Ambiguous => {
                CodexMessageOutcome::Ambiguous {
                    reason: error.to_string(),
                }
            }
            Err(error) => CodexMessageOutcome::Failed {
                reason: error.to_string(),
            },
        }
    }
}

/// Only the pane process's exact writer lock under this Codex home proves
/// ownership. A shared daemon's loaded threads, resume arguments, and rollout
/// recency cannot identify which conversation its TUI currently displays.
fn writer_identity(
    output: &str,
    codex_home: &Path,
    sessions: &[crate::HarnessSession],
) -> Option<String> {
    let locks = codex_home.join("thread-writer-locks");
    let mut found = std::collections::HashSet::new();
    for path in output.lines().filter_map(|line| line.strip_prefix('n')) {
        let path = Path::new(path);
        if path.parent() != Some(locks.as_path()) {
            continue;
        }
        if let Some(id) = path
            .file_name()
            .and_then(|file| file.to_str())
            .and_then(|file| file.strip_suffix(".lock"))
            && sessions.iter().any(|session| session.session_id == id)
        {
            found.insert(id.to_owned());
        }
    }
    (found.len() == 1)
        .then(|| found.into_iter().next())
        .flatten()
}

#[derive(Clone, Debug)]
enum CliQueueResult {
    Accepted,
    Unsupported,
    Ambiguous(String),
}

fn receipt(output: &str, thread: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("queued message") && lower.contains(&thread.to_ascii_lowercase())
}
fn unsupported(stderr: &str, stdout: &str) -> bool {
    let text = format!("{stderr}\n{stdout}").to_ascii_lowercase();
    text.contains("unrecognized subcommand")
        || text.contains("unknown command")
        || text.contains("unexpected argument 'queue'")
        || text.contains("unexpected argument \"queue\"")
}
fn compact(stderr: &str, stdout: &str) -> String {
    let text = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join("; ")
}
fn codex_pane_is_idle(text: &str) -> bool {
    if text.to_ascii_lowercase().contains("esc to interrupt") {
        return false;
    }
    text.lines()
        .map(str::trim)
        .rfind(|line| line.starts_with("› "))
        == Some("› Ask Codex to do anything")
}
fn terminal_text(text: &str) -> &str {
    if text.split_whitespace().next() == Some("/compact") {
        // Codex rejects inline compact arguments by interpreting them as a
        // normal user turn. Claude/OpenCode keep their instructions elsewhere.
        "/compact"
    } else if is_codex_command(text) {
        text.trim()
    } else {
        text
    }
}

fn is_codex_command(text: &str) -> bool {
    let first = text.split_whitespace().next().unwrap_or_default();
    let Some(command) = first.strip_prefix('/') else {
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

fn closed_turn(parse_complete: bool, last_event: Option<&str>) -> bool {
    parse_complete && matches!(last_event, Some("task_complete" | "turn_aborted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> crate::HarnessSession {
        crate::HarnessSession {
            display_name: id.into(),
            session_id: id.into(),
            tmux_session_name: "feature-codex".into(),
            last_active_ms: None,
            is_live: false,
            extras: crate::HarnessExtras::default(),
        }
    }

    #[test]
    fn identity_requires_one_exact_writer_lock_in_the_selected_codex_home() {
        let home = Path::new("/managed/codex");
        let sessions = [session("thread-a"), session("thread-b")];
        assert_eq!(
            writer_identity(
                "p10\nn/managed/codex/thread-writer-locks/thread-a.lock\n",
                home,
                &sessions
            ),
            Some("thread-a".into())
        );
        for output in [
            // Other Codex homes and unrelated files cannot prove ownership.
            "n/other/codex/thread-writer-locks/thread-a.lock\n",
            "n/managed/codex/sessions/rollout-thread-a.jsonl\n",
            "n/managed/codex/thread-writer-locks/thread-unknown.lock\n",
            // Multiple writers could be a shared daemon, never a TUI identity.
            "n/managed/codex/thread-writer-locks/thread-a.lock\nn/managed/codex/thread-writer-locks/thread-b.lock\n",
            "",
        ] {
            assert_eq!(writer_identity(output, home, &sessions), None, "{output}");
        }
    }

    #[test]
    fn command_transport_preserves_arguments_except_native_compact() {
        for command in [
            "/compact",
            " /compact focus on fleet ",
            "/compact\nToday is Monday",
        ] {
            assert_eq!(terminal_text(command), "/compact");
            assert!(is_codex_command(terminal_text(command)));
        }
        assert_eq!(
            terminal_text(" /review current changes "),
            "/review current changes"
        );
        for message in [
            "Explain /compact",
            "/Users/michael/file",
            "$manager",
            " ordinary text ",
        ] {
            assert_eq!(terminal_text(message), message);
            assert!(!is_codex_command(message));
        }
    }

    #[tokio::test]
    async fn thread_only_delivery_rejects_commands_before_contacting_queue() {
        let temp = tempfile::tempdir().unwrap();
        let runner = ProcessRunner::default();
        let tmux = TmuxClient::new(
            runner.clone(),
            wt_tmux::TmuxServer::at(temp.path().join("absent.sock")),
        );
        let mut messenger = CodexMessenger::new(
            CodexPaths::new(temp.path(), temp.path().join("cache")),
            runner,
            tmux,
        );
        for command in ["/compact focus", "/review", " /model next"] {
            let error = messenger
                .send("thread", command, &CancellationToken::new())
                .await
                .unwrap_err();
            assert_eq!(error.kind, CodexAppServerFailureKind::Rejected);
            assert!(error.detail.contains("interactive target"));
        }
        for text in [
            "/review\nsecond command",
            "/review\tchanges",
            "/review \u{1b}",
        ] {
            let outcome = messenger
                .send_target(
                    &CodexMessageTarget {
                        slug: "unused".into(),
                        cwd: temp.path().into(),
                        managed_name: None,
                        text: text.into(),
                    },
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(
                matches!(outcome, CodexMessageOutcome::Failed { reason } if reason.contains("control characters"))
            );
        }
    }

    #[test]
    fn commands_require_a_closed_turn_and_an_empty_ordinary_composer() {
        assert!(is_codex_command("/compact focus on current work"));
        assert!(!is_codex_command("/Users/michael/file"));
        for event in [None, Some("task_started")] {
            assert!(!closed_turn(true, event));
        }
        assert!(!closed_turn(false, Some("task_complete")));
        assert!(closed_turn(true, Some("task_complete")));
        assert!(closed_turn(true, Some("turn_aborted")));
        assert!(codex_pane_is_idle("output\n› Ask Codex to do anything"));
        assert!(!codex_pane_is_idle(
            "Working (esc to interrupt)\n› Ask Codex to do anything"
        ));
        assert!(!codex_pane_is_idle(
            "› Ask Codex to do anything\nAllow command?\n› 1. Yes"
        ));
        assert!(!codex_pane_is_idle("› user draft"));
    }
}
