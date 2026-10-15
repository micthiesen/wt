use std::{path::PathBuf, sync::Arc};

use tokio_util::sync::CancellationToken;
use wt_config::Config;
use wt_platform::process::ProcessRunner;
use wt_vcs::GitRepository;

use crate::database::Database;

/// Explicit application services shared by CLI handlers and TUI actions.
/// Presentation never owns this context or calls these services during render.
#[derive(Clone)]
pub struct AppContext {
    pub config: Arc<Config>,
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub database: Database,
    pub repository: Arc<GitRepository>,
    pub processes: ProcessRunner,
    /// Original admission retained when a selected action view is reused by a
    /// continuation. Ordinary work must never inherit the interactive reserve.
    pub background_processes: ProcessRunner,
    /// Reserved admission for user-awaited controls. Created once per process;
    /// background reads and lifecycle commands cannot consume these permits.
    pub interactive_processes: ProcessRunner,
    pub cancellation: CancellationToken,
    /// Section moves this process made, so the attention feed can tell the
    /// user's own moves from moves made elsewhere.
    pub section_writes: SectionWrites,
    /// Disposable per-source snapshot cache for this configuration.
    pub caches: wt_runtime::CacheStore,
}

impl AppContext {
    /// Use the controller's scheduling classification at both local and remote
    /// execution boundaries. Cloning this view never creates a new process pool.
    pub fn for_action(&self, action: &wt_tui::UiAction) -> anyhow::Result<Self> {
        use crate::controller::Lane;
        let mut context = self.clone();
        let interactive = matches!(
            crate::controller::lane_for(action)?,
            Lane::Terminal(..)
                | Lane::Preparation(..)
                | Lane::Metadata(..)
                | Lane::HostControl(..)
                | Lane::Target(..)
                | Lane::SessionControl(..)
                | Lane::Presentation
                | Lane::ViewControl
        );
        context.processes = if interactive {
            self.interactive_processes.clone()
        } else {
            self.background_processes.clone()
        };
        context.repository = Arc::new(
            self.repository
                .with_process_runner(context.processes.clone()),
        );
        Ok(context)
    }
}

/// Section assignments this process wrote and has not yet seen come back
/// through the state source. Bounded and short-lived: an unconsumed entry
/// expires, so a missed observation cannot hide a later outside move.
#[derive(Clone, Default)]
pub struct SectionWrites(Arc<std::sync::Mutex<Vec<SectionWrite>>>);

struct SectionWrite {
    key: String,
    section: Option<String>,
    at: std::time::Instant,
}

const SECTION_WRITE_TTL: std::time::Duration = std::time::Duration::from_secs(60);
const SECTION_WRITE_CAP: usize = 512;

impl SectionWrites {
    pub fn record(&self, keys: impl IntoIterator<Item = String>, section: Option<&str>) {
        let mut writes = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        writes.retain(|write| now.duration_since(write.at) < SECTION_WRITE_TTL);
        writes.extend(keys.into_iter().map(|key| SectionWrite {
            key,
            section: section.map(str::to_owned),
            at: now,
        }));
        if writes.len() > SECTION_WRITE_CAP {
            let excess = writes.len() - SECTION_WRITE_CAP;
            writes.drain(..excess);
        }
    }

