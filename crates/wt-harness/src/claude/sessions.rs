use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use thiserror::Error;

use super::{
    claude_tmux_name,
    identity::{claude_session_id, session_jsonl_path},
    names::{NamesError, build_claude_session_entries, list_claude_names},
    registry::{read_registry, registry_by_session_id},
    summaries::read_session_summaries,
    transcript::{ClaudeStatus, read_session_tail},
};
use crate::{DiscoveryRequest, HarnessExtras, HarnessSession, HarnessSpawnRequest, SpawnCommand};
use tokio_util::sync::CancellationToken;
use wt_tmux::{TmuxClient, TmuxError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudePaths {
    pub home: PathBuf,
    pub cache_dir: PathBuf,
    pub config_dir: PathBuf,
    pub lock_dir: PathBuf,
}

impl ClaudePaths {
    pub fn new(home: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Self {
        let home = home.into();
        let cache_dir = cache_dir.into();
        Self {
            config_dir: home.join(".claude"),
            home,
            lock_dir: cache_dir.join("locks"),
            cache_dir,
        }
    }

    pub fn with_dirs(
        home: impl Into<PathBuf>,
        config_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
    ) -> Self {
        let cache_dir = cache_dir.into();
        Self {
            home: home.into(),
            config_dir: config_dir.into(),
            lock_dir: cache_dir.join("locks"),
            cache_dir,
        }
    }

    pub fn with_lock_dir(mut self, lock_dir: impl Into<PathBuf>) -> Self {
        self.lock_dir = lock_dir.into();
        self
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.config_dir.join("sessions")
    }
    pub fn names_file(&self) -> PathBuf {
        self.cache_dir.join("claude-sessions.json")
    }
    pub fn config_file(&self) -> PathBuf {
        self.home.join(".claude.json")
    }
    pub fn usage_file(&self) -> PathBuf {
        self.home.join(".cache/claude-statusline-usage.json")
    }
}

#[derive(Debug, Error)]
pub enum ClaudeHarnessError {
    #[error(transparent)]
    Names(#[from] NamesError),
    #[error("Claude harness I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Tmux(#[from] TmuxError),
}

#[derive(Clone, Debug)]
pub struct ClaudeHarness {
    paths: ClaudePaths,
}

impl ClaudeHarness {
    pub fn new(paths: ClaudePaths) -> Self {
        Self { paths }
    }
    pub fn paths(&self) -> &ClaudePaths {
        &self.paths
    }

    pub fn session_id(&self, worktree_path: &Path, managed_name: Option<&str>) -> String {
        claude_session_id(worktree_path, managed_name)
    }

    pub fn tmux_session_name(&self, slug: &str, managed_name: Option<&str>) -> String {
        claude_tmux_name(slug, managed_name)
    }

    pub fn build_spawn_command(&self, request: &HarnessSpawnRequest) -> SpawnCommand {
        let display_name = request
            .display_label
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| claude_tmux_name(&request.slug, request.managed_name.as_deref()));
        let session_id = claude_session_id(&request.worktree_path, request.managed_name.as_deref());
        let transcript = session_jsonl_path(&self.paths.home, &request.worktree_path, &session_id);
        let mut args = vec!["--name".to_owned(), display_name];
        if transcript.is_file() {
            args.extend(["--resume".to_owned(), session_id]);
        } else {
            args.extend(["--session-id".to_owned(), session_id]);
        }
        SpawnCommand {
            program: PathBuf::from("claude"),
            args,
        }
    }

    /// Discover persisted sessions for a worktree. `live_tmux_names` is a
    /// snapshot supplied by the caller so UI query caching need not depend on
    /// tmux polling. Entries always return `is_live=false`; the caller merges
    /// liveness by exact tmux name as the TypeScript adapter does.
    pub fn discover(
        &self,
        request: &DiscoveryRequest,
        live_tmux_names: &[String],
    ) -> Result<Vec<HarnessSession>, ClaudeHarnessError> {
        let names = list_claude_names(&self.paths.cache_dir, &request.slug)?;
        let live_names: Vec<Option<String>> = live_tmux_names
            .iter()
            .filter_map(|name| super::parse_claude_tmux_name(name, &request.slug))
            .collect();
        let all_names: Vec<Option<String>> = std::iter::once(None)
            .chain(names.into_iter().map(Some))
            .chain(live_names.iter().flatten().cloned().map(Some))
            .collect();
        let mut unique = Vec::new();
        for n in all_names {
            if !unique.contains(&n) {
                unique.push(n);
            }
        }
        let mut tails = HashMap::new();
        let mut status = ClaudeStatus::default();
        for name in &unique {
            let id = claude_session_id(&request.worktree_path, name.as_deref());
            let path = session_jsonl_path(&self.paths.home, &request.worktree_path, &id);
            let tail = read_session_tail(&path, name.clone());
            if tail.has_jsonl {
                status.sessions.push(tail.clone());
            }
            tails.insert(name.clone(), tail);
        }
        let registry =
            registry_by_session_id(&read_registry(&self.paths.sessions_dir()), &request.slug);
        let registry_status: HashMap<_, _> = registry
            .iter()
            .map(|(id, entry)| (id.clone(), entry.status))
            .collect();
        let ids: Vec<_> = unique
            .iter()
            .map(|name| claude_session_id(&request.worktree_path, name.as_deref()))
            .collect();
        let summaries: HashMap<_, _> =
            read_session_summaries(&self.paths.home, &request.worktree_path, &ids)
                .into_iter()
                .collect();
        let entries = build_claude_session_entries(
            &request.slug,
            &request.worktree_path,
            &unique.iter().flatten().cloned().collect::<Vec<_>>(),
            &live_names,
            &tails,
            &registry_status,
            &summaries,
        );
        Ok(entries
            .into_iter()
            .map(|entry| {
                let context_percent = tails
                    .get(&entry.name)
                    .and_then(|tail| tail.context_usage.as_ref())
                    .map(|usage| usage.percent());
                let session_id = entry.session_id.clone();
                let waiting_for = registry.get(&session_id).and_then(|r| {
                    (entry.state == crate::DerivedState::Asking)
                        .then(|| r.waiting_for.clone())
                        .flatten()
                });
                let status_since = registry
                    .get(&session_id)
                    .map(|r| r.updated_at)
                    .filter(|t| *t != 0);
                HarnessSession {
                    display_name: entry.name.clone().unwrap_or_else(|| "primary".to_owned()),
                    tmux_session_name: claude_tmux_name(&request.slug, entry.name.as_deref()),
                    session_id,
                    last_active_ms: entry.last_entry_ms,
                    is_live: false,
                    extras: HarnessExtras {
                        managed_name: entry.name,
                        derived_state: Some(entry.state),
                        queued: entry.queued,
                        waiting_for,
                        status_since,
                        tail_ended_at: None,
                        session_summary: entry.session_summary,
                        context_percent,
                    },
                }
            })
            .collect())
    }

    pub async fn discover_from_tmux(
        &self,
        tmux: &TmuxClient,
        request: &DiscoveryRequest,
        cancellation: &CancellationToken,
    ) -> Result<Vec<HarnessSession>, ClaudeHarnessError> {
        let sessions = tmux.list_sessions(cancellation).await?;
        let names: Vec<_> = sessions.into_iter().map(|session| session.name).collect();
        self.discover(request, &names)
    }

    pub fn names(&self, slug: &str) -> Result<Vec<String>, ClaudeHarnessError> {
        Ok(list_claude_names(&self.paths.cache_dir, slug)?)
    }

    pub fn usage(&self) -> Option<super::usage::ClaudeUsage> {
        super::usage::read_claude_usage(&self.paths.usage_file())
    }

    pub fn status(&self, request: &DiscoveryRequest) -> Result<ClaudeStatus, ClaudeHarnessError> {
        let names = self.names(&request.slug)?;
        let mut status = ClaudeStatus::default();
        for name in std::iter::once(None).chain(names.into_iter().map(Some)) {
            let id = claude_session_id(&request.worktree_path, name.as_deref());
            let path = session_jsonl_path(&self.paths.home, &request.worktree_path, &id);
            let tail = read_session_tail(&path, name);
            if tail.has_jsonl {
                status.sessions.push(tail);
            }
        }
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn spawn_uses_stable_name_and_switches_to_resume_when_transcript_exists() {
        let tmp = tempdir().unwrap();
        let paths = ClaudePaths::new(tmp.path().join("home"), tmp.path().join("cache"));
        let harness = ClaudeHarness::new(paths.clone());
        let req = HarnessSpawnRequest {
            worktree_path: PathBuf::from("/tmp/wt/demo"),
            slug: "demo".into(),
            managed_name: Some("review".into()),
            resume_session_id: None,
            display_label: None,
        };
        let fresh = harness.build_spawn_command(&req);
        assert_eq!(fresh.args[0..2], ["--name", "demo~review"]);
        assert_eq!(fresh.args[2], "--session-id");
        let id = claude_session_id(&req.worktree_path, Some("review"));
        let transcript = session_jsonl_path(&paths.home, &req.worktree_path, &id);
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, "{}").unwrap();
        let resumed = harness.build_spawn_command(&req);
        assert_eq!(resumed.args[2], "--resume");
        assert_eq!(resumed.args[3], id);
    }

    #[test]
    fn discovery_isolated_by_slug_and_includes_live_unregistered_names() {
        let tmp = tempdir().unwrap();
        let paths = ClaudePaths::new(tmp.path().join("home"), tmp.path().join("cache"));
        let harness = ClaudeHarness::new(paths.clone());
        super::super::names::add_claude_name(&paths.cache_dir, "repo-a", "review").unwrap();
        super::super::names::add_claude_name(&paths.cache_dir, "repo-b", "other").unwrap();
        let request = DiscoveryRequest {
            slug: "repo-a".into(),
            worktree_path: PathBuf::from("/tmp/repo-a"),
            live_session_id: None,
        };
        let result = harness
            .discover(
                &request,
                &[
                    "repo-a~review".into(),
                    "repo-a~manual".into(),
                    "repo-b~other".into(),
                ],
            )
            .unwrap();
        let names: Vec<_> = result
            .iter()
            .map(|s| s.extras.managed_name.as_deref())
            .collect();
        assert!(names.contains(&Some("review")));
        assert!(names.contains(&Some("manual")));
        assert!(!names.contains(&Some("other")));
    }

    #[tokio::test]
    async fn discovers_only_matching_sessions_from_an_isolated_tmux_server() {
        use std::num::NonZeroUsize;
        use tokio_util::sync::CancellationToken;
        use wt_platform::process::ProcessRunner;
        use wt_tmux::{CreateSession, TmuxClient, TmuxServer};

        let tmp = tempdir().unwrap();
        let paths = ClaudePaths::new(tmp.path().join("home"), tmp.path().join("cache"));
        let harness = ClaudeHarness::new(paths.clone());
        super::super::names::add_claude_name(&paths.cache_dir, "repo-a", "review").unwrap();
        let tmux = TmuxClient::new(
            ProcessRunner::new(NonZeroUsize::new(4).unwrap()),
            TmuxServer::at(tmp.path().join("private-tmux.sock"))
                .with_cwd(tmp.path())
                .with_config_file("/dev/null"),
        );
        let cancellation = CancellationToken::new();
        let started = tmux
            .create_session(
                &CreateSession {
                    name: "repo-a~review".into(),
                    cwd: tmp.path().to_owned(),
                    command: vec!["/bin/sleep".into(), "30".into()],
                    width: Some(80),
                    height: Some(24),
                },
                &cancellation,
            )
            .await;
        if let Err(error) = started {
            // Some build hosts intentionally do not install tmux. This is a
            // hermetic integration check, not a requirement for pure tests.
            if error.to_string().contains("No such file") {
                return;
            }
            panic!("isolated tmux server failed to start: {error}");
        }
        let request = DiscoveryRequest {
            slug: "repo-a".into(),
            worktree_path: PathBuf::from("/tmp/repo-a"),
            live_session_id: None,
        };
        let result = harness
            .discover_from_tmux(&tmux, &request, &cancellation)
            .await
            .unwrap();
        assert!(
            result
                .iter()
                .any(|session| session.tmux_session_name == "repo-a~review")
        );
        tmux.kill_server(&cancellation).await.unwrap();
    }
}
