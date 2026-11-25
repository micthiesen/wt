//! Deploy proof. Each configured `[environments.<stage>]` probe is polled for
//! the commit its stage is running, and the answer is recorded as
//! `refs/wt/deployed/<stage>` in the main clone. Everything that asks "is
//! this landing deployed" reads that ref (`wt_ops::deploys`), so the snapshot
//! published here only says when it moved and whether the probe is healthy.

use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use wt_config::EnvironmentConfig;
use wt_core::{Domain, VerifyStage};
use wt_platform::process::CommandSpec;
use wt_runtime::{LeafHandle, Persistence, SourceSpec, Stamp, TaskScope, leaf_channel};

use wt_app_core::context::AppContext;

/// A probe failure is reported once it has outlasted this many intervals: a
/// deploy in progress or a brief outage is not worth anyone's attention.
const FAILED_INTERVALS: u32 = 3;
/// A refresh request reruns a probe at most this often.
const MIN_REFRESH_GAP: Duration = Duration::from_secs(15);
const PROBE_OUTPUT_LIMIT: usize = 64 * 1024;
const MAX_ERROR_CHARS: usize = 240;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StageDeploy {
    /// The commit the stage's deployed ref holds: the last one its probe
    /// reported, from this run or an earlier one.
    pub oid: Option<String>,
    /// The probe's current failure, once it has lasted `FAILED_INTERVALS`.
    pub error: Option<String>,
}

/// What each probed stage is running. A stage without a probe stays empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeploySnapshot {
    pub staging: StageDeploy,
    pub production: StageDeploy,
}

impl DeploySnapshot {
    pub fn get(&self, stage: VerifyStage) -> &StageDeploy {
        match stage {
            VerifyStage::Staging => &self.staging,
            VerifyStage::Production => &self.production,
        }
    }

    fn set(&mut self, stage: VerifyStage, deploy: StageDeploy) {
        match stage {
            VerifyStage::Staging => self.staging = deploy,
            VerifyStage::Production => self.production = deploy,
        }
    }
}

struct Probe {
    stage: VerifyStage,
    config: EnvironmentConfig,
    deploy: StageDeploy,
    failures: u32,
    /// The attention line already written for the current failure.
    reported: Option<String>,
    /// A reported commit this clone lacked after fetching origin for it.
    unfetchable: Option<String>,
    next_at: Instant,
    last_run: Option<Instant>,
}

impl Probe {
    fn new(stage: VerifyStage, config: EnvironmentConfig) -> Self {
        Self {
            stage,
            config,
            deploy: StageDeploy::default(),
            failures: 0,
            reported: None,
            unfetchable: None,
            next_at: Instant::now(),
            last_run: None,
        }
    }

    async fn run(&mut self, context: &AppContext, cancel: &CancellationToken) {
        let observed = self.observe(context, cancel).await;
        if cancel.is_cancelled() {
            return;
        }
        let stage = self.stage.as_str();
        match observed {
            Ok(oid) => {
                if self.reported.take().is_some() {
                    tracing::info!(stage, "deploy probe recovered");
                }
                self.failures = 0;
                self.unfetchable = None;
                self.deploy = StageDeploy {
                    oid: Some(oid),
                    error: None,
                };
            }
            Err(error) => {
                self.failures = self.failures.saturating_add(1);
                tracing::debug!(stage, %error, failures = self.failures, "deploy probe failed");
                if self.failures < FAILED_INTERVALS {
                    return;
                }
                let text = format!("Deploy probe for {stage} is failing: {error}");
                if self.reported.as_deref() != Some(text.as_str()) {
                    tracing::warn!(
                        target: "wt_attention",
                        event_channel = "attention",
                        event_source = "deploy probe",
                        event_text = %text,
                        "deploy probe failing"
                    );
                    self.reported = Some(text);
                }
                self.deploy.error = Some(error);
            }
        }
    }

    /// Run the probe and record what it reports. The stage's ref moves only
    /// to a commit this clone holds.
    async fn observe(
        &mut self,
        context: &AppContext,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        let main_clone = &context.config.paths.main_clone;
        let (program, args) = self
            .config
            .deployed
            .split_first()
            .ok_or("no probe command is configured")?;
        let mut spec = CommandSpec::new(program).args(args).cwd(main_clone);
        spec.timeout = Duration::from_secs(self.config.timeout_seconds);
        spec.output_limit = PROBE_OUTPUT_LIMIT;
        let output = context
            .processes
            .run(spec, cancel)
            .await
            .map_err(|error| bounded(&error.to_string()))?;
        if !output.status.success() {
            let stderr = output.stderr_text();
            let detail = stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("no error output");
            return Err(bounded(&format!(
                "{program} exited with {}: {detail}",
                output
                    .status
                    .code()
                    .map_or_else(|| "a signal".to_owned(), |code| code.to_string()),
            )));
        }
        let stdout = output.stdout_text();
        let sha = wt_vcs::parse_probe_output(&stdout).map_err(|error| bounded(&error.0))?;
        let fetch = self.unfetchable.as_deref() != Some(sha);
        let oid = match wt_vcs::resolve_deployed_commit(
            &context.processes,
            main_clone,
            sha,
            fetch,
            cancel,
        )
        .await
        {
            Ok(oid) => oid,
            Err(error) => {
                self.unfetchable = Some(sha.to_owned());
                return Err(bounded(&error.0));
            }
        };
        wt_vcs::record_deployed(&context.processes, main_clone, self.stage, &oid, cancel)
            .await
            .map_err(|error| bounded(&error.0))?;
        Ok(oid)
    }
}

