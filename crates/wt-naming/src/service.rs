use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use wt_config::NamingConfig;
use wt_core::HarnessId;
use wt_platform::process::{CommandSpec, ProcessError, ProcessRunner};

use crate::completion::{
    CompletionSpec, HarnessCompletionError, HarnessPrograms, build_completion_spec,
    is_stack_title_meta_only, parse_stack_title, parse_title_description,
};
use crate::diff::{DiffContextError, build_diff_context};
use crate::{AiSummary, StackMember};

const SUMMARY_SYSTEM: &str = "You summarise git changes for a developer scanning their worktrees.\n\nOutput format, exactly:\nTITLE: <one concise, natural title for the work>\nDESCRIPTION: <1 to 3 sentences of plain prose>\n\nRules:\n- TITLE: short and specific, like a good PR title. Use only the words needed to identify the work. Natural wording, no quotes or trailing period. Examples: \"Move files to R2\", \"Fix reviewer picker\", \"Add auto-merge support\".\n- DESCRIPTION: describe what the change does, not which files it touches. No markdown, no headings, no lists.\n- Skip filler like \"This change...\" or \"The diff shows...\". Lead with the action.\n- If the changes feel exploratory or scaffolding, say so.\n\nReturn only the formatted output. Nothing before TITLE, nothing after the description.";

const STACK_SYSTEM: &str = "You name a group of related git branches for a section header in a developer tool.\n\nOutput exactly:\nTITLE: <name>\n\nRules:\n- Find the common theme — what unifies the branches. Often a feature, subsystem, or area they all touch.\n- TITLE: 4 words maximum. Caveman noun phrase, no leading verb (\"Add\", \"Fix\", \"Refactor\"...), no articles, no quotes, no trailing period.\n- Examples: \"Auto-merge support\", \"Markdown link popover\", \"Reviewer picker UI\", \"Atomic builder claim\".\n- Name the WORK, not its packaging: never echo words from these instructions (\"stack\", \"branch\", \"section\", \"header\", \"TUI\", \"group\") unless the changes themselves are about that concept.\n- If the branches look unrelated, pick the most prominent shared theme rather than listing them.\n\nReturn only the TITLE line.";

#[derive(Clone, Debug)]
pub struct NamingServiceConfig {
    pub naming: NamingConfig,
    pub primary_harness: HarnessId,
    pub main_clone: PathBuf,
    pub trunk_branch: String,
    pub programs: HarnessPrograms,
}

#[derive(Clone)]
pub struct NamingService {
    config: Arc<NamingServiceConfig>,
    runner: ProcessRunner,
    permits: Arc<Semaphore>,
}