    /// Whether `key` landing in `section` is one of this process's writes;
    /// a match is consumed.
    pub fn consume(&self, key: &str, section: Option<&str>) -> bool {
        let mut writes = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        writes.retain(|write| now.duration_since(write.at) < SECTION_WRITE_TTL);
        let matches = |write: &SectionWrite| {
            write.key == key
                && match (write.section.as_deref(), section) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    (None, None) => true,
                    _ => false,
                }
        };
        match writes.iter().position(matches) {
            Some(index) => {
                writes.remove(index);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SectionWrites;

    async fn hold_process_permit(
        runner: wt_platform::process::ProcessRunner,
    ) -> (
        tokio_util::sync::CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::Arc;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let token = cancellation.clone();
        let started = Arc::new(tokio::sync::Notify::new());
        let observed = started.clone();
        let task = tokio::spawn(async move {
            let _ = runner
                .run_streaming(
                    wt_platform::process::CommandSpec::new("sh")
                        .args(["-c", "printf ready; exec sleep 30"]),
                    &token,
                    move |_, bytes| {
                        if !bytes.is_empty() {
                            observed.notify_one();
                        }
                    },
                )
                .await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .expect("blocking fixture process started");
        (cancellation, task)
    }

    #[tokio::test]
    async fn user_awaited_processes_bypass_saturated_background_admission() -> anyhow::Result<()> {
        use std::{num::NonZeroUsize, sync::Arc, time::Duration};
        use wt_platform::process::{CommandSpec, ProcessRunner};
        use wt_tui::UiAction;

        let mut fixture = crate::commands::test_support::CommandFixture::new().await?;
        fixture.ctx.processes = ProcessRunner::new(NonZeroUsize::new(1).unwrap());
        fixture.ctx.background_processes = fixture.ctx.processes.clone();
        fixture.ctx.repository = Arc::new(
            fixture
                .ctx
                .repository
                .with_process_runner(fixture.ctx.processes.clone()),
        );
        let (cancel, blocker) = hold_process_permit(fixture.ctx.processes.clone()).await;
        let result: anyhow::Result<()> = async {
            let interactive = fixture.ctx.for_action(&UiAction::PrepareHardRefresh)?;
            let ordinary = interactive.for_action(&UiAction::Restack { key: "one".into() })?;
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(100),
                    ordinary
                        .processes
                        .run(CommandSpec::new("true"), &ordinary.cancellation),
                )
                .await
                .is_err(),
                "ordinary work remains bounded by its occupied pool"
            );
            for action in [
                UiAction::EnterHarness {
                    key: "one".into(),
                    harness: wt_core::HarnessId::Codex,
                },
                UiAction::PrepareSessions {
                    key: Some("one".into()),
                    target: wt_tui::SessionTarget::Harness,
                },
                UiAction::SetTitle {
                    key: "one".into(),
                    title: "Renamed".into(),
                },
            ] {
                for routed in [
                    action.clone(),
                    UiAction::OnHost {
                        host: Some("remote".into()),
                        action: Box::new(action.clone()),
                    },
                ] {
                    let context = fixture.ctx.for_action(&routed)?;
                    tokio::time::timeout(Duration::from_secs(5), async {
                        context
                            .processes
                            .run(CommandSpec::new("true"), &context.cancellation)
                            .await?;
                        context.repository.inventory(&context.cancellation).await?;
                        anyhow::Ok(())
                    })
                    .await??;
                }
            }
            Ok(())
        }
        .await;
        cancel.cancel();
        blocker.await?;
        fixture.close().await?;
        result
    }

    #[tokio::test]
    async fn action_contexts_share_one_bounded_interactive_pool() -> anyhow::Result<()> {
        use std::{num::NonZeroUsize, time::Duration};
        use wt_platform::process::{CommandSpec, ProcessRunner};
        let mut fixture = crate::commands::test_support::CommandFixture::new().await?;
        fixture.ctx.interactive_processes = ProcessRunner::new(NonZeroUsize::new(1).unwrap());
        let action = wt_tui::UiAction::PrepareSessions {
            key: Some("one".into()),
            target: wt_tui::SessionTarget::Harness,
        };
        let first = fixture.ctx.for_action(&action)?;
        let (cancel, blocker) = hold_process_permit(first.processes).await;
        let second = fixture.ctx.clone().for_action(&action)?;
        let blocked = tokio::time::timeout(
            Duration::from_millis(100),
            second
                .processes
                .run(CommandSpec::new("true"), &second.cancellation),
        )
        .await
        .is_err();
        cancel.cancel();
        blocker.await?;
        fixture.close().await?;
        assert!(
            blocked,
            "new action contexts must not create per-click process pools"
        );
        Ok(())
    }

    #[test]
    fn a_recorded_section_write_is_consumed_once() {
        let writes = SectionWrites::default();
        writes.record(["a".to_owned()], Some("To Merge"));
        assert!(!writes.consume("a", None));
        assert!(writes.consume("a", Some("to merge")));
        assert!(
            !writes.consume("a", Some("To Merge")),
            "a second move is someone else's"
        );
        writes.record(["b".to_owned()], None);
        assert!(writes.consume("b", None));
    }
}
