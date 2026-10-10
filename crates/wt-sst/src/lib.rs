//! SST state inspection and namespace-safe local stage ownership.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use futures_util::{StreamExt, stream};
use serde::Serialize;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_config::{SstConfig, StageConfig};
use wt_platform::process::{CommandSpec, ProcessError, ProcessRunner};

const AWS_TIMEOUT: Duration = Duration::from_secs(60);
const AWS_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
const STATE_PROBE_CONCURRENCY: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SstStage {
    pub name: String,
    pub size_bytes: u64,
    pub modified: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnknownStage {
    #[serde(flatten)]
    pub stage: SstStage,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct StageInventory {
    pub live: Vec<SstStage>,
    pub orphaned: Vec<SstStage>,
    /// Namespaced stages whose state could not be read or understood. They
    /// are shown to operators and are never eligible for automatic cleanup.
    pub unknown: Vec<UnknownStage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeploymentObservation {
    Deployed {
        stage: String,
    },
    NotDeployed {
        stage: Option<String>,
        reason: String,
    },
    Unknown {
        stage: Option<String>,
        reason: String,
    },
}

#[derive(Debug, Error)]
pub enum LocalStageError {
    #[error("stage prefix is not configured")]
    MissingPrefix,
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("pinned stage {stage:?} is outside configured namespace {prefix:?}")]
    Foreign { stage: String, prefix: String },
    #[error("pinned stage {0:?} is not a valid stage name")]
    InvalidName(String),
}

#[derive(Debug, Error)]
pub enum SstError {
    #[error("SST is not configured")]
    NotConfigured,
    #[error("SST operation cancelled")]
    Cancelled,
    #[error("refusing to inspect protected or out-of-namespace stage {0:?}")]
    ProtectedStage(String),
    #[error("AWS S3 {operation} failed: {source}")]
    Process {
        operation: &'static str,
        #[source]
        source: ProcessError,
    },
    #[error("AWS S3 {operation} returned invalid UTF-8")]
    InvalidUtf8 { operation: &'static str },
    #[error("AWS S3 {operation} failed: {stderr}")]
    CommandFailed {
        operation: &'static str,
        stderr: String,
    },
}

#[derive(Clone, Debug)]
pub struct SstPrograms {
    pub aws: OsString,
}

impl Default for SstPrograms {
    fn default() -> Self {
        Self { aws: "aws".into() }
    }
}

/// AWS state access is bounded by `ProcessRunner`; probes are additionally
/// limited to four concurrent state downloads.
#[derive(Clone)]
pub struct SstService {
    sst: SstConfig,
    stage: StageConfig,
    runner: ProcessRunner,
    programs: SstPrograms,
}

impl SstService {
    pub fn new(sst: SstConfig, stage: StageConfig, runner: ProcessRunner) -> Self {
        Self {
            sst,
            stage,
            runner,
            programs: SstPrograms::default(),
        }
    }

    /// Executable injection keeps fixture tests isolated from credentials and
    /// the user's AWS configuration. Production callers should use `new`.
    pub fn with_programs(mut self, programs: SstPrograms) -> Self {
        self.programs = programs;
        self
    }

    pub fn stage_config(&self) -> &StageConfig {
        &self.stage
    }

    pub async fn list(&self, cancellation: &CancellationToken) -> Result<Vec<SstStage>, SstError> {
        let bucket = self.sst.state_bucket.trim_matches('/');
        let prefix = self.sst.state_prefix.trim_start_matches('/');
        let output = self
            .aws_s3(
                &["ls".into(), format!("s3://{bucket}/{prefix}").into()],
                cancellation,
                "list stages",
            )
            .await?;
        let text = std::str::from_utf8(&output).map_err(|_| SstError::InvalidUtf8 {
            operation: "list stages",
        })?;
        let mut stages = text
            .lines()
            .filter_map(parse_stage_line)
            .collect::<Vec<_>>();
        stages.sort_by(|a, b| a.name.cmp(&b.name));
        stages.dedup_by(|a, b| a.name == b.name);
        Ok(stages)
    }

    pub async fn categorize(
        &self,
        stages: Vec<SstStage>,
        live_worktree_stages: &BTreeSet<String>,
        cancellation: &CancellationToken,
    ) -> Result<StageInventory, SstError> {
        let mut inventory = StageInventory::default();
        let mut candidates = Vec::new();
        for stage in stages {
            if stage.name == self.stage.default_personal
                || !stage_name_in_namespace(&stage.name, &self.stage.prefix)
            {
                continue;
            }
            if live_worktree_stages.contains(&stage.name) {
                inventory.live.push(stage);
            } else {
                candidates.push(stage);
            }
        }
        let service = self.clone();
        let mut probes = stream::iter(candidates.into_iter().map(move |stage| {
            let service = service.clone();
            let token = cancellation.child_token();
            async move {
                (
                    stage.clone(),
                    service.stage_resources(&stage.name, &token).await,
                )
            }
        }))
        .buffer_unordered(STATE_PROBE_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
        if cancellation.is_cancelled() {
            return Err(SstError::Cancelled);
        }
        probes.sort_by(|(a, _), (b, _)| b.modified.cmp(&a.modified));
        for (stage, result) in probes {
            match result {
                Ok(true) => inventory.orphaned.push(stage),
                Ok(false) => {} // Empty SST state is the residue of a prior remove.
                Err(error) => inventory.unknown.push(UnknownStage {
                    stage,
                    reason: error.to_string(),
                }),
            }
        }
        inventory.live.sort_by(|a, b| a.name.cmp(&b.name));
        inventory
            .orphaned
            .sort_by(|a, b| b.modified.cmp(&a.modified));
        inventory
            .unknown
            .sort_by(|a, b| b.stage.modified.cmp(&a.stage.modified));
        Ok(inventory)
    }

    /// Rechecks S3 state and refuses to classify errors as an empty stage.
    pub async fn stage_resources(
        &self,
        stage: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, SstError> {
        if !stage_name_in_namespace(stage, &self.stage.prefix)
            || stage == self.stage.default_personal
        {
            return Err(SstError::ProtectedStage(stage.to_owned()));
        }
        let bucket = self.sst.state_bucket.trim_matches('/');
        let prefix = self.sst.state_prefix.trim_start_matches('/');
        let source = format!("s3://{bucket}/{prefix}{stage}.json");
        let bytes = self
            .aws_s3(
                &["cp".into(), source.into(), "-".into()],
                cancellation,
                "read stage state",
            )
            .await?;
        let state: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|error| SstError::CommandFailed {
                operation: "read stage state",
                stderr: format!("stage {stage}: invalid JSON state: {error}"),
            })?;
        let resources = state
            .get("checkpoint")
            .and_then(|value| value.get("latest"))
            .and_then(|value| value.get("resources"))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| SstError::CommandFailed {
                operation: "read stage state",
                stderr: format!("stage {stage}: checkpoint.latest.resources is missing or invalid"),
            })?;
        Ok(!resources.is_empty())
    }

    async fn aws_s3(
        &self,
        args: &[OsString],
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<Vec<u8>, SstError> {
        if self.sst.state_bucket.trim().is_empty() {
            return Err(SstError::NotConfigured);
        }
        let mut argv = vec![OsString::from("s3")];
        argv.extend(args.iter().cloned());
        argv.extend([
            OsString::from("--profile"),
            OsString::from(&self.sst.aws_profile),
        ]);
        let mut spec = CommandSpec::new(self.programs.aws.clone())
            .args(argv)
            .cwd(Path::new("."));
        spec.timeout = AWS_TIMEOUT;
        spec.output_limit = AWS_OUTPUT_LIMIT;
        let output = self
            .runner
            .run(spec, cancellation)
            .await
            .map_err(|source| SstError::Process { operation, source })?;
        if !output.status.success() {
            return Err(SstError::CommandFailed {
                operation,
                stderr: output.stderr_text().trim().to_owned(),
            });
        }
        Ok(output.stdout)
    }
}

fn parse_stage_line(line: &str) -> Option<SstStage> {
    let mut parts = line.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let size = parts.next()?.parse::<u64>().ok()?;
    let key = parts.next()?;
    if !key.ends_with(".json") {
        return None;
    }
    let name = key.strip_suffix(".json")?;
    if !valid_stage_name(name) {
        return None;
    }
    Some(SstStage {
        name: name.to_owned(),
        size_bytes: size,
        modified: format!("{date}T{time}Z"),
    })
}

pub fn valid_stage_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub fn stage_name_in_namespace(name: &str, prefix: &str) -> bool {
    !prefix.is_empty() && valid_stage_name(name) && name.starts_with(prefix)
}

pub fn safe_pinned_stage(path: &Path, prefix: &str) -> Result<String, LocalStageError> {
    if prefix.is_empty() {
        return Err(LocalStageError::MissingPrefix);
    }
    let file = path.join(".sst/stage");
    let stage = std::fs::read_to_string(&file)
        .map_err(|source| LocalStageError::Read { path: file, source })?
        .trim()
        .to_owned();
    if !stage.starts_with(prefix) {
        return Err(LocalStageError::Foreign {
            stage,
            prefix: prefix.to_owned(),
        });
    }
    if !valid_stage_name(&stage) {
        return Err(LocalStageError::InvalidName(stage));
    }
    Ok(stage)
}

/// Observe the local deployment facts without collapsing unreadable files
/// into a negative answer. Destructive callers must proceed only on `Deployed`
/// and still check stage namespace and live inventory at the action boundary.
pub fn observe_local_deployment(path: &Path, prefix: &str) -> DeploymentObservation {
    let pin_path = path.join(".sst/stage");
    let stage = match std::fs::read_to_string(&pin_path) {
        Ok(value) if value.trim().is_empty() => {
            return DeploymentObservation::NotDeployed {
                stage: None,
                reason: "stage pin is empty".into(),
            };
        }
        Ok(value) => value.trim().to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return DeploymentObservation::NotDeployed {
                stage: None,
                reason: "no .sst/stage pin".into(),
            };
        }
        Err(error) => {
            return DeploymentObservation::Unknown {
                stage: None,
                reason: format!("could not read {}: {error}", pin_path.display()),
            };
        }
    };
    if prefix.is_empty() || !stage.starts_with(prefix) || !valid_stage_name(&stage) {
        return DeploymentObservation::NotDeployed {
            stage: Some(stage),
            reason: "stage pin is outside the configured namespace or invalid".into(),
        };
    }
    let outputs_path = path.join(".sst/outputs.json");
    let outputs = match std::fs::read_to_string(&outputs_path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return DeploymentObservation::NotDeployed {
                stage: Some(stage),
                reason: "no .sst/outputs.json".into(),
            };
        }
        Err(error) => {
            return DeploymentObservation::Unknown {
                stage: Some(stage),
                reason: format!("could not read {}: {error}", outputs_path.display()),
            };
        }
    };
    if serde_json::from_str::<serde_json::Value>(&outputs).is_err() {
        return DeploymentObservation::Unknown {
            stage: Some(stage),
            reason: "outputs file is not valid JSON".into(),
        };
    }
    if outputs.contains(&stage) {
        DeploymentObservation::Deployed { stage }
    } else {
        DeploymentObservation::NotDeployed {
            stage: Some(stage),
            reason: "outputs do not reference the pinned stage".into(),
        }
    }
}

/// File-backed ownership observation for async callers. Disk access runs on
/// Tokio's blocking pool so an input or render task never performs SST IO.
pub async fn observe_local_deployment_async(
    path: PathBuf,
    prefix: String,
) -> Result<DeploymentObservation, tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || observe_local_deployment(&path, &prefix)).await
}

