use std::{collections::HashSet, path::Path, path::PathBuf, time::Duration};

use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessError, ProcessRunner};
use wt_tmux::{OptionScope, PaneTarget, TmuxClient};

use crate::{DiscoveryRequest, HarnessSpawnRequest, persist::FileLock};

use super::{
    CodexAppServerError, CodexAppServerFailureKind, CodexHarness, CodexHarnessError,
    CodexLaunchedThreads, CodexPaths, CodexQueueDelivery, app_server, find_rollout,
    read_codex_tail,
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
    /// Deliver into a running turn instead of queueing behind it.
    pub steer: bool,
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
        let slot = initial_inventory
            .iter()
            .find(|s| s.name == tmux_name)
            .cloned();
        let was_live = slot.is_some();
        let stamped_id = slot.as_ref().and_then(|s| s.harness_session_id.clone());
        let harness = CodexHarness::new(self.paths.clone(), self.runner.clone(), self.tmux.clone());
        let request = DiscoveryRequest {
            slug: target.slug.clone(),
            worktree_path: target.cwd.clone(),
            live_session_id: None,
        };
        // Rollout discovery walks and parses history files and takes a
        // blocking names lock; run it on a blocking worker.
        let discovered = {
            let harness = harness.clone();
            let stamped = stamped_id.clone();
            tokio::task::spawn_blocking(move || harness.discover_sync(&request, stamped.as_deref()))
                .await
                .map_err(|error| CodexHarnessError::Operation {
                    operation: "discover",
                    detail: error.to_string(),
                })??
        };

        let mut session_id = None;
        let mut unidentified = None;
        if let Some(slot) = &slot {
            if let Some(id) = stamped_id {
                // The stamp names the conversation wt resumed or proved
                // there; it must still be a conversation of this checkout and
                // slot before anything is addressed to it.
                if !discovered.iter().any(|session| session.session_id == id) {
                    return Ok(CodexMessageOutcome::Failed {
                        reason: format!(
                            "Codex slot {tmux_name} records thread {id}, which is not a conversation of this worktree slot. {NOTHING_SENT}"
                        ),
                    });
                }
                session_id = Some(id);
            } else {
                match self
                    .identify_slot(slot, &target.cwd, &target.slug, &discovered, true, cancel)
                    .await?
                {
                    SlotIdentity::Known(id) => session_id = Some(id),
                    SlotIdentity::Unknown(unknown) => unidentified = Some(unknown),
                    SlotIdentity::Changed => {
                        return Ok(CodexMessageOutcome::Failed {
                            reason: format!(
                                "Codex slot {tmux_name} changed while wt was identifying its conversation. Nothing was sent, so a retry is safe"
                            ),
                        });
                    }
                }
            }
        } else {
            session_id = target
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
                .map(|s| s.session_id.clone());
        }

        let spawn = HarnessSpawnRequest {
            worktree_path: target.cwd.clone(),
            slug: target.slug.clone(),
            managed_name: None,
            resume_session_id: session_id.clone(),
            display_label: None,
        };
        let cold_started = if !was_live {
            harness.start(&spawn, cancel).await?
        } else {
            false
        };

        if cold_started && session_id.is_none() {
            // Codex creates the thread's rollout only with its first prompt.
            // Manager and main launches carry that prompt, so they must
            // publish their UUID; a bare launch may instead show the empty
            // composer of a thread that has no history to misaddress.
            let opening_prompt = !harness.build_spawn_command(&spawn).args.is_empty();
            match self
                .await_launched_identity(
                    &tmux_name,
                    &target.cwd,
                    &target.slug,
                    !opening_prompt,
                    cancel,
                )
                .await?
            {
                Launched::Known(id) => session_id = Some(id),
                Launched::FreshComposer => {}
                Launched::Failed(reason) => return Ok(CodexMessageOutcome::Failed { reason }),
            }
        }

        let Some(id) = session_id else {
            let unknown = unidentified.unwrap_or(UnknownIdentity::NoThreadSinceLaunch);
            if let UnknownIdentity::Ambiguous(detail) = unknown {
                return Ok(CodexMessageOutcome::Failed {
                    reason: missing_identity(&tmux_name, &detail),
                });
            }
            // No conversation has been created since this slot launched. Only
            // an empty new-thread composer is safe to type into; anything
            // else may be a resumed conversation wt cannot name.
            return self
                .terminal_fallback(
                    &tmux_name,
                    &target.cwd,
                    &target.slug,
                    Expect::FreshComposer,
                    &target.text,
                    cold_started,
                    if is_codex_command(&target.text) {
                        "Codex slash commands execute through the interactive terminal"
                    } else {
                        "new Codex thread has no UUID until its first prompt"
                    },
                    cancel,
                )
                .await;
        };

        if is_codex_command(&target.text) {
            return self
                .terminal_fallback(
                    &tmux_name,
                    &target.cwd,
                    &target.slug,
                    Expect::Thread(&id),
                    &target.text,
                    cold_started,
                    "Codex slash commands execute through the interactive terminal",
                    cancel,
                )
                .await;
        }
        if target.steer {
            match app_server::steer_message(
                &self.paths.app_server_socket(),
                &id,
                &target.text,
                cancel,
            )
            .await
            {
                Ok(Some(delivery)) => return Ok(CodexMessageOutcome::Queued(delivery)),
                // No running turn to steer: the queue starts it immediately.
                Ok(None) => {}
                Err(error) if error.kind == CodexAppServerFailureKind::Ambiguous => {
                    return Ok(CodexMessageOutcome::Ambiguous {
                        reason: format!("{error}; wt did not retry or queue the message"),
                    });
                }
                Err(error) => {
                    return Ok(CodexMessageOutcome::Failed {
                        reason: format!("{error}. {NOTHING_SENT}"),
                    });
                }
            }
        }
        match self.send(&id, &target.text, cancel).await {
            Ok(delivery) => Ok(CodexMessageOutcome::Queued(delivery)),
            Err(error) if error.kind == CodexAppServerFailureKind::Ambiguous => {
                Ok(CodexMessageOutcome::Ambiguous {
                    reason: format!("{}; wt did not retry or type the message", error),
                })
            }
            Err(error)
                if matches!(
                    error.kind,
                    CodexAppServerFailureKind::Absent
                        | CodexAppServerFailureKind::Unavailable
                        | CodexAppServerFailureKind::Unsupported
                ) =>
            {
                match self.cli_queue(&id, &target.text, cancel).await {
                    CliQueueResult::Accepted => {
                        Ok(CodexMessageOutcome::CliQueued { thread_id: id })
                    }
                    CliQueueResult::Ambiguous(reason) => {
                        Ok(CodexMessageOutcome::Ambiguous { reason })
                    }
                    CliQueueResult::Unsupported => {
                        self.terminal_fallback(
                            &tmux_name,
                            &target.cwd,
                            &target.slug,
                            Expect::Thread(&id),
                            &target.text,
                            cold_started,
                            "Codex queue API is unavailable",
                            cancel,
                        )
                        .await
                    }
                }
            }
            Err(error) => Ok(CodexMessageOutcome::Failed {
                reason: error.to_string(),
            }),
        }
    }

    /// Identify an unstamped live slot: first by the pane's own writer lock,
    /// then by the sole conversation created in its checkout and slot since
    /// the tmux session started. A found UUID is stamped only on the same
    /// captured tmux session, and only while it is still unstamped.
    async fn identify_slot(
        &self,
        slot: &wt_tmux::SessionInfo,
        cwd: &Path,
        slug: &str,
        discovered: &[crate::HarnessSession],
        use_writer_lock: bool,
        cancel: &CancellationToken,
    ) -> Result<SlotIdentity, CodexHarnessError> {
        let proven = if use_writer_lock {
            self.recover_live_identity(&slot.name, discovered, cancel)
                .await?
        } else {
            None
        };
        let id = match proven {
            Some(id) => id,
            None => {
                let claimed = self
                    .tmux
                    .list_sessions(cancel)
                    .await?
                    .into_iter()
                    .filter(|other| other.name != slot.name)
                    .filter_map(|other| other.harness_session_id)
                    .collect::<HashSet<_>>();
                let harness =
                    CodexHarness::new(self.paths.clone(), self.runner.clone(), self.tmux.clone());
                let (checkout, slug, launched) =
                    (cwd.to_path_buf(), slug.to_owned(), slot.created_at);
                let found = tokio::task::spawn_blocking(move || {
                    harness.launched_threads_sync(&checkout, &slug, launched, &claimed)
                })
                .await
                .map_err(|error| CodexHarnessError::Operation {
                    operation: "discover",
                    detail: error.to_string(),
                })??;
                match found {
                    CodexLaunchedThreads::None => {
                        return Ok(SlotIdentity::Unknown(UnknownIdentity::NoThreadSinceLaunch));
                    }
                    CodexLaunchedThreads::Ambiguous(ids) => {
                        return Ok(SlotIdentity::Unknown(UnknownIdentity::Ambiguous(format!(
                            "{} conversations were started in this checkout since it launched",
                            ids.len()
                        ))));
                    }
                    // A sole rollout is only a candidate: another Codex TUI
                    // in this checkout could have created it. Require that
                    // this pane runs the only one before trusting it.
                    CodexLaunchedThreads::One(id) => {
                        if let Some(detail) = self.foreign_codex(slot, cwd, cancel).await? {
                            return Ok(SlotIdentity::Unknown(UnknownIdentity::Ambiguous(detail)));
                        }
                        id
                    }
                }
            }
        };
        let current = self.tmux.list_sessions(cancel).await?;
        let same_slot = current.iter().find(|now| {
            now.id == slot.id && now.created_at == slot.created_at && now.name == slot.name
        });
        match same_slot {
            Some(now)
                if now
                    .harness_session_id
                    .as_deref()
                    .is_none_or(|stamp| stamp == id) =>
            {
                self.tmux
                    .set_option(
                        &OptionScope::Session(slot.id.clone()),
                        "@wt-harness-session-id",
                        Some(&id),
                        cancel,
                    )
                    .await?;
                Ok(SlotIdentity::Known(id))
            }
            _ => Ok(SlotIdentity::Changed),
        }
    }

    /// Positive evidence for a discovered UUID. This slot's single pane must
    /// run a Codex TUI, and no other Codex TUI on this host may run in, or
    /// name, this checkout, except inside panes whose slots already record
    /// their own thread. Codex 0.163 TUIs are clients of a shared
    /// `codex app-server` daemon, which is not a TUI and is ignored; the TUI
    /// process itself keeps the checkout as its working directory. Returns
    /// why the evidence is missing; any unreadable source counts as missing.
    async fn foreign_codex(
        &self,
        slot: &wt_tmux::SessionInfo,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, CodexHarnessError> {
        let panes = self.tmux.list_all_panes(cancel).await?;
        let inventory = self.tmux.list_sessions(cancel).await?;
        let own = panes
            .iter()
            .filter(|pane| pane.session_name == slot.name && !pane.dead)
            .collect::<Vec<_>>();
        let [own] = own.as_slice() else {
            return Ok(Some(
                "its tmux session does not have exactly one live pane".into(),
            ));
        };
        let Some(own_pid) = own.pid else {
            return Ok(Some("its pane process is unknown".into()));
        };
        let claimed_roots = panes
            .iter()
            .filter(|pane| {
                pane.session_name != slot.name
                    && inventory.iter().any(|session| {
                        session.name == pane.session_name && session.harness_session_id.is_some()
                    })
            })
            .filter_map(|pane| pane.pid)
            .collect::<Vec<_>>();
        let mut ps = CommandSpec::new("ps").args([
            "-A", "-o", "pid=", "-o", "ppid=", "-o", "ucomm=", "-o", "args=",
        ]);
        ps.timeout = Duration::from_secs(2);
        let listing = match self.runner.run(ps, cancel).await {
            Ok(output) if output.status.success() => output.stdout_text(),
            Err(error @ ProcessError::Cancelled { .. }) => return Err(error.into()),
            _ => return Ok(Some("the process list is unavailable".into())),
        };
        let processes = parse_processes(&listing);
        let own_tree = descendants(&processes, own_pid);
        if !processes
            .iter()
            .any(|process| own_tree.contains(&process.pid) && process.is_codex_tui())
        {
            return Ok(Some("its pane is not running a Codex TUI".into()));
        }
        let claimed = claimed_roots
            .iter()
            .flat_map(|root| descendants(&processes, *root))
            .collect::<HashSet<_>>();
        let others = processes
            .iter()
            .filter(|process| {
                process.is_codex_tui()
                    && !own_tree.contains(&process.pid)
                    && !claimed.contains(&process.pid)
            })
            .collect::<Vec<_>>();
        if others.is_empty() {
            return Ok(None);
        }
        let mut checkouts = vec![cwd.to_path_buf()];
        if let Ok(real) = cwd.canonicalize() {
            checkouts.push(real);
        }
        let foreign = |pid: u32| {
            Some(format!(
                "another Codex process (pid {pid}) may have started a conversation in this checkout"
            ))
        };
        if let Some(process) = others.iter().find(|process| {
            checkouts
                .iter()
                .any(|path| process.args.contains(&*path.to_string_lossy()))
        }) {
            return Ok(foreign(process.pid));
        }
        let pids = others
            .iter()
            .map(|process| process.pid.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut lsof =
            CommandSpec::new("lsof").args(["-nP", "-a", "-d", "cwd", "-p", &pids, "-Fn"]);
        lsof.timeout = Duration::from_secs(2);
        // lsof exits nonzero when a listed process has already exited; the
        // working directories it did report are still evidence.
        let output = match self.runner.run(lsof, cancel).await {
            Ok(output) => output.stdout_text(),
            Err(error @ ProcessError::Cancelled { .. }) => return Err(error.into()),
            Err(_) => return Ok(Some("process working directories are unavailable".into())),
        };
        let mut pid = None;
        for line in output.lines() {
            if let Some(value) = line.strip_prefix('p') {
                pid = value.parse::<u32>().ok();
            } else if let Some(path) = line.strip_prefix('n')
                && checkouts.iter().any(|checkout| Path::new(path) == checkout)
            {
                return Ok(foreign(pid.unwrap_or_default()));
            }
        }
        Ok(None)
    }

    /// Wait for a slot this call launched without `resume` to either publish
    /// its new thread's UUID or, when allowed, show an empty new-thread
    /// composer. Delayed rollout publication is retried until the deadline.
    async fn await_launched_identity(
        &self,
        name: &str,
        cwd: &Path,
        slug: &str,
        allow_fresh_composer: bool,
        cancel: &CancellationToken,
    ) -> Result<Launched, CodexHarnessError> {
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        let target = PaneTarget::active_session_pane(name);
        loop {
            if cancel.is_cancelled() {
                return Ok(Launched::Failed(format!(
                    "cancelled before terminal submission. {NOTHING_SENT}"
                )));
            }
            let Some(slot) = self
                .tmux
                .list_sessions(cancel)
                .await?
                .into_iter()
                .find(|s| s.name == name)
            else {
                return Ok(Launched::Failed(format!(
                    "Codex slot {name} exited before it could receive the message. {NOTHING_SENT}"
                )));
            };
            if let Some(id) = slot.harness_session_id.as_deref() {
                return Ok(
                    if find_rollout(&self.paths.sessions_dir(), cwd, slug, id)?.is_some() {
                        Launched::Known(id.to_owned())
                    } else {
                        Launched::Failed(format!(
                            "Codex slot {name} records thread {id}, which is not a conversation of this worktree slot. {NOTHING_SENT}"
                        ))
                    },
                );
            }
            let pane = self
                .tmux
                .capture_pane(&target, Some(40), cancel)
                .await
                .unwrap_or_default();
            if let Some(blocker) = codex_pane_blocker(&pane) {
                return Ok(Launched::Failed(blocked(blocker)));
            }
            if let Some(composer) = idle_composer(&pane) {
                match self
                    .identify_slot(&slot, cwd, slug, &[], false, cancel)
                    .await?
                {
                    SlotIdentity::Known(id) => return Ok(Launched::Known(id)),
                    SlotIdentity::Changed => {
                        return Ok(Launched::Failed(format!(
                            "Codex slot {name} changed while wt was identifying its conversation. {NOTHING_SENT}"
                        )));
                    }
                    SlotIdentity::Unknown(UnknownIdentity::Ambiguous(detail)) => {
                        return Ok(Launched::Failed(missing_identity(name, &detail)));
                    }
                    SlotIdentity::Unknown(UnknownIdentity::NoThreadSinceLaunch)
                        if allow_fresh_composer && composer == Composer::Fresh =>
                    {
                        return Ok(Launched::FreshComposer);
                    }
                    // The opening prompt's rollout may not be published yet.
                    SlotIdentity::Unknown(UnknownIdentity::NoThreadSinceLaunch) => {}
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(Launched::Failed(missing_identity(
                    name,
                    "the new slot did not publish its thread UUID within 30 seconds",
                )));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
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
        // wt's terminal supervisor remains the pane process so it can retain
        // failures reliably on older tmux. Its foreground harness is a direct
        // child; include only children whose ownership we can recheck below.
        let child = self.supervised_child(&panes[0], cancel).await;
        let pids = std::iter::once(pid)
            .chain(child.iter().map(|child| child.0))
            .map(|pid| pid.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut command = CommandSpec::new("lsof").args(["-nP", "-a", "-p", &pids, "-Fn"]);
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
            && (child.is_none() || self.supervised_child(&after[0], cancel).await == child)
        {
            Ok(found)
        } else {
            Ok(None)
        }
    }

    async fn supervised_child(
        &self,
        pane: &wt_tmux::PaneInfo,
        cancel: &CancellationToken,
    ) -> Option<(u32, String)> {
        let pid = pane.pid?;
        let owner = self
            .tmux
            .get_option(
                &OptionScope::Pane(PaneTarget::id(&pane.id)),
                "@wt-terminal-supervisor",
                cancel,
            )
            .await
            .ok()
            .flatten()?;
        if owner != pid.to_string() {
            return None;
        }
        let mut command = CommandSpec::new("ps").args([
            "-A", "-o", "pid=", "-o", "ppid=", "-o", "lstart=", "-o", "comm=",
        ]);
        command.timeout = Duration::from_secs(2);
        let output = self.runner.run(command, cancel).await.ok()?;
        if !output.status.success() {
            return None;
        }
        sole_child(&output.stdout_text(), pid)
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
        expect: Expect<'_>,
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
        let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
        let target = PaneTarget::active_session_pane(name);
        let expected = match expect {
            Expect::Thread(id) => Some(id),
            Expect::FreshComposer => None,
        };
        loop {
            if cancel.is_cancelled() {
                return Ok(CodexMessageOutcome::Failed {
                    reason: format!("cancelled before terminal submission. {NOTHING_SENT}"),
                });
            }
            let inventory = self.tmux.list_sessions(cancel).await?;
            let Some(live) = inventory.iter().find(|s| s.name == name) else {
                return Ok(CodexMessageOutcome::Failed {
                    reason: format!(
                        "Codex slot {name} exited before terminal fallback. {NOTHING_SENT}"
                    ),
                });
            };
            let pane = self
                .tmux
                .capture_pane(&target, Some(40), cancel)
                .await
                .unwrap_or_default();
            if let Some(blocker) = codex_pane_blocker(&pane) {
                return Ok(CodexMessageOutcome::Failed {
                    reason: blocked(blocker),
                });
            }
            // Ownership is rechecked under the cross-process injection lock
            // immediately before typing. Identity failures never wait: they
            // cannot resolve themselves and a later retry is safe.
            let ready = match expect {
                Expect::Thread(id) => {
                    if live.harness_session_id.as_deref() != Some(id) {
                        return Ok(CodexMessageOutcome::Failed {
                            reason: identity_mismatch(name, id, live.harness_session_id.as_deref()),
                        });
                    }
                    let Some(file) = find_rollout(&self.paths.sessions_dir(), cwd, slug, id)?
                    else {
                        return Ok(CodexMessageOutcome::Failed {
                            reason: format!(
                                "Codex thread {id} is not a conversation of this worktree slot. {NOTHING_SENT}"
                            ),
                        });
                    };
                    let thread_ready = read_codex_tail(&file.path, file.mtime_ms, file.size)?
                        .is_some_and(|tail| {
                            closed_turn(tail.parse_complete, tail.last_task_event.as_deref())
                        });
                    thread_ready && idle_composer(&pane).is_some()
                }
                Expect::FreshComposer => {
                    if let Some(stamp) = live.harness_session_id.as_deref() {
                        return Ok(CodexMessageOutcome::Failed {
                            reason: format!(
                                "Codex slot {name} now records thread {stamp}. {NOTHING_SENT}"
                            ),
                        });
                    }
                    if idle_composer(&pane) != Some(Composer::Fresh) {
                        return Ok(CodexMessageOutcome::Failed {
                            reason: missing_identity(
                                name,
                                "it is not showing an empty new conversation",
                            ),
                        });
                    }
                    true
                }
            };
            if ready {
                let still_live = self
                    .tmux
                    .list_sessions(cancel)
                    .await?
                    .into_iter()
                    .find(|s| s.name == name);
                if let Some(live) = still_live
                    && live.harness_session_id.as_deref() == expected
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
                let id = expected.unwrap_or("unidentified");
                return Ok(CodexMessageOutcome::Failed {
                    reason: format!(
                        "Codex thread {id} in {name} did not become idle within 30 seconds (a turn is running, or the composer holds a draft or prompt; {reason}). {NOTHING_SENT}"
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

struct ProcessRow {
    pid: u32,
    ppid: u32,
    name: String,
    args: String,
}

impl ProcessRow {
    /// A Codex terminal client. The shared app-server daemon is not one.
    fn is_codex_tui(&self) -> bool {
        self.name == "codex" && !self.args.split_whitespace().any(|arg| arg == "app-server")
    }
}

fn parse_processes(listing: &str) -> Vec<ProcessRow> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let name = fields.next()?.to_owned();
            Some(ProcessRow {
                pid,
                ppid,
                name,
                args: fields.collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}

fn descendants(processes: &[ProcessRow], root: u32) -> HashSet<u32> {
    let mut tree = HashSet::from([root]);
    loop {
        let before = tree.len();
        for process in processes {
            if tree.contains(&process.ppid) && process.pid != process.ppid {
                tree.insert(process.pid);
            }
        }
        if tree.len() == before {
            return tree;
        }
    }
}

/// Only the pane process's exact writer lock under this Codex home proves
/// ownership. A shared daemon's loaded threads, resume arguments, and rollout
/// recency cannot identify which conversation its TUI currently displays.
fn sole_child(processes: &str, parent: u32) -> Option<(u32, String)> {
    let mut children = processes.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let pid = fields.next()?.parse::<u32>().ok()?;
        let ppid = fields.next()?.parse::<u32>().ok()?;
        let started = fields.collect::<Vec<_>>();
        (ppid == parent && pid > 0 && pid != parent && started.len() >= 6)
            .then(|| (pid, started.join(" ")))
    });
    let child = children.next()?;
    children.next().is_none().then_some(child)
}

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
fn codex_pane_blocker(text: &str) -> Option<&'static str> {
    // Match the active menu, not an earlier trust/login explanation in the
    // transcript. Never dismiss an interactive decision by typing a message.
    let selected = text
        .lines()
        .map(str::trim)
        .rfind(|line| line.starts_with("› "))?;
    if selected == "› 1. Trust and continue" && text.contains("Trust this folder?") {
        Some("folder trust")
    } else if selected == "› 1. Sign in with ChatGPT" && text.contains("API key") {
        Some("sign-in")
    } else {
        None
    }
}

/// Placeholder of an empty composer on a thread with no history.
const FRESH_PLACEHOLDER: &str = "› Ask Codex to do anything";
/// Placeholder Codex 0.163 shows on an empty composer once the thread has
/// history. Both strings are present in the 0.163 binary.
const FOLLOW_UP_PLACEHOLDER: &str = "› Ask a follow-up question";
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const NOTHING_SENT: &str = "Nothing was sent, so a retry is safe";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Composer {
    /// Empty composer of a new conversation.
    Fresh,
    /// Empty composer of a conversation with history.
    FollowUp,
}

#[derive(Clone, Copy, Debug)]
enum Expect<'a> {
    /// Type only into this exact, stamped thread once its turn is closed.
    Thread(&'a str),
    /// Type only into an unstamped slot showing an empty new conversation.
    FreshComposer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SlotIdentity {
    Known(String),
    Unknown(UnknownIdentity),
    /// The captured tmux session ended, was replaced, or gained another stamp.
    Changed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum UnknownIdentity {
    NoThreadSinceLaunch,
    /// Candidates exist but none is provably this pane's; the detail says why.
    Ambiguous(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Launched {
    Known(String),
    FreshComposer,
    Failed(String),
}

fn missing_identity(name: &str, detail: &str) -> String {
    format!(
        "Codex slot {name} has no proven thread UUID ({detail}), so wt cannot tell which conversation it shows. {NOTHING_SENT}. Resume the conversation from the session picker (;) so wt records its thread, then retry"
    )
}

fn identity_mismatch(name: &str, expected: &str, found: Option<&str>) -> String {
    let found = found.unwrap_or("no thread");
    format!("Codex slot {name} now records {found}, not thread {expected}. {NOTHING_SENT}")
}

fn blocked(blocker: &str) -> String {
    format!(
        "Codex is waiting for {blocker}; open this session with F12 to continue. No input was sent"
    )
}

fn idle_composer(text: &str) -> Option<Composer> {
    if text.to_ascii_lowercase().contains("esc to interrupt") {
        return None;
    }
    let lines = text.lines().map(str::trim).collect::<Vec<_>>();
    let composer = lines.iter().rposition(|line| line.starts_with("› "))?;
    match lines[composer] {
        // Codex 0.164 keeps this placeholder after a thread has history, so it
        // alone cannot prove an empty conversation: any transcript item above
        // the composer (an earlier prompt, an assistant or tool line, tool
        // output) means the conversation has history.
        FRESH_PLACEHOLDER
            if lines[..composer]
                .iter()
                .any(|line| ["› ", "• ", "└ "].iter().any(|mark| line.starts_with(mark))) =>
        {
            Some(Composer::FollowUp)
        }
        FRESH_PLACEHOLDER => Some(Composer::Fresh),
        FOLLOW_UP_PLACEHOLDER => Some(Composer::FollowUp),
        _ => None,
    }
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
    fn supervised_identity_requires_one_exact_child_and_keeps_birth_identity() {
        let processes = "12 10 Tue Oct 13 10:00:00 2026 codex\n13 12 Tue Oct 13 10:00:00 2026 codex\n14 1 Tue Oct 13 10:00:00 2026 codex\ninvalid\n";
        assert_eq!(
            sole_child(processes, 10),
            Some((12, "Tue Oct 13 10:00:00 2026 codex".into()))
        );
        assert_eq!(
            sole_child(
                &format!("{processes}15 10 Tue Oct 13 10:00:00 2026 codex\n"),
                10
            ),
            None
        );
        assert_eq!(sole_child("12 10 missing-birth\n", 10), None);
        assert_ne!(
            sole_child(processes, 10),
            sole_child(&processes.replace("10:00:00", "10:00:01"), 10)
        );
        assert_ne!(
            sole_child(processes, 10),
            sole_child(&processes.replace("codex", "tmux"), 10)
        );
    }

    #[test]
    #[ignore = "subprocess writer fixture"]
    fn stable_writer_process() {
        let Some(lock) = std::env::var_os("WT_TEST_WRITER_LOCK") else {
            return;
        };
        let _lock = std::fs::File::create(lock).unwrap();
        std::fs::write(std::env::var_os("WT_TEST_WRITER_READY").unwrap(), "ready").unwrap();
        // Signal readiness only after the final executable owns the descriptor.
        // No subsequent exec or temporary child can change its birth identity.
        loop {
            std::thread::park();
        }
    }

    #[tokio::test]
    async fn writer_identity_recovers_supervised_and_exec_owners_but_not_unmarked_children() {
        use wt_platform::process::ProcessRunner;
        use wt_tmux::{CreateSession, TmuxServer};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let paths = CodexPaths::new(&root, root.join("cache"));
        let locks = paths.codex_home.join("thread-writer-locks");
        std::fs::create_dir_all(&locks).unwrap();
        let tmux = TmuxClient::new(
            ProcessRunner::default(),
            TmuxServer::at(temp.path().join("tmux.sock")).with_config_file("/dev/null"),
        );
        let cancel = CancellationToken::new();
        let messenger = CodexMessenger::new(paths, ProcessRunner::default(), tmux.clone());
        for (name, supervised, child_only) in [
            ("exec", false, false),
            ("wrapped", true, false),
            ("unmarked", false, true),
        ] {
            let ready = temp.path().join(name);
            let writer = vec![
                "env".into(),
                format!(
                    "WT_TEST_WRITER_LOCK={}",
                    locks.join("thread-a.lock").display()
                ),
                format!("WT_TEST_WRITER_READY={}", ready.display()),
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                "--exact".into(),
                "codex::messaging::tests::stable_writer_process".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ];
            let command = if child_only {
                ["sh", "-c", "\"$@\" & wait", "sh"]
                    .into_iter()
                    .map(str::to_owned)
                    .chain(writer)
                    .collect()
            } else {
                writer
            };
            let client = if supervised {
                tmux.clone().with_failure_retention()
            } else {
                tmux.clone()
            };
            client
                .create_session(
                    &CreateSession {
                        name: name.into(),
                        cwd: temp.path().into(),
                        command,
                        width: None,
                        height: None,
                    },
                    &cancel,
                )
                .await
                .unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !ready.exists() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "writer did not start"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let found = messenger
                .recover_live_identity(name, &[session("thread-a")], &cancel)
                .await
                .unwrap();
            assert_eq!(
                found.as_deref(),
                (!child_only).then_some("thread-a"),
                "{name}"
            );
            client.kill_session(name, &cancel).await.unwrap();
        }
        // The last kill stops this private server.
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
                        steer: false,
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
    fn interactive_setup_blockers_name_the_active_menu_only() {
        let trust = "Folder access\n/repo\nTrust this folder? Codex can read files.\n› 1. Trust and continue\n  2. Back to Agent Command Center\nenter continue · esc back";
        assert_eq!(codex_pane_blocker(trust), Some("folder trust"));
        assert_eq!(idle_composer(trust), None);
        assert_eq!(
            codex_pane_blocker(&format!("{trust}\n› Ask Codex to do anything")),
            None
        );
        let login = "Welcome to Codex\n› 1. Sign in with ChatGPT\n  2. Provide an API key";
        assert_eq!(codex_pane_blocker(login), Some("sign-in"));
        assert_eq!(
            codex_pane_blocker("› Explain Sign in with ChatGPT and API key"),
            None
        );
    }

    #[tokio::test]
    async fn terminal_delivery_reports_trust_prompt_without_waiting_or_typing() {
        use wt_tmux::{CreateSession, TmuxServer};
        let temp = tempfile::tempdir().unwrap();
        let runner = ProcessRunner::default();
        let tmux = TmuxClient::new(
            runner.clone(),
            TmuxServer::at(temp.path().join("tmux.sock")).with_config_file("/dev/null"),
        );
        let cancel = CancellationToken::new();
        let name = "trust-codex";
        tmux.create_session(
            &CreateSession {
                name: name.into(),
                cwd: temp.path().into(),
                command: vec![
                    "python3".into(),
                    "-c".into(),
                    "import time; print('Folder access\\n/repo\\nTrust this folder?\\n› 1. Trust and continue\\n  2. Back to Agent Command Center', flush=True); time.sleep(30)".into(),
                ],
                width: Some(120),
                height: Some(30),
            },
            &cancel,
        )
        .await
        .unwrap();
        let target = PaneTarget::active_session_pane(name);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !tmux
            .capture_pane(&target, Some(40), &cancel)
            .await
            .unwrap()
            .contains("Trust and continue")
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "trust fixture did not start"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let messenger = CodexMessenger::new(
            CodexPaths::new(temp.path(), temp.path().join("cache")),
            runner,
            tmux.clone(),
        );
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            messenger.terminal_fallback(
                name,
                temp.path(),
                "trust",
                Expect::FreshComposer,
                "DO NOT TYPE",
                false,
                "unknown UUID",
                &cancel,
            ),
        )
        .await;
        let pane = tmux.capture_pane(&target, Some(40), &cancel).await.unwrap();
        tmux.kill_session(name, &cancel).await.unwrap();
        assert!(
            matches!(outcome.unwrap().unwrap(), CodexMessageOutcome::Failed { reason } if reason.contains("folder trust") && reason.contains("F12"))
        );
        assert!(!pane.contains("DO NOT TYPE"));
        assert!(pane.contains("Trust and continue"));
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
        assert_eq!(
            idle_composer("output\n› Ask Codex to do anything"),
            Some(Composer::Fresh)
        );
        // Codex 0.164 keeps the fresh placeholder on a thread with history;
        // transcript items above the composer show the history.
        for history in [
            "› earlier prompt\n• answer\n› Ask Codex to do anything",
            "• Ran cargo test\n  └ ok\n› Ask Codex to do anything",
            "  └ (no output)\n› Ask Codex to do anything",
        ] {
            assert_eq!(
                idle_composer(history),
                Some(Composer::FollowUp),
                "{history}"
            );
        }
        // Codex 0.163 changes the placeholder once a thread has history.
        assert_eq!(
            idle_composer("history\n› Ask a follow-up question\n  gpt · 80% left"),
            Some(Composer::FollowUp)
        );
        for busy in [
            "Working (esc to interrupt)\n› Ask Codex to do anything",
            "Working (Esc to interrupt)\n› Ask a follow-up question",
            "› Ask Codex to do anything\nAllow command?\n› 1. Yes",
            "› Ask a follow-up question\nAllow command?\n› 1. Yes",
            "› user draft",
            "› Ask a follow-up question and more",
            "",
        ] {
            assert_eq!(idle_composer(busy), None, "{busy}");
        }
    }

    /// Isolated Codex home, tmux server and optional fake app-server. Paths
    /// live under /tmp so Unix socket names stay within the platform limit.
    struct Fixture {
        _root: tempfile::TempDir,
        root: PathBuf,
        cwd: PathBuf,
        paths: CodexPaths,
        tmux: TmuxClient,
        cancel: CancellationToken,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir_in("/tmp").unwrap();
            let root = dir.path().canonicalize().unwrap();
            let cwd = root.join("feature");
            std::fs::create_dir_all(&cwd).unwrap();
            let paths = CodexPaths::new(&root, root.join("cache")).with_codex_home(root.join("c"));
            let tmux = TmuxClient::new(
                ProcessRunner::default(),
                wt_tmux::TmuxServer::at(root.join("tmux.sock")).with_config_file("/dev/null"),
            );
            Self {
                _root: dir,
                root,
                cwd,
                paths,
                tmux,
                cancel: CancellationToken::new(),
            }
        }

        fn messenger(&self) -> CodexMessenger {
            CodexMessenger::new(
                self.paths.clone(),
                ProcessRunner::default(),
                self.tmux.clone(),
            )
        }

        fn target(&self, text: &str) -> CodexMessageTarget {
            CodexMessageTarget {
                slug: "feature".into(),
                cwd: self.cwd.clone(),
                managed_name: None,
                text: text.into(),
                steer: false,
            }
        }

        /// A stand-in Codex TUI: prints `screen`, then echoes typed input.
        /// A copy of a system tool whose process name is `codex`, so process
        /// evidence sees a Codex TUI without running Codex.
        fn codex_bin(&self, tool: &str, dir: &str) -> PathBuf {
            let path = self.root.join(dir).join("codex");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            if !path.exists() {
                std::fs::copy(tool, &path).unwrap();
            }
            path
        }

        async fn slot(&self, screen: &str, stamp: Option<&str>) -> wt_tmux::SessionInfo {
            let tui = self.codex_bin("/bin/cat", "tui");
            self.slot_running(screen, stamp, &tui.to_string_lossy())
                .await
        }

        async fn slot_running(
            &self,
            screen: &str,
            stamp: Option<&str>,
            program: &str,
        ) -> wt_tmux::SessionInfo {
            let script = self.root.join("screen.txt");
            std::fs::write(&script, screen).unwrap();
            self.tmux
                .create_session(
                    &wt_tmux::CreateSession {
                        name: "feature-codex".into(),
                        cwd: self.cwd.clone(),
                        command: vec![
                            "sh".into(),
                            "-c".into(),
                            format!("cat '{}'; exec '{program}'", script.display()),
                        ],
                        width: Some(120),
                        height: Some(30),
                    },
                    &self.cancel,
                )
                .await
                .unwrap();
            if let Some(stamp) = stamp {
                self.tmux
                    .set_option(
                        &OptionScope::Session("feature-codex".into()),
                        "@wt-harness-session-id",
                        Some(stamp),
                        &self.cancel,
                    )
                    .await
                    .unwrap();
            }
            let last = screen.lines().last().unwrap_or_default().to_owned();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !self.pane().await.contains(&last) {
                assert!(tokio::time::Instant::now() < deadline, "slot did not start");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            self.live().await.unwrap()
        }

        async fn live(&self) -> Option<wt_tmux::SessionInfo> {
            self.tmux
                .list_sessions(&self.cancel)
                .await
                .unwrap()
                .into_iter()
                .find(|s| s.name == "feature-codex")
        }

        async fn pane(&self) -> String {
            self.tmux
                .capture_pane(
                    &PaneTarget::active_session_pane("feature-codex"),
                    Some(40),
                    &self.cancel,
                )
                .await
                .unwrap_or_default()
        }

        /// A rollout created `age_secs` before now, owned by `cwd`.
        fn rollout(&self, id: &str, cwd: &Path, age_secs: i64) {
            self.rollout_from(id, cwd, age_secs, "codex-tui");
        }

        fn rollout_from(&self, id: &str, cwd: &Path, age_secs: i64, originator: &str) {
            let created = time::OffsetDateTime::now_utc() - time::Duration::seconds(age_secs);
            let stamp = created
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap();
            let path = self
                .paths
                .sessions_dir()
                .join("2026/10/16")
                .join(format!("rollout-{id}.jsonl"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let header = serde_json::json!({"type":"session_meta","payload":{"id":id,"timestamp":stamp,"cwd":cwd.to_string_lossy(),"originator":originator,"thread_source":"user"}});
            let done = serde_json::json!({"type":"event_msg","timestamp":stamp,"payload":{"type":"task_complete"}});
            std::fs::write(&path, format!("{header}\n{done}\n")).unwrap();
        }

        async fn close(self) {
            let _ = self.tmux.kill_server(&self.cancel).await;
        }
    }

    /// Fake daemon accepting one queue/add + queue/start per connection and
    /// recording every request's method and thread.
    type RequestLog = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

    fn fake_app_server(paths: &CodexPaths) -> (RequestLog, tokio::task::JoinHandle<()>) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let socket = paths.app_server_socket();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let log = log.clone();
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(Message::Text(text))) = ws.next().await {
                        let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                        let method = request["method"].as_str().unwrap_or_default().to_owned();
                        log.lock().unwrap().push((
                            method.clone(),
                            request["params"]["threadId"]
                                .as_str()
                                .unwrap_or_default()
                                .to_owned(),
                        ));
                        let result = match method.as_str() {
                            "initialize" => {
                                serde_json::json!({"userAgent":"fake/0.163.0","codexHome":"/fake"})
                            }
                            "thread/queue/add" => serde_json::json!({"queuedSubmission":{
                                "id":"sub-1",
                                "clientUserMessageId":request["params"]["clientUserMessageId"],
                                "input":request["params"]["input"]}}),
                            "thread/queue/start" => serde_json::json!({}),
                            // Thread `busy` runs turn-1; every other thread is idle.
                            "thread/turns/list" => {
                                let status = if request["params"]["threadId"] == "busy" {
                                    "inProgress"
                                } else {
                                    "completed"
                                };
                                serde_json::json!({"data":[{"id":"turn-1","items":[],"status":status}]})
                            }
                            "turn/steer" if request["params"]["expectedTurnId"] == "turn-1" => {
                                serde_json::json!({"turnId":"turn-1"})
                            }
                            "turn/steer" => {
                                let reply = serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                                    "error":{"code":-32600,"message":"expected turn mismatch"}});
                                ws.send(Message::Text(reply.to_string().into()))
                                    .await
                                    .unwrap();
                                continue;
                            }
                            _ => continue,
                        };
                        let reply =
                            serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result});
                        ws.send(Message::Text(reply.to_string().into()))
                            .await
                            .unwrap();
                    }
                });
            }
        });
        (seen, task)
    }

    fn queued_to(seen: &std::sync::Mutex<Vec<(String, String)>>) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| method == "thread/queue/add")
            .map(|(_, thread)| thread.clone())
            .collect()
    }

    #[test]
    fn launched_thread_discovery_needs_one_post_launch_rollout_of_this_slot() {
        let fixture = Fixture::new();
        let harness = CodexHarness::new(
            fixture.paths.clone(),
            ProcessRunner::default(),
            fixture.tmux.clone(),
        );
        let launched = time::OffsetDateTime::now_utc().unix_timestamp() - 60;
        let none = HashSet::new();
        let found = |claimed: &HashSet<String>| {
            harness
                .launched_threads_sync(&fixture.cwd, "feature", launched, claimed)
                .unwrap()
        };
        assert_eq!(found(&none), CodexLaunchedThreads::None);
        // Conversations created before the slot started, or for another
        // checkout, are never candidates.
        fixture.rollout("old", &fixture.cwd, 3600);
        fixture.rollout("elsewhere", &fixture.root.join("other"), 1);
        assert_eq!(found(&none), CodexLaunchedThreads::None);
        fixture.rollout("new", &fixture.cwd, 1);
        assert_eq!(found(&none), CodexLaunchedThreads::One("new".into()));
        // A second conversation since launch (for example `/new`) is ambiguous
        // unless another live slot already owns it.
        fixture.rollout("second", &fixture.cwd, 1);
        assert_eq!(
            found(&none),
            CodexLaunchedThreads::Ambiguous(vec!["new".into(), "second".into()])
        );
        assert_eq!(
            found(&HashSet::from(["second".to_owned()])),
            CodexLaunchedThreads::One("new".into())
        );
    }

    #[tokio::test]
    async fn unstamped_live_slot_discovers_its_new_thread_and_queues_natively() {
        let fixture = Fixture::new();
        let (seen, server) = fake_app_server(&fixture.paths);
        fixture
            .slot("history\n› Ask a follow-up question\n", None)
            .await;
        fixture.rollout("old-thread", &fixture.cwd, 3600);
        // A daemon-backed TUI's thread carries the client name that first
        // connected to the shared app-server, not `codex-tui`.
        fixture.rollout_from("new-thread", &fixture.cwd, 0, "dotfiles_health");
        let outcome = fixture
            .messenger()
            .send_target(&fixture.target("Address review"), &fixture.cancel)
            .await
            .unwrap();
        let stamp = fixture.live().await.unwrap().harness_session_id;
        let pane = fixture.pane().await;
        server.abort();
        fixture.close().await;
        assert!(
            matches!(outcome, CodexMessageOutcome::Queued(_)),
            "{outcome:?}"
        );
        assert_eq!(queued_to(&seen), ["new-thread"]);
        assert_eq!(stamp.as_deref(), Some("new-thread"));
        assert!(!pane.contains("Address review"));
    }

    #[tokio::test]
    async fn busy_slot_finds_its_thread_although_the_daemon_labels_it_desktop() {
        let fixture = Fixture::new();
        let (seen, server) = fake_app_server(&fixture.paths);
        // Codex 0.164 shows the new-thread placeholder on a working thread,
        // so the composer alone cannot tell this slot from an empty one.
        fixture
            .slot(
                "history\n• Working (6m 15s • esc to interrupt)\n› Ask Codex to do anything\n",
                None,
            )
            .await;
        fixture.rollout_from("launched", &fixture.cwd, 0, "Codex Desktop");
        let outcome = fixture
            .messenger()
            .send_target(&fixture.target("Integrate scratch"), &fixture.cancel)
            .await
            .unwrap();
        let stamp = fixture.live().await.unwrap().harness_session_id;
        let pane = fixture.pane().await;
        server.abort();
        fixture.close().await;
        assert!(
            matches!(outcome, CodexMessageOutcome::Queued(_)),
            "{outcome:?}"
        );
        assert_eq!(queued_to(&seen), ["launched"]);
        assert_eq!(stamp.as_deref(), Some("launched"));
        assert!(!pane.contains("Integrate scratch"));
    }

    #[tokio::test]
    async fn stamped_resumed_thread_is_addressed_without_discovery() {
        let fixture = Fixture::new();
        let (seen, server) = fake_app_server(&fixture.paths);
        // A resumed conversation predates its slot; its stamp is the proof.
        fixture.rollout("resumed", &fixture.cwd, 3600);
        fixture.rollout("newer", &fixture.cwd, 0);
        fixture
            .slot("history\n› Ask a follow-up question\n", Some("resumed"))
            .await;
        let outcome = fixture
            .messenger()
            .send_target(&fixture.target("hello"), &fixture.cancel)
            .await
            .unwrap();
        server.abort();
        fixture.close().await;
        assert!(
            matches!(outcome, CodexMessageOutcome::Queued(_)),
            "{outcome:?}"
        );
        assert_eq!(queued_to(&seen), ["resumed"]);
    }

    #[tokio::test]
    async fn steer_delivers_into_a_running_turn_and_queues_when_idle() {
        let fixture = Fixture::new();
        let (seen, server) = fake_app_server(&fixture.paths);
        fixture.rollout("busy", &fixture.cwd, 0);
        fixture
            .slot("history\n› Ask Codex to do anything\n", Some("busy"))
            .await;
        let steer = |text: &str| CodexMessageTarget {
            steer: true,
            ..fixture.target(text)
        };
        let busy = fixture
            .messenger()
            .send_target(&steer("GO now"), &fixture.cancel)
            .await
            .unwrap();
        let steered = seen.lock().unwrap().clone();
        fixture.close().await;

        let fixture = Fixture::new();
        fixture.rollout("idle", &fixture.cwd, 0);
        fixture
            .slot("history\n› Ask Codex to do anything\n", Some("idle"))
            .await;
        seen.lock().unwrap().clear();
        server.abort();
        let (seen, server) = fake_app_server(&fixture.paths);
        let idle = fixture
            .messenger()
            .send_target(
                &CodexMessageTarget {
                    steer: true,
                    ..fixture.target("GO now")
                },
                &fixture.cancel,
            )
            .await
            .unwrap();
        server.abort();
        fixture.close().await;
        assert!(
            matches!(&busy, CodexMessageOutcome::Queued(delivery)
                if delivery.state == super::super::QueueState::Steered
                    && delivery.submission.id == "turn-1"),
            "{busy:?}"
        );
        assert!(
            steered
                .iter()
                .any(|(method, thread)| method == "turn/steer" && thread == "busy")
        );
        assert!(
            !steered
                .iter()
                .any(|(method, _)| method == "thread/queue/add")
        );
        // With no running turn the queue starts the message at once.
        assert!(
            matches!(&idle, CodexMessageOutcome::Queued(delivery)
                if delivery.state != super::super::QueueState::Steered),
            "{idle:?}"
        );
        assert_eq!(queued_to(&seen), ["idle"]);
    }

    #[tokio::test]
    async fn stale_stamp_for_another_checkout_is_refused_before_any_write() {
        let fixture = Fixture::new();
        let (seen, server) = fake_app_server(&fixture.paths);
        fixture.rollout("foreign", &fixture.root.join("other"), 60);
        fixture
            .slot("history\n› Ask a follow-up question\n", Some("foreign"))
            .await;
        let outcome = fixture
            .messenger()
            .send_target(&fixture.target("DO NOT SEND"), &fixture.cancel)
            .await
            .unwrap();
        let pane = fixture.pane().await;
        server.abort();
        fixture.close().await;
        assert!(
            matches!(&outcome, CodexMessageOutcome::Failed { reason }
                if reason.contains("foreign") && reason.contains("Nothing was sent")),
            "{outcome:?}"
        );
        assert!(queued_to(&seen).is_empty());
        assert!(!pane.contains("DO NOT SEND"));
    }

    #[tokio::test]
    async fn missing_identity_fails_fast_without_typing() {
        let fixture = Fixture::new();
        let (seen, server) = fake_app_server(&fixture.paths);
        // A conversation with history whose UUID was never published.
        fixture
            .slot("history\n› Ask a follow-up question\n", None)
            .await;
        let started = tokio::time::Instant::now();
        let outcome = fixture
            .messenger()
            .send_target(&fixture.target("DO NOT TYPE"), &fixture.cancel)
            .await
            .unwrap();
        let elapsed = started.elapsed();
        let pane = fixture.pane().await;
        let stamp = fixture.live().await.unwrap().harness_session_id;

        // Two conversations started since launch cannot name the slot either.
        fixture.rollout("first", &fixture.cwd, 0);
        fixture.rollout("second", &fixture.cwd, 0);
        let ambiguous = fixture
            .messenger()
            .send_target(&fixture.target("DO NOT TYPE"), &fixture.cancel)
            .await
            .unwrap();
        let pane_after = fixture.pane().await;
        server.abort();
        fixture.close().await;
        assert!(
            matches!(&outcome, CodexMessageOutcome::Failed { reason }
                if reason.contains("no proven thread UUID")
                    && reason.contains("Nothing was sent")
                    && reason.contains("session picker")
                    && !reason.contains("idle")),
            "{outcome:?}"
        );
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
        assert!(
            matches!(&ambiguous, CodexMessageOutcome::Failed { reason }
                if reason.contains("2 conversations") && reason.contains("Nothing was sent")),
            "{ambiguous:?}"
        );
        assert_eq!(stamp, None);
        assert!(queued_to(&seen).is_empty());
        assert!(!pane.contains("DO NOT TYPE"));
        assert!(!pane_after.contains("DO NOT TYPE"));
    }

    #[tokio::test]
    async fn empty_new_conversation_without_a_thread_accepts_terminal_input() {
        let fixture = Fixture::new();
        fixture.slot("› Ask Codex to do anything\n", None).await;
        let outcome = fixture
            .messenger()
            .send_target(&fixture.target("first prompt"), &fixture.cancel)
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !fixture.pane().await.contains("first prompt") {
            assert!(tokio::time::Instant::now() < deadline, "input not typed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        fixture.close().await;
        assert!(
            matches!(
                outcome,
                CodexMessageOutcome::Terminal {
                    cold_started: false,
                    ..
                }
            ),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn delayed_uuid_publication_is_discovered_and_stamped() {
        let fixture = Fixture::new();
        // The opening prompt ran; its rollout is published only later.
        fixture
            .slot("startup prompt\n› Ask a follow-up question\n", None)
            .await;
        let messenger = fixture.messenger();
        let publish = async {
            tokio::time::sleep(Duration::from_millis(600)).await;
            fixture.rollout("late", &fixture.cwd, 0);
        };
        let (launched, ()) = tokio::join!(
            messenger.await_launched_identity(
                "feature-codex",
                &fixture.cwd,
                "feature",
                false,
                &fixture.cancel,
            ),
            publish
        );
        let stamp = fixture.live().await.unwrap().harness_session_id;
        fixture.close().await;
        assert_eq!(launched.unwrap(), Launched::Known("late".into()));
        assert_eq!(stamp.as_deref(), Some("late"));
    }

    #[tokio::test]
    async fn opening_prompt_launch_never_types_into_a_fresh_composer() {
        let fixture = Fixture::new();
        fixture.slot("› Ask Codex to do anything\n", None).await;
        let messenger = fixture.messenger();
        // Bound the 30s wait; the result must not be FreshComposer before it.
        let launched = tokio::time::timeout(
            Duration::from_secs(1),
            messenger.await_launched_identity(
                "feature-codex",
                &fixture.cwd,
                "feature",
                false,
                &fixture.cancel,
            ),
        )
        .await;
        let allowed = messenger
            .await_launched_identity(
                "feature-codex",
                &fixture.cwd,
                "feature",
                true,
                &fixture.cancel,
            )
            .await
            .unwrap();
        fixture.close().await;
        assert!(launched.is_err(), "{launched:?}");
        assert_eq!(allowed, Launched::FreshComposer);
    }

    #[tokio::test]
    async fn exact_thread_mismatch_is_refused_immediately_without_typing() {
        let fixture = Fixture::new();
        fixture.rollout("expected", &fixture.cwd, 60);
        fixture
            .slot("history\n› Ask a follow-up question\n", Some("other"))
            .await;
        let started = tokio::time::Instant::now();
        let outcome = fixture
            .messenger()
            .terminal_fallback(
                "feature-codex",
                &fixture.cwd,
                "feature",
                Expect::Thread("expected"),
                "/compact",
                false,
                "test",
                &fixture.cancel,
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        let pane = fixture.pane().await;
        fixture.close().await;
        assert!(
            matches!(&outcome, CodexMessageOutcome::Failed { reason }
                if reason.contains("not thread expected") && reason.contains("Nothing was sent")),
            "{outcome:?}"
        );
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        assert!(!pane.contains("/compact"));
    }

    #[tokio::test]
    async fn exact_thread_on_follow_up_composer_accepts_a_command() {
        let fixture = Fixture::new();
        fixture.rollout("expected", &fixture.cwd, 60);
        fixture
            .slot("history\n› Ask a follow-up question\n", Some("expected"))
            .await;
        let outcome = fixture
            .messenger()
            .send_target(&fixture.target("/compact"), &fixture.cancel)
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !fixture.pane().await.contains("/compact") {
            assert!(tokio::time::Instant::now() < deadline, "command not typed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        fixture.close().await;
        assert!(
            matches!(outcome, CodexMessageOutcome::Terminal { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn foreign_codex_in_the_same_checkout_blocks_discovery() {
        let fixture = Fixture::new();
        let (seen, server) = fake_app_server(&fixture.paths);
        fixture
            .slot("history\n› Ask a follow-up question\n", None)
            .await;
        // A hand-run Codex TUI in the same worktree, outside wt's pane, could
        // have created the only new rollout.
        let foreign_bin = fixture.codex_bin("/bin/sleep", "foreign");
        let mut foreign = std::process::Command::new(&foreign_bin)
            .arg("30")
            .current_dir(&fixture.cwd)
            .spawn()
            .unwrap();
        fixture.rollout("someone-elses", &fixture.cwd, 0);
        let outcome = fixture
            .messenger()
            .send_target(&fixture.target("DO NOT SEND"), &fixture.cancel)
            .await
            .unwrap();
        let stamp = fixture.live().await.unwrap().harness_session_id;
        let pane = fixture.pane().await;
        let _ = foreign.kill();
        let _ = foreign.wait();
        server.abort();
        fixture.close().await;
        assert!(
            matches!(&outcome, CodexMessageOutcome::Failed { reason }
                if reason.contains("another Codex process")
                    && reason.contains(&foreign.id().to_string())
                    && reason.contains("Nothing was sent")),
            "{outcome:?}"
        );
        assert_eq!(stamp, None);
        assert!(queued_to(&seen).is_empty());
        assert!(!pane.contains("DO NOT SEND"));
    }

    #[tokio::test]
    async fn foreign_rollouts_and_non_codex_panes_are_never_stamped() {
        let fixture = Fixture::new();
        let (seen, server) = fake_app_server(&fixture.paths);
        fixture
            .slot_running("history\n› Ask a follow-up question\n", None, "cat")
            .await;
        // A rollout cannot be attributed to a pane that runs no Codex.
        fixture.rollout("tui-thread", &fixture.cwd, 0);
        let no_tui = fixture
            .messenger()
            .send_target(&fixture.target("DO NOT SEND"), &fixture.cancel)
            .await
            .unwrap();
        let live_stamp = fixture.live().await.unwrap().harness_session_id;
        server.abort();
        fixture.close().await;
        assert!(
            matches!(&no_tui, CodexMessageOutcome::Failed { reason }
                if reason.contains("not running a Codex TUI")),
            "{no_tui:?}"
        );
        assert_eq!(live_stamp, None);
        assert!(queued_to(&seen).is_empty());
    }

    #[test]
    fn process_trees_ignore_the_shared_app_server_daemon() {
        let rows = parse_processes(
            "10 1 sh sh -c x\n11 10 codex /bin/codex\n12 11 node node x\n20 1 codex /bin/codex app-server --listen unix://\nbad\n",
        );
        assert_eq!(descendants(&rows, 10), HashSet::from([10, 11, 12]));
        let tuis = rows
            .iter()
            .filter(|row| row.is_codex_tui())
            .map(|row| row.pid)
            .collect::<Vec<_>>();
        assert_eq!(tuis, [11]);
    }
}