fn bounded(text: &str) -> String {
    let clean = wt_core::sanitize_terminal_text(text);
    if clean.chars().count() <= MAX_ERROR_CHARS {
        return clean;
    }
    let mut cut: String = clean.chars().take(MAX_ERROR_CHARS).collect();
    cut.push('…');
    cut
}

/// Start polling the configured probes. With none configured the leaf holds
/// an empty snapshot and nothing runs.
pub fn start(scope: &TaskScope, context: &AppContext) -> LeafHandle<DeploySnapshot> {
    let (source, mut publisher) = leaf_channel(
        scope,
        SourceSpec {
            name: "deploy-proof",
            // Perf is the domain the refresh motion ignores: a background
            // probe must never animate the title bar.
            domain: Domain::Perf,
            persistence: Persistence::Never("live probe output; the result is kept in Git refs"),
            policy: Default::default(),
        },
    );
    let cancellation = scope.token();
    let mut context = context.clone();
    context.cancellation = cancellation.clone();
    scope.spawn(async move {
        let mut probes = context
            .config
            .environments
            .probed()
            .map(|(stage, config)| Probe::new(stage, config.clone()))
            .collect::<Vec<_>>();
        let mut snapshot = DeploySnapshot::default();
        // A ref recorded by an earlier run is still what the stage ran last.
        for probe in &mut probes {
            probe.deploy.oid = wt_vcs::deployed_oid(
                &context.processes,
                &context.config.paths.main_clone,
                probe.stage,
                &cancellation,
            )
            .await
            .ok()
            .flatten();
            snapshot.set(probe.stage, probe.deploy.clone());
        }
        publisher.publish_live(snapshot.clone(), Stamp::now());
        if probes.is_empty() {
            // Hold the publisher: dropping it would read as shutdown.
            cancellation.cancelled().await;
            drop(publisher);
            return;
        }
        loop {
            let next = probes
                .iter()
                .map(|probe| probe.next_at)
                .min()
                .unwrap_or_else(Instant::now);
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    let now = Instant::now();
                    for probe in &mut probes {
                        if probe.last_run.is_none_or(|at| now.duration_since(at) >= MIN_REFRESH_GAP) {
                            probe.next_at = now;
                        }
                    }
                    continue;
                }
                _ = tokio::time::sleep_until(next) => {}
            }
            let observed = Stamp::now();
            let mut next_snapshot = snapshot.clone();
            for probe in &mut probes {
                if probe.next_at > Instant::now() {
                    continue;
                }
                let attempt = cancellation.child_token();
                probe.run(&context, &attempt).await;
                let finished = Instant::now();
                probe.last_run = Some(finished);
                probe.next_at = finished + Duration::from_secs(probe.config.every_seconds);
                next_snapshot.set(probe.stage, probe.deploy.clone());
            }
            if cancellation.is_cancelled() {
                break;
            }
            if next_snapshot != snapshot {
                snapshot = next_snapshot;
                publisher.publish_live(snapshot.clone(), observed);
            }
        }
    });
    source
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, sync::Arc};
    use wt_app_core::test_support::CommandFixture;

    async fn git(context: &AppContext, path: &Path, args: &[&str]) -> String {
        wt_ops::commands::resolve::run_git(context, path, args.iter().copied())
            .await
            .unwrap()
            .checked("fixture git")
            .unwrap()
            .stdout_text()
            .trim()
            .to_owned()
    }

    /// A probe that prints whatever `deployed.txt` holds, the way a real one
    /// would ask a platform what it is running.
    fn fake_probe(fixture: &mut CommandFixture, stage: VerifyStage) -> std::path::PathBuf {
        let answer = fixture._root.path().join(format!("{}.txt", stage.as_str()));
        let script = fixture._root.path().join(format!("{}.sh", stage.as_str()));
        std::fs::write(&script, format!("cat {:?}\n", answer.to_string_lossy())).unwrap();
        let probe = EnvironmentConfig {
            deployed: vec!["sh".into(), script.to_string_lossy().into_owned()],
            every_seconds: 1,
            timeout_seconds: 10,
        };
        let environments = &mut Arc::make_mut(&mut fixture.ctx.config).environments;
        match stage {
            VerifyStage::Staging => environments.staging = Some(probe),
            VerifyStage::Production => environments.production = Some(probe),
        }
        answer
    }

    #[tokio::test]
    async fn a_probe_moves_the_stage_ref_and_reports_only_lasting_failures() {
        let mut fixture = CommandFixture::new().await.unwrap();
        let answer = fake_probe(&mut fixture, VerifyStage::Staging);
        let context = fixture.ctx.clone();
        let main = context.config.paths.main_clone.clone();
        let cancel = CancellationToken::new();
        let recorded = || async {
            wt_vcs::deployed_oid(&context.processes, &main, VerifyStage::Staging, &cancel)
                .await
                .unwrap()
        };
        let base = git(&context, &main, &["rev-parse", "HEAD"]).await;
        let mut probe = Probe::new(
            VerifyStage::Staging,
            context.config.environments.staging.clone().unwrap(),
        );

        // An abbreviated SHA with trailing text resolves to the full commit.
        std::fs::write(&answer, format!("{} live since noon\n", &base[..9])).unwrap();
        probe.run(&context, &cancel).await;
        assert_eq!(probe.deploy.oid.as_deref(), Some(base.as_str()));
        assert_eq!(probe.deploy.error, None);
        assert_eq!(recorded().await.as_deref(), Some(base.as_str()));

        // A failure keeps the last proof and stays quiet for three intervals.
        std::fs::write(&answer, "deploying...\n").unwrap();
        for _ in 0..FAILED_INTERVALS - 1 {
            probe.run(&context, &cancel).await;
            assert_eq!(probe.deploy.error, None);
            assert!(probe.reported.is_none());
        }
        probe.run(&context, &cancel).await;
        let error = probe.deploy.error.clone().unwrap();
        assert!(error.contains("not a commit SHA"), "{error}");
        assert_eq!(probe.deploy.oid.as_deref(), Some(base.as_str()));
        assert_eq!(recorded().await.as_deref(), Some(base.as_str()));
        let first_report = probe.reported.clone().unwrap();
        assert!(first_report.starts_with("Deploy probe for staging is failing: "));
        // The same failure is not reported again; a different one is.
        probe.run(&context, &cancel).await;
        assert_eq!(probe.reported.as_deref(), Some(first_report.as_str()));
        // A commit nobody has is an error, never a silent "not deployed".
        std::fs::write(&answer, "f".repeat(40)).unwrap();
        probe.run(&context, &cancel).await;
        let error = probe.deploy.error.clone().unwrap();
        assert!(error.contains("is not in this repository"), "{error}");
        assert_ne!(probe.reported.as_deref(), Some(first_report.as_str()));
        assert_eq!(probe.unfetchable.as_deref(), Some("f".repeat(40).as_str()));
        std::fs::remove_file(&answer).unwrap();
        probe.run(&context, &cancel).await;
        let error = probe.deploy.error.clone().unwrap();
        assert!(error.contains("sh exited with"), "{error}");

        // Recovery clears the error, and the ref follows the new deploy.
        let worktree = context.cwd.clone();
        std::fs::write(worktree.join("shipped.txt"), "shipped\n").unwrap();
        git(&context, &worktree, &["add", "shipped.txt"]).await;
        git(&context, &worktree, &["commit", "-m", "shipped"]).await;
        let shipped = git(&context, &worktree, &["rev-parse", "HEAD"]).await;
        std::fs::write(&answer, &shipped).unwrap();
        probe.run(&context, &cancel).await;
        assert_eq!(
            probe.deploy,
            StageDeploy {
                oid: Some(shipped.clone()),
                error: None
            }
        );
        assert!(probe.reported.is_none() && probe.failures == 0);
        assert_eq!(recorded().await.as_deref(), Some(shipped.as_str()));
        assert_eq!(
            wt_ops::deploys::landing_deployed(
                &context,
                VerifyStage::Staging,
                &[base.as_str()],
                &cancel
            )
            .await,
            Some(true)
        );
        // The unprobed stage is not tracked at all.
        assert_eq!(
            wt_ops::deploys::landing_deployed(
                &context,
                VerifyStage::Production,
                &[base.as_str()],
                &cancel
            )
            .await,
            None
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn the_source_publishes_each_stage_as_its_probe_answers() {
        let mut fixture = CommandFixture::new().await.unwrap();
        let answer = fake_probe(&mut fixture, VerifyStage::Production);
        let context = fixture.ctx.clone();
        let main = context.config.paths.main_clone.clone();
        let base = git(&context, &main, &["rev-parse", "HEAD"]).await;
        std::fs::write(&answer, &base).unwrap();
        let scope = TaskScope::new();
        let deploys = start(&scope, &context);
        let mut updates = deploys.view().subscribe();
        let published = tokio::time::timeout(
            Duration::from_secs(20),
            updates.wait_for(|snapshot| {
                snapshot
                    .data()
                    .is_some_and(|deploys| deploys.production.oid.is_some())
            }),
        )
        .await
        .expect("the first probe runs at start")
        .unwrap()
        .data()
        .unwrap()
        .as_ref()
        .clone();
        assert_eq!(published.production.oid.as_deref(), Some(base.as_str()));
        assert_eq!(published.staging, StageDeploy::default());
        assert!(deploys.confirmed().is_some());
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
        fixture.close().await.unwrap();
    }
}