pub fn preview_url(stage: &str, prefix: &str, domain: Option<&str>) -> Option<String> {
    if !stage_name_in_namespace(stage, prefix) {
        return None;
    }
    let domain = domain?.trim().trim_matches('.');
    if domain.is_empty()
        || domain.contains('/')
        || domain.contains(':')
        || domain.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(format!("https://{stage}.{domain}"))
}

pub fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    #[test]
    fn parses_only_valid_s3_stage_listing_lines() {
        let parsed = parse_stage_line("2026-09-10 12:34:56 123 personal-feature.json").unwrap();
        assert_eq!(parsed.name, "personal-feature");
        assert_eq!(parsed.modified, "2026-09-10T12:34:56Z");
        assert_eq!(parsed.size_bytes, 123);
        assert!(parse_stage_line("PRE folder/").is_none());
        assert!(parse_stage_line("2026-09-10 12:34:56 nope personal-x.json").is_none());
        assert!(parse_stage_line("2026-09-10 12:34:56 1 ../prod.json").is_none());
    }

    #[test]
    fn stage_url_and_local_ownership_fail_closed() {
        assert_eq!(
            preview_url("m-feature", "m-", Some("preview.example")),
            Some("https://m-feature.preview.example".into())
        );
        assert_eq!(
            preview_url("production", "m-", Some("preview.example")),
            None
        );
        assert_eq!(preview_url("m-a", "m-", Some("https://evil.example")), None);

        let dir = tempfile::tempdir().unwrap();
        let sst = dir.path().join(".sst");
        std::fs::create_dir(&sst).unwrap();
        std::fs::write(sst.join("stage"), "m-feature\n").unwrap();
        std::fs::write(
            sst.join("outputs.json"),
            r#"{"url":"https://m-feature.preview.example"}"#,
        )
        .unwrap();
        assert_eq!(
            observe_local_deployment(dir.path(), "m-"),
            DeploymentObservation::Deployed {
                stage: "m-feature".into()
            }
        );
        std::fs::write(sst.join("outputs.json"), "not json").unwrap();
        assert!(matches!(
            observe_local_deployment(dir.path(), "m-"),
            DeploymentObservation::Unknown { .. }
        ));
        std::fs::write(sst.join("stage"), "production").unwrap();
        assert!(matches!(
            observe_local_deployment(dir.path(), "m-"),
            DeploymentObservation::NotDeployed { .. }
        ));
    }

    #[tokio::test]
    async fn categorization_protects_default_and_foreign_and_marks_failed_state_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let aws = tmp.path().join("aws");
        std::fs::write(&aws, "#!/bin/sh\necho 'state unavailable' >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let service = SstService::new(
            SstConfig {
                state_bucket: "bucket".into(),
                state_prefix: "apps/".into(),
                aws_profile: "test".into(),
                auto_regen_paths: vec![],
            },
            StageConfig {
                prefix: "m-".into(),
                default_personal: "m-personal".into(),
                domain: None,
            },
            ProcessRunner::new(NonZeroUsize::new(4).unwrap()),
        )
        .with_programs(SstPrograms {
            aws: aws.into_os_string(),
        });
        assert!(matches!(
            service
                .stage_resources("foreign-stage", &CancellationToken::new())
                .await,
            Err(SstError::ProtectedStage(_))
        ));
        assert!(matches!(
            service
                .stage_resources("m-personal", &CancellationToken::new())
                .await,
            Err(SstError::ProtectedStage(_))
        ));
        let stages = ["m-live", "m-orphan", "m-personal", "foreign"]
            .into_iter()
            .map(|name| SstStage {
                name: name.into(),
                size_bytes: 1,
                modified: "2026-01-01T00:00:00Z".into(),
            })
            .collect();
        let inventory = service
            .categorize(
                stages,
                &BTreeSet::from(["m-live".into()]),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(inventory.live.len(), 1);
        assert!(inventory.orphaned.is_empty());
        assert_eq!(inventory.unknown.len(), 1);
        assert_eq!(inventory.unknown[0].stage.name, "m-orphan");
    }

    #[tokio::test]
    async fn categorization_distinguishes_resources_empty_state_and_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let aws = tmp.path().join("aws");
        std::fs::write(
            &aws,
            "#!/bin/sh\ncase \"$3\" in\n  *m-active.json) printf '%s' '{\"checkpoint\":{\"latest\":{\"resources\":[{\"urn\":\"fixture\"}]}}}' ;;\n  *m-empty.json) printf '%s' '{\"checkpoint\":{\"latest\":{\"resources\":[]}}}' ;;\n  *m-malformed.json) printf '%s' '{broken' ;;\n  *) exit 9 ;;\nesac\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let service = SstService::new(
            SstConfig {
                state_bucket: "bucket".into(),
                state_prefix: "apps/".into(),
                aws_profile: "test".into(),
                auto_regen_paths: vec![],
            },
            StageConfig {
                prefix: "m-".into(),
                default_personal: "m-personal".into(),
                domain: None,
            },
            ProcessRunner::new(NonZeroUsize::new(4).unwrap()),
        )
        .with_programs(SstPrograms {
            aws: aws.into_os_string(),
        });
        let stages = [
            "m-active",
            "m-empty",
            "m-malformed",
            "m-personal",
            "foreign",
        ]
        .into_iter()
        .map(|name| SstStage {
            name: name.into(),
            size_bytes: 20,
            modified: "2026-01-01T00:00:00Z".into(),
        })
        .collect();
        let inventory = service
            .categorize(stages, &BTreeSet::new(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            inventory
                .orphaned
                .iter()
                .map(|stage| stage.name.as_str())
                .collect::<Vec<_>>(),
            ["m-active"]
        );
        assert_eq!(
            inventory
                .unknown
                .iter()
                .map(|stage| stage.stage.name.as_str())
                .collect::<Vec<_>>(),
            ["m-malformed"]
        );
    }

    #[tokio::test]
    async fn cancellation_during_state_probes_is_not_reported_as_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let aws = tmp.path().join("aws");
        std::fs::write(&aws, "#!/bin/sh\nsleep 2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let service = SstService::new(
            SstConfig {
                state_bucket: "bucket".into(),
                state_prefix: "apps/".into(),
                aws_profile: "test".into(),
                auto_regen_paths: vec![],
            },
            StageConfig {
                prefix: "m-".into(),
                default_personal: "m-personal".into(),
                domain: None,
            },
            ProcessRunner::new(NonZeroUsize::new(4).unwrap()),
        )
        .with_programs(SstPrograms {
            aws: aws.into_os_string(),
        });
        let token = CancellationToken::new();
        let cancel = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            cancel.cancel();
        });
        let stages = vec![SstStage {
            name: "m-probe".into(),
            size_bytes: 1,
            modified: "2026-01-01T00:00:00Z".into(),
        }];
        let result = service.categorize(stages, &BTreeSet::new(), &token).await;
        assert!(matches!(result, Err(SstError::Cancelled)));
    }
}
