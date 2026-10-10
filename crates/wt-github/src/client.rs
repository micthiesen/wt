use std::{
    collections::HashSet,
    ffi::OsString,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{StreamExt, TryStreamExt, stream};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use wt_config::{Config, ReviewBotMode};
use wt_platform::process::{CommandSpec, ProcessOutput, ProcessRunner};

use crate::{
    checks::{
        CHUNK_SIZE, checks_still_pending, chunk_branches, is_rate_limit, missing_workflow_scope,
        not_yet_enqueueable, output_failure, rollup_checks,
    },
    types::*,
};

const GH_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_ATTEMPTS: usize = 3;
const MAX_CHUNK_CONCURRENCY: usize = 4;
const WORKFLOW_SCOPE_REMEDY: &str = "This is the LOCAL gh token, not the PR: run `gh auth refresh -h github.com -s workflow` (then `gh auth status` to confirm), and try again. Retrying as-is never clears it.";
const PICKER_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
type PickerCache<T> = Arc<AsyncMutex<Option<(tokio::time::Instant, T)>>>;

#[derive(Clone, Debug)]
pub struct GithubOptions {
    pub base_branch: String,
    pub reviewers_enabled: bool,
    pub ignored_checks: Vec<String>,
    pub review_bot_name: String,
    pub review_bot_login: String,
    pub review_bot_contexts: Vec<String>,
    pub review_bot_mode: ReviewBotMode,
    pub review_bot_summary_markers: Vec<String>,
    pub review_bot_pending_marker: Option<String>,
    pub ignored_review_repositories: Vec<String>,
    pub default_reviewer: Option<String>,
    pub repo_has_ci: bool,
}

impl GithubOptions {
    pub fn from_config(config: &Config, repo_has_ci: bool) -> Self {
        Self {
            base_branch: config.branch.base.clone(),
            reviewers_enabled: config.github.reviewers,
            ignored_checks: config.github.ignored_checks.clone(),
            review_bot_name: config.review_bot.name.clone(),
            review_bot_login: config.review_bot.login.clone(),
            review_bot_contexts: config.review_bot.check_contexts.clone(),
            review_bot_mode: config.review_bot.unresolved_via,
            review_bot_summary_markers: config.review_bot.summary_marker.clone(),
            review_bot_pending_marker: config.review_bot.pending_marker.clone(),
            ignored_review_repositories: config.github.ignored_review_repositories.clone(),
            default_reviewer: config.github.default_reviewer.clone(),
            repo_has_ci,
        }
    }
}

#[derive(Clone)]
pub struct GithubClient {
    processes: ProcessRunner,
    cwd: PathBuf,
    options: GithubOptions,
    repo: Option<RepoSlug>,
    gh_program: OsString,
    merge_requests: Arc<Mutex<HashSet<u64>>>,
    viewer_login_cache: PickerCache<String>,
    contributors_cache: PickerCache<Vec<Contributor>>,
}

impl GithubClient {
    pub fn new(processes: ProcessRunner, cwd: PathBuf, options: GithubOptions) -> Self {
        Self {
            processes,
            cwd,
            options,
            repo: None,
            gh_program: "gh".into(),
            merge_requests: Arc::new(Mutex::new(HashSet::new())),
            viewer_login_cache: Arc::new(AsyncMutex::new(None)),
            contributors_cache: Arc::new(AsyncMutex::new(None)),
        }
    }

    pub fn with_repository(mut self, repo: RepoSlug) -> Self {
        self.repo = Some(repo);
        self
    }

    /// Set an explicit executable, primarily for isolated transport fixtures.
    pub fn with_gh_program(mut self, program: impl Into<OsString>) -> Self {
        self.gh_program = program.into();
        self
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// Forget picker-only identity and contributor data after an explicit
    /// user refresh. Cached GitHub query data is owned by the source layer.
    pub async fn invalidate_picker_cache(&self) {
        *self.viewer_login_cache.lock().await = None;
        *self.contributors_cache.lock().await = None;
    }

    async fn run(
        &self,
        args: impl IntoIterator<Item = impl Into<std::ffi::OsString>>,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, GithubError> {
        let mut spec = CommandSpec::new(self.gh_program.clone())
            .args(args)
            .cwd(self.cwd.clone());
        spec.timeout = GH_TIMEOUT;
        self.processes
            .run(spec, cancellation)
            .await
            .map_err(|e| match e {
                wt_platform::process::ProcessError::Io { source, .. }
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    GithubError::MissingCli
                }
                wt_platform::process::ProcessError::Timeout { .. } => GithubError::Transient {
                    message: e.to_string(),
                },
                wt_platform::process::ProcessError::Cancelled { .. } => GithubError::Transient {
                    message: "GitHub request cancelled".into(),
                },
                _ => GithubError::Command(e.to_string()),
            })
    }

    pub async fn repository(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<RepoSlug, GithubError> {
        if let Some(repo) = &self.repo {
            return Ok(repo.clone());
        }
        let out = self
            .run(["repo", "view", "--json", "nameWithOwner"], cancellation)
            .await?;
        if !out.status.success() {
            return Err(GithubError::Repository(output_failure(&out).0));
        }
        let data: Value = serde_json::from_slice(&out.stdout).map_err(|e| {
            GithubError::Protocol(format!("unparseable gh repo view response: {e}"))
        })?;
        let slug = data
            .get("nameWithOwner")
            .and_then(Value::as_str)
            .ok_or_else(|| GithubError::Protocol("gh repo view omitted nameWithOwner".into()))?;
        RepoSlug::parse(slug)
            .ok_or_else(|| GithubError::Protocol(format!("invalid repository name {slug:?}")))
    }

    pub async fn authenticated_login(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, GithubError> {
        let out = self
            .run(["api", "user", "--jq", ".login"], cancellation)
            .await?;
        if !out.status.success() {
            return Ok(None);
        }
        let login = out.stdout_text().trim().to_owned();
        Ok((!login.is_empty()).then_some(login))
    }

    pub async fn fetch_worktrees(
        &self,
        branches: &[String],
        cancellation: &CancellationToken,
    ) -> Result<GithubData, GithubError> {
        if branches.is_empty() {
            return Ok(GithubData::default());
        }
        let repo = self.repository(cancellation).await?;
        let groups = chunk_branches(branches, CHUNK_SIZE);
        let client = self.clone();
        let cancel = cancellation.clone();
        let fetch = async {
            let mut chunks = stream::iter(groups.into_iter().enumerate().map(|(i, group)| {
                let client = client.clone();
                let cancel = cancel.clone();
                let repo = repo.clone();
                async move { client.fetch_chunk(&repo, &group, i == 0, &cancel).await }
            }))
            .buffer_unordered(MAX_CHUNK_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;
            let mut data = GithubData::default();
            for mut chunk in chunks.drain(..) {
                data.prs.append(&mut chunk.prs);
                data.merge_queue.append(&mut chunk.merge_queue);
            }
            Ok::<GithubData, GithubError>(data)
        };
        timeout(Duration::from_secs(100), fetch)
            .await
            .map_err(|_| GithubError::Transient {
                message: "github fetch retry budget exhausted".into(),
            })?
    }

    async fn fetch_chunk(
        &self,
        repo: &RepoSlug,
        branches: &[String],
        with_queue: bool,
        cancellation: &CancellationToken,
    ) -> Result<GithubData, GithubError> {
        for attempt in 0..MAX_ATTEMPTS {
            let (query, vars) = build_query(
                branches,
                with_queue,
                &repo.owner,
                &repo.name,
                &self.options.base_branch,
                if self.options.review_bot_mode == ReviewBotMode::Checklist {
                    30
                } else {
                    10
                },
            );
            let mut args = vec![
                "api".to_owned(),
                "graphql".to_owned(),
                "-f".to_owned(),
                format!("query={query}"),
            ];
            for (key, value) in vars {
                // All variables in this query are GraphQL strings. `-F`
                // asks gh to coerce values such as `false` and `2026` to
                // JSON booleans/numbers, which GraphQL then rejects.
                args.extend(["-f".into(), format!("{key}={value}")]);
            }
            match self.run(args, cancellation).await {
                Ok(out) if out.status.success() => {
                    match parse_chunk(&out.stdout, branches, with_queue, &self.options) {
                        Ok(data) => return Ok(data),
                        Err(GithubError::Transient { .. }) if attempt + 1 < MAX_ATTEMPTS => {
                            sleep(Duration::from_millis(400 << attempt)).await
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(out) => {
                    let (message, transient) = output_failure(&out);
                    if transient && attempt + 1 < MAX_ATTEMPTS {
                        sleep(Duration::from_millis(400 << attempt)).await;
                        continue;
                    }
                    if is_rate_limit(&message) {
                        return Err(GithubError::RateLimit(message));
                    }
                    if transient {
                        return Err(GithubError::Transient { message });
                    }
                    return Err(GithubError::Command(message));
                }
                Err(GithubError::Transient { .. }) if attempt + 1 < MAX_ATTEMPTS => {
                    sleep(Duration::from_millis(400 << attempt)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(GithubError::Transient {
            message: "GitHub fetch retry budget exhausted".into(),
        })
    }

    pub async fn fetch_review_requests(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<ReviewRequestPr>, GithubError> {
        if !self.options.reviewers_enabled {
            return Ok(Vec::new());
        }
        let query = review_requests_query();
        let out = self
            .run(
                ["api", "graphql", "-f", &format!("query={query}")],
                cancellation,
            )
            .await?;
        if !out.status.success() {
            return Err(classify_output(&out));
        }
        let body: Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| GithubError::Protocol(format!("invalid review-request JSON: {e}")))?;
        if body
            .get("errors")
            .is_some_and(|e| !e.is_null() && e.as_array().is_some_and(|a| !a.is_empty()))
        {
            return Err(GithubError::Protocol(
                "GitHub returned partial review-request GraphQL errors".into(),
            ));
        }
        let nodes = body
            .pointer("/data/search/nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                GithubError::Protocol("GitHub omitted review request search results".into())
            })?;
        Ok(nodes
            .iter()
            .filter_map(|node| parse_review_request(node, &self.options))
            .collect())
    }

    pub async fn view_pr(
        &self,
        branch: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<LivePrInfo>, GithubError> {
        if branch.is_empty() {
            return Ok(None);
        }
        let out = self
            .run(
                [
                    "pr",
                    "view",
                    branch,
                    "--json",
                    "number,baseRefName,state,isDraft,title,id,headRefOid",
                ],
                cancellation,
            )
            .await?;
        if !out.status.success() {
            return Ok(None);
        }
        let v: Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| GithubError::Protocol(format!("invalid PR info: {e}")))?;
        Ok(v.get("number")
            .and_then(Value::as_u64)
            .map(|number| LivePrInfo {
                number,
                base_ref_name: str_at(&v, "/baseRefName").unwrap_or_default().into(),
                state: str_at(&v, "/state")
                    .filter(|s| ["OPEN", "CLOSED", "MERGED"].contains(s))
                    .unwrap_or("OPEN")
                    .into(),
                is_draft: v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
                title: str_at(&v, "/title").unwrap_or_default().into(),
                id: str_at(&v, "/id").unwrap_or_default().into(),
                head_ref_oid: str_at(&v, "/headRefOid").unwrap_or_default().into(),
            }))
    }

    pub async fn edit_reviewers(
        &self,
        number: u64,
        add: &[String],
        remove: &[String],
        cancellation: &CancellationToken,
    ) -> GhActionResult {
        if add.is_empty() && remove.is_empty() {
            return GhActionResult::success();
        }
        let mut args = vec!["pr".into(), "edit".into(), number.to_string()];
        for login in add {
            args.extend(["--add-reviewer".into(), login.clone()]);
        }
        for login in remove {
            args.extend(["--remove-reviewer".into(), login.clone()]);
        }
        self.write(args, cancellation).await
    }

    pub async fn retarget_pr_base(
        &self,
        number: u64,
        base: &str,
        cancellation: &CancellationToken,
    ) -> GhActionResult {
        self.write(
            [
                "pr".into(),
                "edit".into(),
                number.to_string(),
                "--base".into(),
                base.into(),
            ],
            cancellation,
        )
        .await
    }
    pub async fn mark_pull_request_ready(
        &self,
        number: u64,
        cancellation: &CancellationToken,
    ) -> GhActionResult {
        self.write(
            ["pr".into(), "ready".into(), number.to_string()],
            cancellation,
        )
        .await
    }
    pub async fn close_issue(
        &self,
        number: u64,
        cancellation: &CancellationToken,
    ) -> GhActionResult {
        self.write(
            [
                "issue".into(),
                "close".into(),
                number.to_string(),
                "--reason".into(),
                "completed".into(),
            ],
            cancellation,
        )
        .await
    }
    pub async fn delete_remote_branch(
        &self,
        branch: &str,
        cancellation: &CancellationToken,
    ) -> GhActionResult {
        if branch.is_empty() {
            return GhActionResult::error("missing branch");
        }
        if branch == self.options.base_branch {
            return GhActionResult::error(format!("refusing to delete the trunk branch {branch}"));
        }
        let repo = match self.repository(cancellation).await {
            Ok(repo) => repo,
            Err(e) => return GhActionResult::error(e.to_string()),
        };
        self.write(
            [
                "api".into(),
                "--method".into(),
                "DELETE".into(),
                format!("repos/{}/git/refs/heads/{branch}", repo.as_str()),
            ],
            cancellation,
        )
        .await
    }

    async fn write(
        &self,
        args: impl IntoIterator<Item = impl Into<std::ffi::OsString>>,
        cancellation: &CancellationToken,
    ) -> GhActionResult {
        match self.run(args, cancellation).await {
            Ok(out) if out.status.success() => GhActionResult::success(),
            Ok(out) => {
                let (raw, _) = output_failure(&out);
                let error = if missing_workflow_scope(&raw) {
                    format!("{raw} {WORKFLOW_SCOPE_REMEDY}")
                } else {
                    raw
                };
                GhActionResult::error(error)
            }
            Err(error) => GhActionResult::error(error.to_string()),
        }
    }

    pub async fn enable_auto_merge(
        &self,
        target: &PrMergeTarget,
        cancellation: &CancellationToken,
    ) -> GhActionResult {
        let _guard = match MergeGuard::acquire(self.merge_requests.clone(), target.number) {
            Ok(g) => g,
            Err(message) => return GhActionResult::error(message),
        };
        if target.id.is_empty() {
            return GhActionResult::error("missing PR node id");
        }
        // Probe the PR's actual base queue and always use explicit queue mode.
        let queue_query = "query($owner: String!, $name: String!, $branch: String!) { repository(owner:$owner,name:$name) { mergeQueue(branch:$branch) { id } } }";
        let repo = match self.repository(cancellation).await {
            Ok(repo) => repo,
            Err(e) => return GhActionResult::error(e.to_string()),
        };
        let probe = self
            .run(
                [
                    "api",
                    "graphql",
                    "-f",
                    &format!("query={queue_query}"),
                    "-f",
                    &format!("owner={}", repo.owner),
                    "-f",
                    &format!("name={}", repo.name),
                    "-f",
                    &format!("branch={}", target.base_ref_name),
                ],
                cancellation,
            )
            .await;
        let probe = match probe {
            Ok(out) if out.status.success() => parse_merge_queue_probe(&out.stdout),
            Ok(out) => Err(output_failure(&out).0),
            Err(error) => Err(error.to_string()),
        };
        let queue_id = match probe {
            Ok(queue_id) => queue_id,
            Err(error) => {
                return GhActionResult::error(format!(
                    "cannot determine whether base branch {:?} has a merge queue: {error}; refusing to arm classic auto-merge",
                    target.base_ref_name
                ));
            }
        };
        if queue_id.is_some() {
            if target.head_ref_oid.is_empty() {
                return GhActionResult::error("missing PR head SHA; refresh GitHub and retry");
            }
            let endpoint = format!(
                "repos/{}/pulls/{}/merge-async",
                repo.as_str(),
                target.number
            );
            let out = self
                .run(
                    [
                        "api".into(),
                        "--include".into(),
                        "--method".into(),
                        "PUT".into(),
                        endpoint.clone(),
                        "-H".into(),
                        "Accept: application/vnd.github+json".into(),
                        "-H".into(),
                        "X-GitHub-Api-Version: 2026-03-10".into(),
                        "-f".into(),
                        format!("sha={}", target.head_ref_oid),
                        "-f".into(),
                        "merge_action=merge_queue".into(),
                        "-F".into(),
                        "bypass_rules=false".into(),
                    ],
                    cancellation,
                )
                .await;
            match out {
                Ok(out) => {
                    let response = out.stdout_text();
                    match crate::mutations::parse_async_merge_response(
                        &response,
                        &target.head_ref_oid,
                    ) {
                        Ok(true) if out.status.success() => {
                            return GhActionResult::success();
                        }
                        Ok(true) => {
                            return GhActionResult::failed(
                                format!(
                                    "async merge response for PR #{} was successful but gh exited unsuccessfully; inspect GitHub before retrying",
                                    target.number
                                ),
                                false,
                            );
                        }
                        Ok(false) => {
                            let Some(uuid) = crate::mutations::async_uuid(&response) else {
                                return GhActionResult::failed(
                                    format!(
                                        "GitHub accepted the async merge but returned no request UUID for PR #{}; inspect GitHub before retrying",
                                        target.number
                                    ),
                                    false,
                                );
                            };
                            for _ in 0..60 {
                                tokio::select! { _=cancellation.cancelled()=>return GhActionResult::failed(format!("PR #{} async merge outcome remains unknown; inspect GitHub before retrying",target.number),false), _=sleep(Duration::from_secs(1))=>{} }
                                let polled = self
                                    .run(
                                        [
                                            "api".into(),
                                            "--include".into(),
                                            "--method".into(),
                                            "GET".into(),
                                            format!("{endpoint}/{uuid}"),
                                            "-H".into(),
                                            "Accept: application/vnd.github+json".into(),
                                            "-H".into(),
                                            "X-GitHub-Api-Version: 2026-03-10".into(),
                                        ],
                                        cancellation,
                                    )
                                    .await;
                                let Ok(polled) = polled else {
                                    return GhActionResult::failed(
                                        format!(
                                            "Cannot confirm async merge for PR #{}; inspect GitHub before retrying",
                                            target.number
                                        ),
                                        false,
                                    );
                                };
                                if !polled.status.success() {
                                    return GhActionResult::failed(
                                        format!(
                                            "Cannot confirm async merge for PR #{}: {}; inspect GitHub before retrying",
                                            target.number,
                                            output_failure(&polled).0
                                        ),
                                        false,
                                    );
                                }
                                match crate::mutations::parse_async_merge_response(
                                    &polled.stdout_text(),
                                    &target.head_ref_oid,
                                ) {
                                    Ok(true) => return GhActionResult::success(),
                                    Ok(false) => continue,
                                    Err(error) => {
                                        return GhActionResult::failed(
                                            format!(
                                                "Cannot confirm async merge for PR #{}: {error}; inspect GitHub before retrying",
                                                target.number
                                            ),
                                            false,
                                        );
                                    }
                                }
                            }
                            return GhActionResult::failed(
                                format!(
                                    "Async merge is still pending for PR #{} after 60 seconds; inspect GitHub before retrying",
                                    target.number
                                ),
                                false,
                            );
                        }
                        Err(error) => {
                            if missing_workflow_scope(&error) {
                                return GhActionResult::error(format!(
                                    "{error} {WORKFLOW_SCOPE_REMEDY}"
                                ));
                            }
                            if !not_yet_enqueueable(&error) && !checks_still_pending(&error) {
                                return GhActionResult::error(error);
                            }
                            let armed = self.classic_arm(target, cancellation).await;
                            if armed.is_ok() {
                                return armed;
                            }
                            let pending = checks_still_pending(&error);
                            return GhActionResult::failed(
                                format!(
                                    "{error} (arming instead also failed: {})",
                                    result_error(&armed)
                                ),
                                pending,
                            );
                        }
                    }
                }
                Err(e) => {
                    return GhActionResult::failed(
                        format!(
                            "async merge outcome unknown: {e}; inspect PR #{} before retrying",
                            target.number
                        ),
                        false,
                    );
                }
            }
        }
        self.classic_arm(target, cancellation).await
    }

    async fn classic_arm(
        &self,
        target: &PrMergeTarget,
        cancellation: &CancellationToken,
    ) -> GhActionResult {
        self.write(["api".into(), "graphql".into(), "-f".into(), "query=mutation($prId: ID!, $method: PullRequestMergeMethod!) { enablePullRequestAutoMerge(input: {pullRequestId:$prId,mergeMethod:$method}) { pullRequest { number } } }".into(), "-f".into(), format!("prId={}",target.id), "-f".into(), "method=REBASE".into()], cancellation).await
    }

    pub async fn disable_auto_merge(
        &self,
        target: &PrMergeTarget,
        cancellation: &CancellationToken,
    ) -> GhActionResult {
        if self
            .merge_requests
            .lock()
            .map(|s| s.contains(&target.number))
            .unwrap_or(true)
        {
            return GhActionResult::error(format!(
                "merge request for PR #{} is still processing; wait before cancelling",
                target.number
            ));
        }
        if target.id.is_empty() {
            return GhActionResult::error(format!(
                "cannot inspect #{}: missing PR node id",
                target.number
            ));
        }
        let query = "query($prId: ID!) { node(id:$prId) { ... on PullRequest { mergeQueueEntry { id } autoMergeRequest { enabledAt } } } }";
        let out = match self
            .run(
                [
                    "api",
                    "graphql",
                    "-f",
                    &format!("query={query}"),
                    "-f",
                    &format!("prId={}", target.id),
                ],
                cancellation,
            )
            .await
        {
            Ok(out) if out.status.success() => out,
            Ok(out) => {
                return GhActionResult::error(format!(
                    "cannot inspect #{}'s merge state: {}",
                    target.number,
                    output_failure(&out).0
                ));
            }
            Err(e) => {
                return GhActionResult::error(format!(
                    "cannot inspect #{}'s merge state: {e}",
                    target.number
                ));
            }
        };
        let kind = match parse_merge_arm_probe(&out.stdout) {
            Ok(kind) => kind,
            Err(error) => {
                return GhActionResult::error(format!(
                    "cannot inspect #{}'s merge state: {error}",
                    target.number
                ));
            }
        };
        if kind == "none" {
            return GhActionResult::error(format!(
                "#{} has neither a merge-queue entry nor classic auto-merge armed",
                target.number
            ));
        }
        if kind == "queue" || kind == "both" {
            let dequeue = self.write(["api".into(), "graphql".into(), "-f".into(), "query=mutation($prId: ID!) { dequeuePullRequest(input:{id:$prId}) { mergeQueueEntry { position } } }".into(), "-f".into(), format!("prId={}",target.id)], cancellation).await;
            if !dequeue.is_ok() || kind == "queue" {
                return dequeue;
            }
        }
        self.write(
            [
                "pr".into(),
                "merge".into(),
                target.number.to_string(),
                "--disable-auto".into(),
            ],
            cancellation,
        )
        .await
    }

    pub async fn fetch_failed_run_log(
        &self,
        branch: &str,
        cancellation: &CancellationToken,
    ) -> Result<(u64, Vec<String>), GithubError> {
        let out = self
            .run(
                [
                    "run",
                    "list",
                    "--branch",
                    branch,
                    "--status",
                    "failure",
                    "--limit",
                    "1",
                    "--json",
                    "databaseId",
                ],
                cancellation,
            )
            .await?;
        if !out.status.success() {
            return Err(classify_output(&out));
        }
        let runs: Vec<Value> = serde_json::from_slice(&out.stdout)
            .map_err(|e| GithubError::Protocol(format!("invalid run list: {e}")))?;
        let id = runs
            .first()
            .and_then(|v| v.get("databaseId"))
            .and_then(Value::as_u64)
            .ok_or_else(|| GithubError::Protocol("no failed workflow run".into()))?;
        let out = self
            .run(
                ["run", "view", &id.to_string(), "--log-failed"],
                cancellation,
            )
            .await?;
        if !out.status.success() {
            return Err(classify_output(&out));
        }
        Ok((id, out.stdout_text().lines().map(str::to_owned).collect()))
    }

    /// Top contributors intersected with authors active on the default branch
    /// in the previous ~six months. If the recency probe fails, preserve the
    /// contributor list rather than turning an outage into an empty picker.
    pub async fn fetch_repo_contributors(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<Contributor>, GithubError> {
        let mut cache = self.contributors_cache.lock().await;
        if let Some((updated_at, contributors)) = cache.as_ref()
            && updated_at.elapsed() < PICKER_CACHE_TTL
        {
            return Ok(contributors.clone());
        }
        let contributors = self.fetch_repo_contributors_uncached(cancellation).await?;
        *cache = Some((tokio::time::Instant::now(), contributors.clone()));
        Ok(contributors)
    }

    /// Current authenticated login, cached with the picker contributor data.
    pub async fn viewer_login(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<String, GithubError> {
        let mut cache = self.viewer_login_cache.lock().await;
        if let Some((updated_at, login)) = cache.as_ref()
            && updated_at.elapsed() < PICKER_CACHE_TTL
        {
            return Ok(login.clone());
        }
        let output = self
            .run(["api", "user", "--jq", ".login"], cancellation)
            .await?;
        if !output.status.success() {
            return Err(classify_output(&output));
        }
        let login = output.stdout_text().trim().to_owned();
        if login.is_empty() {
            return Err(GithubError::Protocol(
                "authenticated GitHub user response omitted login".into(),
            ));
        }
        *cache = Some((tokio::time::Instant::now(), login.clone()));
        Ok(login)
    }

    async fn fetch_repo_contributors_uncached(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<Contributor>, GithubError> {
        let repo = self.repository(cancellation).await?.as_str();
        let contributors = self
            .run(
                [
                    "api".into(),
                    format!("repos/{repo}/contributors?per_page=100"),
                ],
                cancellation,
            )
            .await?;
        if !contributors.status.success() {
            return Err(classify_output(&contributors));
        }
        let raw: Vec<Value> = serde_json::from_slice(&contributors.stdout)
            .map_err(|e| GithubError::Protocol(format!("invalid contributor list: {e}")))?;
        let since = (OffsetDateTime::now_utc() - time::Duration::days(183))
            .format(&Rfc3339)
            .unwrap_or_default();
        let mut active = std::collections::HashSet::<String>::new();
        let mut recency_ok = true;
        for page in 1..=5 {
            let commits = self
                .run(
                    [
                        "api".into(),
                        format!("repos/{repo}/commits?since={since}&per_page=100&page={page}"),
                    ],
                    cancellation,
                )
                .await;
            let Ok(commits) = commits else {
                recency_ok = false;
                break;
            };
            if !commits.status.success() {
                recency_ok = false;
                break;
            }
            let Ok(entries) = serde_json::from_slice::<Vec<Value>>(&commits.stdout) else {
                recency_ok = false;
                break;
            };
            let n = entries.len();
            for entry in entries {
                if let Some(login) = str_at(&entry, "/author/login") {
                    active.insert(login.to_owned());
                }
            }
            if n < 100 {
                break;
            }
        }
        Ok(raw
            .into_iter()
            .filter_map(|c| {
                let login = str_at(&c, "/login")?;
                if str_at(&c, "/type") == Some("Bot") || login.ends_with("[bot]") {
                    return None;
                }
                if recency_ok && !active.is_empty() && !active.contains(login) {
                    return None;
                }
                Some(Contributor {
                    login: login.into(),
                    contributions: c.get("contributions").and_then(Value::as_u64).unwrap_or(0),
                })
            })
            .collect())
    }
}

struct MergeGuard {
    requests: Arc<Mutex<HashSet<u64>>>,
    number: u64,
}
impl MergeGuard {
    fn acquire(requests: Arc<Mutex<HashSet<u64>>>, number: u64) -> Result<Self, String> {
        let mut set = requests
            .lock()
            .map_err(|_| "merge request state is unavailable".to_owned())?;
        if !set.insert(number) {
            return Err(format!(
                "#{}: merge request still processing; wait for its result",
                number
            ));
        }
        drop(set);
        Ok(Self { requests, number })
    }
}
impl Drop for MergeGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.requests.lock() {
            set.remove(&self.number);
        }
    }
}

fn classify_output(output: &ProcessOutput) -> GithubError {
    let (message, transient) = output_failure(output);
    if is_rate_limit(&message) {
        GithubError::RateLimit(message)
    } else if transient {
        GithubError::Transient { message }
    } else {
        GithubError::Command(message)
    }
}

fn result_error(value: &GhActionResult) -> &str {
    value.error.as_deref().unwrap_or("success")
}

fn str_at<'a>(value: &'a Value, ptr: &str) -> Option<&'a str> {
    value.pointer(ptr).and_then(Value::as_str)
}

fn build_query(
    branches: &[String],
    with_queue: bool,
    owner: &str,
    repo: &str,
    base: &str,
    comment_limit: usize,
) -> (String, Vec<(String, String)>) {
    let var_decls = (0..branches.len())
        .map(|i| format!("$b{i}: String!"))
        .collect::<Vec<_>>()
        .join(", ");
    let aliases = (0..branches.len()).map(|i| format!("wt_{i}: pullRequests(first:2,headRefName:$b{i},orderBy:{{field:UPDATED_AT,direction:DESC}}) {{ nodes {{ ...PrFields }} }}")).collect::<Vec<_>>().join("\n");
    let merge_queue = if with_queue {
        "mergeQueue(branch:$mergeQueueBranch) { entries(first:50) { nodes { enqueuedAt estimatedTimeToMerge position state pullRequest { headRefName } } } }"
    } else {
        ""
    };
    let merge_var = if with_queue {
        ", $mergeQueueBranch: String!"
    } else {
        ""
    };
    let fragment = PR_FRAGMENT.replace("COMMENT_FETCH_LIMIT", &comment_limit.to_string());
    let query = format!(
        "query($owner:String!,$name:String!{merge_var},{var_decls}) {{ repository(owner:$owner,name:$name) {{ {aliases} {merge_queue} }} }} {fragment}"
    );
    let mut vars = vec![("owner".into(), owner.into()), ("name".into(), repo.into())];
    if with_queue {
        vars.push(("mergeQueueBranch".into(), base.into()));
    }
    vars.extend(
        branches
            .iter()
            .enumerate()
            .map(|(i, b)| (format!("b{i}"), b.clone())),
    );
    (query, vars)
}

fn parse_merge_queue_probe(bytes: &[u8]) -> Result<Option<String>, String> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid merge queue response: {e}"))?;
    if let Some(errors) = value
        .get("errors")
        .and_then(Value::as_array)
        .filter(|errors| !errors.is_empty())
    {
        let message = errors
            .iter()
            .map(|error| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("GraphQL error")
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(message);
    }
    let repository = value
        .pointer("/data/repository")
        .ok_or_else(|| "response omitted data.repository".to_owned())?;
    let queue = repository
        .get("mergeQueue")
        .ok_or_else(|| "response omitted repository.mergeQueue".to_owned())?;
    if queue.is_null() {
        return Ok(None);
    }
    let id = queue
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "response contained a merge queue without an id".to_owned())?;
    Ok(Some(id.to_owned()))
}

fn parse_merge_arm_probe(bytes: &[u8]) -> Result<&'static str, String> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid GitHub response: {e}"))?;
    if let Some(errors) = value
        .get("errors")
        .and_then(Value::as_array)
        .filter(|errors| !errors.is_empty())
    {
        let message = errors
            .iter()
            .map(|error| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("GraphQL error")
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(message);
    }
    let node = value
        .pointer("/data/node")
        .filter(|node| node.is_object())
        .ok_or_else(|| "incomplete response; missing pull request".to_owned())?;
    if !node.as_object().is_some_and(|node| {
        node.contains_key("mergeQueueEntry") && node.contains_key("autoMergeRequest")
    }) {
        return Err("incomplete response; merge state fields are missing".into());
    }
    Ok(crate::mutations::merge_arm_kind(node))
}

const PR_FRAGMENT: &str = r#"fragment PrFields on PullRequest {
id number url title headRefName headRefOid baseRefName mergeCommit { oid } isDraft state mergeable mergeStateStatus mergedAt closedAt reviewDecision
reviewRequests(first:20) { totalCount nodes { requestedReviewer { __typename ... on User { login } ... on Team { combinedSlug } } } }
suggestedReviewers { reviewer { login } isAuthor isCommenter }
autoMergeRequest { enabledAt mergeMethod }
commits(last:1) { nodes { commit { committedDate statusCheckRollup { contexts(first:50) { nodes { __typename ... on CheckRun { name status conclusion startedAt checkSuite { workflowRun { databaseId workflow { databaseId } } } } ... on StatusContext { context state createdAt } } } } } } }
reviewThreads(first:50) { nodes { isResolved comments(first:1) { nodes { author { login __typename } } } } }
comments(last:COMMENT_FETCH_LIMIT) { nodes { author { login __typename } body createdAt updatedAt } }
reviews(last:10) { nodes { author { login __typename } body state createdAt } }
}"#;

fn parse_chunk(
    bytes: &[u8],
    branches: &[String],
    with_queue: bool,
    options: &GithubOptions,
) -> Result<GithubData, GithubError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| GithubError::Transient {
        message: format!("unparseable GitHub JSON: {e}"),
    })?;
    if let Some(errors) = value
        .get("errors")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        let message = errors
            .iter()
            .map(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("GraphQL error")
            })
            .collect::<Vec<_>>()
            .join("\n");
        if is_rate_limit(&message) {
            return Err(GithubError::RateLimit(message));
        }
        if crate::checks::is_transient_failure("", &message, false) {
            return Err(GithubError::Transient { message });
        }
        return Err(GithubError::Protocol(message));
    }
    let repo = value
        .pointer("/data/repository")
        .ok_or_else(|| GithubError::Protocol("missing GitHub repository payload".into()))?;
    let mut data = GithubData::default();
    for (i, _) in branches.iter().enumerate() {
        let nodes = repo
            .get(format!("wt_{i}"))
            .and_then(|v| v.get("nodes"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let node = nodes
            .iter()
            .find(|n| str_at(n, "/state") == Some("OPEN"))
            .or_else(|| nodes.first());
        if let Some(node) = node
            && let Some(pr) = parse_pr(node, options)
        {
            data.prs.insert(pr.head_ref_name.clone(), pr);
        }
    }
    if with_queue
        && let Some(nodes) = repo
            .pointer("/mergeQueue/entries/nodes")
            .and_then(Value::as_array)
    {
        for item in nodes {
            let Some(head) = str_at(item, "/pullRequest/headRefName") else {
                continue;
            };
            let state = match str_at(item, "/state").unwrap_or("") {
                "AWAITING_CHECKS" => MergeQueueState::AwaitingChecks,
                "LOCKED" => MergeQueueState::Locked,
                "MERGEABLE" => MergeQueueState::Mergeable,
                "QUEUED" => MergeQueueState::Queued,
                _ => MergeQueueState::Unmergeable,
            };
            data.merge_queue.insert(
                head.into(),
                MergeQueueEntry {
                    head_ref_name: head.into(),
                    position: item.get("position").and_then(Value::as_u64).unwrap_or(0) as u32,
                    state,
                    enqueued_at: str_at(item, "/enqueuedAt").unwrap_or_default().into(),
                    estimated_time_to_merge: item
                        .get("estimatedTimeToMerge")
                        .and_then(Value::as_u64),
                },
            );
        }
    }
    Ok(data)
}

fn parse_pr(pr: &Value, options: &GithubOptions) -> Option<PullRequest> {
    let contexts = pr
        .pointer("/commits/nodes/0/commit/statusCheckRollup/contexts/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let (mut checks, failed_checks) = rollup_checks(
        &contexts,
        &options.ignored_checks,
        &options.review_bot_contexts,
    );
    let state = str_at(pr, "/state").unwrap_or("OPEN");
    if options.repo_has_ci && state == "OPEN" && checks == PrChecks::None {
        checks = PrChecks::Pending;
    }
    let request_nodes = pr
        .pointer("/reviewRequests/nodes")
        .and_then(Value::as_array);
    let requested_reviewers: Vec<String> = request_nodes
        .into_iter()
        .flatten()
        .filter_map(|n| {
            let r = n.get("requestedReviewer")?;
            match str_at(r, "/__typename") {
                Some("User") => str_at(r, "/login").map(str::to_owned),
                Some("Team") => str_at(r, "/combinedSlug").map(str::to_owned),
                _ => None,
            }
        })
        .collect();
    let decision = str_at(pr, "/reviewDecision");
    let review = if state != "OPEN" {
        PrReview::None
    } else {
        match decision {
            Some("APPROVED") => PrReview::Approved,
            Some("CHANGES_REQUESTED") if !has_stale_changes_request(pr, &requested_reviewers) => {
                PrReview::ChangesRequested
            }
            Some("CHANGES_REQUESTED") => PrReview::Pending,
            _ if requested_reviewers.is_empty() => PrReview::Unrequested,
            _ => PrReview::Pending,
        }
    };
    let comments = human_comments(pr, &options.review_bot_login);
    let threads = pr.pointer("/reviewThreads/nodes").and_then(Value::as_array);
    let unresolved_threads_total = threads
        .into_iter()
        .flatten()
        .filter(|t| t.get("isResolved").and_then(Value::as_bool) == Some(false))
        .count() as u32;
    let unresolved_threads = threads
        .into_iter()
        .flatten()
        .filter(|t| {
            t.get("isResolved").and_then(Value::as_bool) == Some(false)
                && human_author(
                    t.pointer("/comments/nodes/0/author"),
                    &options.review_bot_login,
                )
        })
        .count() as u32;
    let review_bot = Some(review_bot_status(pr, state, &contexts, options));
    Some(PullRequest {
        id: str_at(pr, "/id").map(str::to_owned),
        number: pr.get("number")?.as_u64()?,
        url: str_at(pr, "/url")?.into(),
        head_ref_name: str_at(pr, "/headRefName")?.into(),
        head_ref_oid: str_at(pr, "/headRefOid").map(str::to_owned),
        base_ref_name: str_at(pr, "/baseRefName").unwrap_or_default().into(),
        merge_commit_oid: str_at(pr, "/mergeCommit/oid").map(str::to_owned),
        title: str_at(pr, "/title").unwrap_or_default().into(),
        is_draft: pr.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        state: state.into(),
        mergeable: str_at(pr, "/mergeable").map(str::to_owned),
        merge_state_status: str_at(pr, "/mergeStateStatus").map(str::to_owned),
        checks,
        failed_checks: if state == "OPEN" {
            failed_checks
        } else {
            Vec::new()
        },
        review,
        review_requests: pr
            .pointer("/reviewRequests/totalCount")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        requested_reviewers,
        suggested_reviewers: pr
            .get("suggestedReviewers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| {
                Some(SuggestedReviewer {
                    login: str_at(v, "/reviewer/login")?.into(),
                    is_author: v.get("isAuthor").and_then(Value::as_bool).unwrap_or(false),
                    is_commenter: v
                        .get("isCommenter")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect(),
        review_bot,
        auto_merge: pr
            .get("autoMergeRequest")
            .filter(|v| !v.is_null())
            .and_then(|v| {
                Some(AutoMerge {
                    enabled_at: str_at(v, "/enabledAt")?.into(),
                    merge_method: match str_at(v, "/mergeMethod")? {
                        "MERGE" => AutoMergeMethod::Merge,
                        "SQUASH" => AutoMergeMethod::Squash,
                        _ => AutoMergeMethod::Rebase,
                    },
                })
            }),
        comments,
        unresolved_threads,
        unresolved_threads_total,
        merged_at: str_at(pr, "/mergedAt").map(str::to_owned),
        closed_at: str_at(pr, "/closedAt").map(str::to_owned),
    })
}

fn human_author(author: Option<&Value>, bot_login: &str) -> bool {
    let Some(a) = author else { return false };
    let Some(login) = str_at(a, "/login") else {
        return false;
    };
    str_at(a, "/__typename") != Some("Bot")
        && login.trim_end_matches("[bot]") != bot_login.trim_end_matches("[bot]")
}
fn human_comments(pr: &Value, bot_login: &str) -> Vec<PrComment> {
    let mut out = Vec::new();
    for path in ["/comments/nodes", "/reviews/nodes"] {
        for c in pr
            .pointer(path)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !human_author(c.get("author"), bot_login) {
                continue;
            }
            let body = str_at(c, "/body").unwrap_or("").trim();
            if body.is_empty() {
                continue;
            }
            out.push(PrComment {
                author: str_at(c, "/author/login").unwrap_or_default().into(),
                body: body.into(),
                created_at: str_at(c, "/createdAt").unwrap_or_default().into(),
            });
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out.truncate(10);
    out
}

fn review_requests_query() -> &'static str {
    r#"query { search(query:"is:pr is:open review-requested:@me",type:ISSUE,first:50) { nodes { ... on PullRequest { number url title isDraft createdAt updatedAt author { login } repository { nameWithOwner } headRefName headRefOid additions deletions changedFiles reviewDecision comments { totalCount } commits(last:1) { nodes { commit { statusCheckRollup { contexts(first:50) { nodes { __typename ... on CheckRun { name status conclusion startedAt checkSuite { workflowRun { databaseId workflow { databaseId } } } } ... on StatusContext { context state createdAt } } } } } } } } } } }"#
}
fn parse_review_request(v: &Value, options: &GithubOptions) -> Option<ReviewRequestPr> {
    let repo = str_at(v, "/repository/nameWithOwner").unwrap_or_default();
    if options
        .ignored_review_repositories
        .iter()
        .any(|r| r.trim().eq_ignore_ascii_case(repo.trim()))
    {
        return None;
    }
    let checks = rollup_checks(
        v.pointer("/commits/nodes/0/commit/statusCheckRollup/contexts/nodes")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        &options.ignored_checks,
        &options.review_bot_contexts,
    )
    .0;
    Some(ReviewRequestPr {
        number: v.get("number")?.as_u64()?,
        url: str_at(v, "/url")?.into(),
        title: str_at(v, "/title")?.into(),
        repo_name_with_owner: repo.into(),
        head_ref_name: str_at(v, "/headRefName").map(str::to_owned),
        head_ref_oid: str_at(v, "/headRefOid").map(str::to_owned),
        author: str_at(v, "/author/login").map(str::to_owned),
        is_draft: v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        checks: if options.repo_has_ci && checks == PrChecks::None {
            PrChecks::Pending
        } else {
            checks
        },
        review_decision: str_at(v, "/reviewDecision")
            .filter(|s| ["APPROVED", "CHANGES_REQUESTED", "REVIEW_REQUIRED"].contains(s))
            .map(str::to_owned),
        additions: v.get("additions").and_then(Value::as_u64).unwrap_or(0),
        deletions: v.get("deletions").and_then(Value::as_u64).unwrap_or(0),
        changed_files: v.get("changedFiles").and_then(Value::as_u64).unwrap_or(0),
        comment_count: v
            .pointer("/comments/totalCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        created_at: str_at(v, "/createdAt").unwrap_or_default().into(),
        updated_at: str_at(v, "/updatedAt").unwrap_or_default().into(),
    })
}

fn is_bot(login: Option<&str>, configured: &str) -> bool {
    login.is_some_and(|l| {
        l == configured || l.trim_end_matches("[bot]") == configured.trim_end_matches("[bot]")
    })
}
fn has_stale_changes_request(pr: &Value, requested: &[String]) -> bool {
    if requested.is_empty() {
        return false;
    }
    let requested: std::collections::HashSet<_> = requested.iter().map(String::as_str).collect();
    let mut seen = std::collections::HashSet::new();
    for review in pr
        .pointer("/reviews/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
    {
        let Some(author) = str_at(review, "/author/login") else {
            continue;
        };
        if seen.contains(author) {
            continue;
        }
        let Some(state) = str_at(review, "/state") else {
            continue;
        };
        if !["APPROVED", "CHANGES_REQUESTED", "DISMISSED"].contains(&state) {
            continue;
        }
        seen.insert(author);
        if state == "CHANGES_REQUESTED" && requested.contains(author) {
            return true;
        }
    }
    false
}
fn wildcard(pattern: &str, value: &str) -> bool {
    let p = pattern.to_lowercase();
    let v = value.to_lowercase();
    let mut rest = v.as_str();
    let parts: Vec<_> = p.split('*').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let Some(pos) = rest.find(part) else {
            return false;
        };
        if i == 0 && !p.starts_with('*') && pos != 0 {
            return false;
        }
        rest = &rest[pos + part.len()..];
    }
    p.ends_with('*') || rest.is_empty()
}
fn bot_context_state(contexts: &[Value], options: &GithubOptions) -> Option<bool> {
    let mut done = false;
    for check in contexts {
        let Some(name) = str_at(check, "/name").or_else(|| str_at(check, "/context")) else {
            continue;
        };
        if !options
            .review_bot_contexts
            .iter()
            .any(|p| wildcard(p, name))
        {
            continue;
        }
        let pending = if str_at(check, "/__typename") == Some("CheckRun") {
            str_at(check, "/status").is_some_and(|s| s != "COMPLETED")
        } else {
            matches!(str_at(check, "/state"), Some("PENDING" | "EXPECTED"))
        };
        if pending {
            return Some(true);
        }
        done = true;
    }
    if done { Some(false) } else { None }
}
fn boxes(body: &str) -> u32 {
    let (mut count, mut fence) = (0, false);
    for line in body.lines() {
        let line = line.trim_start();
        if line.starts_with("```") || line.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if !fence && line.starts_with("- [ ]") {
            count += 1;
        }
    }
    count
}
fn has_marker(body: &str, marker: &str) -> bool {
    body.lines()
        .take(3)
        .any(|line| line.trim_start().starts_with(marker))
}
fn review_bot_status(
    pr: &Value,
    state: &str,
    contexts: &[Value],
    options: &GithubOptions,
) -> ReviewBotStatus {
    let none = || ReviewBotStatus {
        state: "none".into(),
        unresolved: 0,
        stale: None,
    };
    if state != "OPEN" {
        return none();
    }
    if options.review_bot_mode == ReviewBotMode::Checklist {
        let mut summaries = std::collections::BTreeMap::<String, (String, String, String)>::new();
        let mut ack: Option<(String, String)> = None;
        for c in pr
            .pointer("/comments/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !is_bot(str_at(c, "/author/login"), &options.review_bot_login) {
                continue;
            }
            let body = str_at(c, "/body").unwrap_or("");
            let created = str_at(c, "/createdAt").unwrap_or("");
            let updated = str_at(c, "/updatedAt").unwrap_or(created);
            let marker = options
                .review_bot_summary_markers
                .iter()
                .filter(|m| has_marker(body, m))
                .max_by_key(|m| m.len());
            if let Some(marker) = marker {
                let e = summaries.entry(marker.clone()).or_insert((
                    created.into(),
                    updated.into(),
                    body.into(),
                ));
                if created > e.0.as_str() {
                    *e = (created.into(), updated.into(), body.into());
                }
            } else if options
                .review_bot_pending_marker
                .as_deref()
                .is_some_and(|m| has_marker(body, m))
                && ack.as_ref().is_none_or(|a| created > a.0.as_str())
            {
                ack = Some((created.into(), updated.into()));
            }
        }
        let live: Vec<_> = summaries.values().collect();
        let unresolved: u32 = live.iter().map(|(_, _, body)| boxes(body)).sum();
        let newest = live.iter().map(|(c, _, _)| c.as_str()).max();
        let covers = pr
            .pointer("/headRefOid")
            .and_then(Value::as_str)
            .filter(|h| h.len() >= 7)
            .is_some_and(|h| live.iter().any(|(_, _, body)| body.contains(&h[..7])));
        let ctx = bot_context_state(contexts, options);
        let head_date = str_at(pr, "/commits/nodes/0/commit/committedDate");
        let stale =
            newest.is_some_and(|n| !covers && ctx.is_none() && head_date.is_some_and(|d| n < d));
        let touched = live.iter().map(|(_, u, _)| u.as_str()).max();
        let rerunning = ack
            .as_ref()
            .is_some_and(|(created, _)| touched.is_none_or(|t| created.as_str() > t));
        if unresolved > 0 {
            return ReviewBotStatus {
                state: "unresolved".into(),
                unresolved,
                stale: Some(stale),
            };
        }
        if ctx == Some(true) || rerunning {
            return ReviewBotStatus {
                state: "pending".into(),
                unresolved: 0,
                stale: None,
            };
        }
        if !live.is_empty() {
            return ReviewBotStatus {
                state: "clean".into(),
                unresolved: 0,
                stale: Some(stale),
            };
        }
        return none();
    }
    let unresolved = pr
        .pointer("/reviewThreads/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| {
            t.get("isResolved").and_then(Value::as_bool) == Some(false)
                && is_bot(
                    str_at(t, "/comments/nodes/0/author/login"),
                    &options.review_bot_login,
                )
        })
        .count() as u32;
    if unresolved > 0 {
        return ReviewBotStatus {
            state: "unresolved".into(),
            unresolved,
            stale: None,
        };
    }
    match bot_context_state(contexts, options) {
        Some(true) => ReviewBotStatus {
            state: "pending".into(),
            unresolved: 0,
            stale: None,
        },
        Some(false) => ReviewBotStatus {
            state: "clean".into(),
            unresolved: 0,
            stale: None,
        },
        None => none(),
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;
    use wt_platform::process::ProcessRunner;

    fn options() -> GithubOptions {
        GithubOptions {
            base_branch: "main".into(),
            reviewers_enabled: true,
            ignored_checks: vec![],
            review_bot_name: "CodeRabbit".into(),
            review_bot_login: "coderabbitai".into(),
            review_bot_contexts: vec!["CodeRabbit".into()],
            review_bot_mode: ReviewBotMode::Threads,
            review_bot_summary_markers: vec![],
            review_bot_pending_marker: None,
            ignored_review_repositories: vec![],
            default_reviewer: None,
            repo_has_ci: true,
        }
    }

    #[tokio::test]
    async fn fake_gh_graphql_fixture_preserves_pr_fields() {
        let dir = tempdir().unwrap();
        let fixture = dir.path().join("response.json");
        fs::write(&fixture,r#"{"data":{"repository":{"wt_0":{"nodes":[{"id":"PR_node","number":17,"url":"https://github.com/acme/app/pull/17","title":"A title","headRefName":"feature/a","headRefOid":"abc1234","baseRefName":"main","isDraft":false,"state":"OPEN","mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","reviewDecision":"APPROVED","reviewRequests":{"totalCount":1,"nodes":[{"requestedReviewer":{"__typename":"User","login":"reviewer"}}]},"suggestedReviewers":[{"reviewer":{"login":"suggested"},"isAuthor":false,"isCommenter":true}],"autoMergeRequest":null,"commits":{"nodes":[{"commit":{"committedDate":"2026-10-01T12:00:00Z","statusCheckRollup":{"contexts":{"nodes":[{"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS","startedAt":"2026-10-02T12:00:00Z"}]}}}}]},"reviewThreads":{"nodes":[{"isResolved":false,"comments":{"nodes":[{"author":{"login":"person","__typename":"User"}}]}}]},"comments":{"nodes":[{"author":{"login":"person","__typename":"User"},"body":"hello","createdAt":"2026-10-03T12:00:00Z"}]},"reviews":{"nodes":[]},"mergedAt":null,"closedAt":null}]}}}}"#).unwrap();
        let script = dir.path().join("fake-gh");
        fs::write(&script, format!("#!/bin/sh\ncat '{}'\n", fixture.display())).unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let client = GithubClient::new(ProcessRunner::default(), dir.path().to_owned(), options())
            .with_gh_program(script)
            .with_repository(RepoSlug::parse("acme/app").unwrap());
        let data = client
            .fetch_worktrees(&["feature/a".into()], &CancellationToken::new())
            .await
            .unwrap();
        let pr = &data.prs["feature/a"];
        assert_eq!(pr.number, 17);
        assert_eq!(pr.checks, PrChecks::Pass);
        assert_eq!(pr.review, PrReview::Approved);
        assert_eq!(pr.requested_reviewers, ["reviewer"]);
        assert_eq!(pr.suggested_reviewers[0].login, "suggested");
        assert_eq!(pr.comments[0].body, "hello");
        assert_eq!(pr.unresolved_threads_total, 1);
        assert_eq!(pr.unresolved_threads, 1);
    }

    #[tokio::test]
    async fn reviewer_picker_identity_and_contributors_are_cached_per_client() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("fake-gh");
        let calls = dir.path().join("calls.jsonl");
        fs::write(
            &script,
            format!(
                r#"#!/usr/bin/env python3
import json, sys
args=sys.argv[1:]
with open({calls:?}, 'a') as f: f.write(json.dumps(args)+'\n')
if args[:2] == ['api', 'user']:
    print('reviewer')
elif len(args) > 1 and args[1].startswith('repos/acme/repo/contributors'):
    print('[{{"login":"author","contributions":7}}]')
elif len(args) > 1 and args[1].startswith('repos/acme/repo/commits'):
    print('[]')
else:
    raise SystemExit('unexpected gh args: ' + repr(args))
"#,
                calls = calls.to_string_lossy()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let client = GithubClient::new(ProcessRunner::default(), dir.path().to_owned(), options())
            .with_gh_program(script)
            .with_repository(RepoSlug::parse("acme/repo").unwrap());
        let cancellation = CancellationToken::new();

        assert_eq!(
            client.viewer_login(&cancellation).await.unwrap(),
            "reviewer"
        );
        assert_eq!(
            client.clone().viewer_login(&cancellation).await.unwrap(),
            "reviewer"
        );
        let contributors = client.fetch_repo_contributors(&cancellation).await.unwrap();
        let again = client
            .clone()
            .fetch_repo_contributors(&cancellation)
            .await
            .unwrap();
        assert_eq!(contributors, again);
        assert_eq!(contributors[0].login, "author");
        assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 3);
    }

    #[test]
    fn malformed_success_payload_does_not_become_empty_data() {
        let result = parse_chunk(br#"{"data":{}}"#, &["feature/a".into()], false, &options());
        assert!(matches!(result, Err(GithubError::Protocol { .. })));
        let gql = parse_chunk(
            br#"{"data":{"repository":{}},"errors":[{"message":"rate limit exceeded"}]}"#,
            &["feature/a".into()],
            false,
            &options(),
        );
        assert!(matches!(gql, Err(GithubError::RateLimit(_))));
    }

    #[test]
    fn checklist_bot_counts_live_findings_and_marks_old_head_stale() {
        let mut opts = options();
        opts.review_bot_mode = ReviewBotMode::Checklist;
        opts.review_bot_login = "review-bot".into();
        opts.review_bot_summary_markers = vec!["<!-- summary -->".into()];
        opts.review_bot_pending_marker = Some("<!-- pending -->".into());
        let pr = serde_json::json!({"headRefOid":"abcdef0123456789","commits":{"nodes":[{"commit":{"committedDate":"2026-10-04T00:00:00Z"}}]},"comments":{"nodes":[{"author":{"login":"review-bot[bot]"},"body":"<!-- summary -->\n- [ ] Fix this\n```md\n- [ ] example\n```","createdAt":"2026-10-01T00:00:00Z"}]}});
        let status = review_bot_status(&pr, "OPEN", &[], &opts);
        assert_eq!(status.state, "unresolved");
        assert_eq!(status.unresolved, 1);
        assert_eq!(status.stale, Some(true));
        assert_eq!(boxes("- [ ] one\n~~~\n- [ ] ignored\n~~~\n  - [ ] two"), 2);
    }

    #[test]
    fn branch_names_are_variables_and_only_queue_chunk_needs_queue() {
        let branch = "feature/$staging".to_owned();
        let (with_queue, vars) = build_query(
            std::slice::from_ref(&branch),
            true,
            "owner",
            "repo",
            "release",
            30,
        );
        assert_eq!(
            with_queue.matches('{').count(),
            with_queue.matches('}').count()
        );
        assert!(with_queue.contains("mergeQueue(branch:$mergeQueueBranch)"));
        assert!(!with_queue.contains(&branch));
        assert!(
            vars.iter()
                .any(|(key, value)| key == "b0" && value == &branch)
        );
        let (without_queue, _) = build_query(&[branch], false, "owner", "repo", "release", 10);
        assert_eq!(
            without_queue.matches('{').count(),
            without_queue.matches('}').count()
        );
        assert!(!without_queue.contains("mergeQueue("));
    }

    #[test]
    fn review_request_query_is_complete() {
        let query = review_requests_query();
        assert!(query.starts_with("query "));
        assert_eq!(query.matches('{').count(), query.matches('}').count());
    }

    #[tokio::test]
    async fn string_graphql_variables_use_untyped_gh_fields() {
        let dir = tempdir().unwrap();
        let fixture = dir.path().join("response.json");
        fs::write(
            &fixture,
            r#"{"data":{"repository":{"wt_0":{"nodes":[]},"wt_1":{"nodes":[]},"wt_2":{"nodes":[]}}}}"#,
        )
        .unwrap();
        let args_file = dir.path().join("args.txt");
        let script = dir.path().join("fake-gh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat '{}'\n",
                args_file.display(),
                fixture.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let client = GithubClient::new(ProcessRunner::default(), dir.path().to_owned(), options())
            .with_gh_program(script)
            .with_repository(RepoSlug::parse("acme/app").unwrap());
        let branches = ["false", "2026", "@me"].map(str::to_owned);
        client
            .fetch_worktrees(&branches, &CancellationToken::new())
            .await
            .unwrap();

        let args = fs::read_to_string(args_file).unwrap();
        let args: Vec<_> = args.lines().collect();
        assert!(args.windows(2).any(|pair| pair == ["-f", "b0=false"]));
        assert!(args.windows(2).any(|pair| pair == ["-f", "b1=2026"]));
        assert!(args.windows(2).any(|pair| pair == ["-f", "b2=@me"]));
        assert!(!args.contains(&"-F"));
    }

    #[tokio::test]
    async fn failed_queue_probe_never_falls_back_to_classic_arm() {
        let dir = tempdir().unwrap();
        let fixture = dir.path().join("response.json");
        fs::write(
            &fixture,
            r#"{"data":{"repository":{"mergeQueue":null}},"errors":[{"message":"rate limit exceeded"}]}"#,
        )
        .unwrap();
        let calls = dir.path().join("calls.txt");
        let script = dir.path().join("fake-gh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho call >> '{}'\ncat '{}'\n",
                calls.display(),
                fixture.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        let client = GithubClient::new(ProcessRunner::default(), dir.path().to_owned(), options())
            .with_gh_program(script)
            .with_repository(RepoSlug::parse("acme/app").unwrap());
        let result = client
            .enable_auto_merge(
                &PrMergeTarget {
                    id: "PR_node".into(),
                    number: 42,
                    base_ref_name: "release".into(),
                    head_ref_oid: "deadbeef".into(),
                },
                &CancellationToken::new(),
            )
            .await;
        assert!(!result.is_ok());
        assert!(
            result
                .error
                .unwrap()
                .contains("refusing to arm classic auto-merge")
        );
        assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    }

    #[test]
    fn merge_queue_probe_distinguishes_absence_from_unknown() {
        assert_eq!(
            parse_merge_queue_probe(br#"{"data":{"repository":{"mergeQueue":null}}}"#),
            Ok(None)
        );
        assert_eq!(
            parse_merge_queue_probe(br#"{"data":{"repository":{"mergeQueue":{"id":"MQ_1"}}}}"#),
            Ok(Some("MQ_1".into()))
        );
        assert!(parse_merge_queue_probe(br#"{"data":{"repository":{}}}"#).is_err());
        assert!(parse_merge_queue_probe(
            br#"{"data":{"repository":{"mergeQueue":null}},"errors":[{"message":"not authorized"}]}"#
        )
        .is_err());
    }

    #[test]
    fn merge_arm_probe_requires_a_complete_error_free_pr_state() {
        assert_eq!(
            parse_merge_arm_probe(
                br#"{"data":{"node":{"mergeQueueEntry":null,"autoMergeRequest":null}}}"#
            ),
            Ok("none")
        );
        assert_eq!(
            parse_merge_arm_probe(
                br#"{"data":{"node":{"mergeQueueEntry":{"id":"Q"},"autoMergeRequest":null}}}"#
            ),
            Ok("queue")
        );
        assert!(parse_merge_arm_probe(br#"{"data":{"node":{}}}"#).is_err());
        assert!(parse_merge_arm_probe(
            br#"{"data":{"node":{"mergeQueueEntry":null,"autoMergeRequest":null}},"errors":[{"message":"partial result"}]}"#
        )
        .is_err());
    }
}