#[derive(Debug, Error)]
pub enum NamingError {
    #[error("naming completion: {0}")]
    Completion(#[from] HarnessCompletionError),
    #[error(transparent)]
    Diff(#[from] DiffContextError),
    #[error("naming harness {harness} failed: {detail}")]
    Harness {
        harness: &'static str,
        detail: String,
    },
    #[error("naming was cancelled")]
    Cancelled,
    #[error("stack title was only meta-vocabulary after two attempts: {0}")]
    InvalidStackTitle(String),
    #[error("stack context exceeded bounded input limits")]
    StackContextTooLarge,
}

impl NamingService {
    /// The service is intended to be shared by all app observers so naming
    /// calls remain globally serialized across rows and stack sections.
    pub fn new(config: NamingServiceConfig, runner: ProcessRunner) -> Result<Self, NamingError> {
        // Validate numeric configuration eagerly, before work can be queued.
        let _ = build_completion_spec(
            &config.naming,
            config.primary_harness,
            "",
            &config.main_clone,
            &config.programs,
        )?;
        Ok(Self {
            config: Arc::new(config),
            runner,
            permits: Arc::new(Semaphore::new(1)),
        })
    }

    pub fn config(&self) -> &NamingServiceConfig {
        &self.config
    }

    pub async fn diff_context(
        &self,
        repo: impl AsRef<std::path::Path>,
        effective_base: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Option<crate::DiffContext>, NamingError> {
        let base = effective_base
            .map(str::to_owned)
            .unwrap_or_else(|| format!("origin/{}", self.config.trunk_branch));
        Ok(build_diff_context(
            repo,
            &base,
            self.config.naming.max_input_tokens,
            &self.runner,
            cancellation,
        )
        .await?)
    }

    pub async fn summarize_diff(
        &self,
        prompt: &str,
        cancellation: &CancellationToken,
    ) -> Result<AiSummary, NamingError> {
        let _permit = self.acquire(cancellation).await?;
        let combined = format!("{SUMMARY_SYSTEM}\n\nINPUT:\n{prompt}");
        let output = self.complete(&combined, cancellation).await?;
        Ok(parse_title_description(&output))
    }

    pub async fn summarize_stack(
        &self,
        members: &[StackMember],
        cancellation: &CancellationToken,
    ) -> Result<String, NamingError> {
        if members.len() > 1000
            || members
                .iter()
                .map(|m| m.branch.len().saturating_add(m.title.len()))
                .sum::<usize>()
                > 64 * 1024
        {
            return Err(NamingError::StackContextTooLarge);
        }
        let user_prompt = format!(
            "Branches in this stack:\n{}",
            members
                .iter()
                .map(|m| format!("- {}: {}", m.branch, m.title))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let mut rejected = None;
        for attempt in 0..2 {
            let mut prompt = format!("{STACK_SYSTEM}\n\nINPUT:\n{user_prompt}");
            if let Some(previous) = &rejected {
                prompt.push_str(&format!("\n\nYour previous answer \"{previous}\" just echoed words from the instructions. Name the actual WORK these branches do, not the tool or the grouping."));
            }
            let _permit = self.acquire(cancellation).await?;
            let output = self.complete(&prompt, cancellation).await?;
            let title = parse_stack_title(&output).unwrap_or_default();
            if !title.is_empty() && !is_stack_title_meta_only(&title) {
                return Ok(title);
            }
            rejected = Some(if title.is_empty() {
                output.trim().to_string()
            } else {
                title
            });
            if attempt == 0 {
                sleep_cancellable(Duration::from_millis(500), cancellation).await?;
            }
        }
        Err(NamingError::InvalidStackTitle(rejected.unwrap_or_default()))
    }

    async fn acquire(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, NamingError> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(NamingError::Cancelled),
            permit = self.permits.clone().acquire_owned() => permit.map_err(|_| NamingError::Cancelled),
        }
    }

    async fn complete(
        &self,
        prompt: &str,
        cancellation: &CancellationToken,
    ) -> Result<String, NamingError> {
        let spec = build_completion_spec(
            &self.config.naming,
            self.config.primary_harness,
            prompt,
            &self.config.main_clone,
            &self.config.programs,
        )?;
        let mut last_error = None;
        for attempt in 0..2 {
            match self.complete_once(&spec, cancellation).await {
                Ok(output) => return Ok(output),
                Err(NamingError::Cancelled) => return Err(NamingError::Cancelled),
                Err(error) => last_error = Some(error),
            }
            if attempt == 0 {
                sleep_cancellable(Duration::from_millis(500), cancellation).await?;
            }
        }
        Err(last_error.expect("two completion attempts always set an error"))
    }

    async fn complete_once(
        &self,
        spec: &CompletionSpec,
        cancellation: &CancellationToken,
    ) -> Result<String, NamingError> {
        let mut command = CommandSpec::new(spec.program.clone())
            .args(spec.args.clone())
            .cwd(spec.cwd.clone());
        command.timeout = spec.timeout;
        command.output_limit = 1024 * 1024;
        command.input = spec.input.clone();
        let output = self
            .runner
            .run(command, cancellation)
            .await
            .map_err(|err| process_error(spec.harness_id, err))?;
        if !output.status.success() {
            let detail = brief_detail(if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            });
            return Err(NamingError::Harness {
                harness: spec.harness_id.as_str(),
                detail: format!("exit {:?}: {detail}", output.status.code()),
            });
        }
        let text = output.stdout_text().trim().to_string();
        if text.is_empty() {
            return Err(NamingError::Harness {
                harness: spec.harness_id.as_str(),
                detail: "returned no content".into(),
            });
        }
        Ok(text)
    }
}

fn process_error(harness: HarnessId, error: ProcessError) -> NamingError {
    if matches!(error, ProcessError::Cancelled { .. }) {
        NamingError::Cancelled
    } else {
        NamingError::Harness {
            harness: harness.as_str(),
            detail: error.to_string(),
        }
    }
}

fn brief_detail(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}

async fn sleep_cancellable(
    duration: Duration,
    cancellation: &CancellationToken,
) -> Result<(), NamingError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(NamingError::Cancelled),
        _ = tokio::time::sleep(duration) => Ok(()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use wt_config::{NamingHarness, NamingReasoningEffort};

    #[tokio::test]
    async fn fake_installed_harness_returns_parsed_summary_without_model_api() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake-claude");
        std::fs::write(&fake, "#!/bin/sh\ncat >/dev/null\nprintf 'TITLE: Keep prior summary.\\nDESCRIPTION: Uses the isolated fake executable.\\n'\n").unwrap();
        let mut permissions = std::fs::metadata(&fake).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).unwrap();
        let naming = NamingConfig {
            harness: NamingHarness::Claude,
            reasoning_effort: NamingReasoningEffort::Low,
            ..NamingConfig::default()
        };
        let config = NamingServiceConfig {
            naming,
            primary_harness: HarnessId::Codex,
            main_clone: dir.path().to_path_buf(),
            trunk_branch: "main".into(),
            programs: HarnessPrograms {
                claude: fake.into_os_string(),
                ..HarnessPrograms::default()
            },
        };
        let service = NamingService::new(
            config,
            ProcessRunner::new(std::num::NonZeroUsize::new(1).unwrap()),
        )
        .unwrap();
        let result = service
            .summarize_diff("prompt", &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.title.as_deref(), Some("Keep prior summary"));
        assert_eq!(result.description, "Uses the isolated fake executable.");
    }
}
