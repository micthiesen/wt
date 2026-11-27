//! Shared lifecycle planning for CLI, TUI and automation callers. A plan is
//! presentation data, not permission to skip the service's locked rechecks.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use wt_github::{GithubClient, GithubData, GithubOptions};
use wt_lifecycle::{LifecycleService, ServiceConfig};
use wt_runtime::Confirmed;
use wt_vcs::WorktreeRecord;

use crate::commands::resolve::run_git;
use wt_app_core::context::AppContext;

pub fn service(ctx: &AppContext) -> Result<LifecycleService> {
    let context = ctx.clone();
    let service = LifecycleService::new(
        ServiceConfig::from_config(&ctx.config),
        (*ctx.repository).clone(),
        ctx.processes.clone(),
    )
    .with_before_remove(move |target, cancellation| {
        let context = context.clone();
        async move {
            match crate::host_cleanup::before_remove(
                &context,
                target.slug(),
                Path::new(&target.path),
                &cancellation,
            )
            .await
            {
                Ok(cleanup) => {
                    for session in cleanup.stopped_sessions {
                        tracing::info!(%session, "stopped worktree session before removal");
                    }
                    for pid in cleanup.reaped_listeners {
                        tracing::info!(pid, "reaped worktree listener before removal");
                    }
                    cleanup.warnings
                }
                Err(_) if cancellation.is_cancelled() => Vec::new(),
                Err(error) => vec![format!("host cleanup failed: {error:#}")],
            }
        }
    });
    Ok(if ctx.config.dev_server.is_some() {
        service.with_dev_server(wt_app_core::dev::service(ctx)?)
    } else {
        service
    })
}

pub async fn resolve_key(ctx: &AppContext, key: &str) -> Result<WorktreeRecord> {
    ctx.repository
        .inventory(&ctx.cancellation)
        .await?
        .into_iter()
        .find(|row| !row.is_main && wt_core::worktree_target_key(&row.target) == key)
        .context("worktree disappeared before the operation could run")
}

/// The lifecycle evidence automations evaluate for one worktree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LifecycleFacts {
    pub local_merged: Option<bool>,
    pub clean_candidate: bool,
    /// The landing reached `[branch] production`. False is unknown.
    pub in_production: bool,
    /// Deploy proof for the stages an owed check or a watch asks about.
    /// Every other stage with a probe reads not deployed; `None` is only a
    /// stage without one.
    pub deploys: wt_core::StageDeploys,
}

pub struct RemovalPlan {
    pub row: WorktreeRecord,
    pub landed: bool,
    pub hazards: Vec<String>,
    pub destroy_stage: bool,
    pub revision: wt_lifecycle::RemovalRevision,
    pub removed_snapshot: wt_store::RemovedWorktree,
}

pub struct RemovalPlans {
    pub rows: Vec<RemovalPlan>,
    pub warning: Option<String>,
}

/// Per-row landing evidence for automation facts. Each row's failure stays
/// with that row; no row here carries a removal witness.
#[derive(Default)]
pub struct AutomationLifecycleEvidence {
    pub rows: BTreeMap<String, LifecycleFacts>,
    pub row_errors: BTreeMap<String, String>,
    /// The commit that carried each landed row's work onto the base branch,
    /// for the rows a watch belongs to: what the watch follows once the
    /// worktree is removed.
    pub landing_commits: BTreeMap<String, String>,
}

/// What pending watches need known about one worktree's landing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WatchWants {
    pub staging: bool,
    pub production: bool,
}

impl WatchWants {
    fn stage(self, stage: wt_core::VerifyStage) -> bool {
        match stage {
            wt_core::VerifyStage::Staging => self.staging,
            wt_core::VerifyStage::Production => self.production,
        }
    }
}

/// The worktrees pending watches depend on, by slug: every watch's owner,
/// and each worktree a condition names, with the stages asked about.
pub fn watch_wants(state: &serde_json::Value) -> BTreeMap<String, WatchWants> {
    wants_of(&wt_store::stored_watches(state))
}

/// [`watch_wants`] for watches the caller holds, stored or not.
pub fn wants_of(watches: &[wt_core::watch::Watch]) -> BTreeMap<String, WatchWants> {
    use wt_core::watch::{Condition, RowState};
    let mut wants: BTreeMap<String, WatchWants> = BTreeMap::new();
    for watch in watches {
        if watch.owner_gone {
            continue;
        }
        wants.entry(watch.slug.clone()).or_default();
        if watch.is_met() {
            continue;
        }
        for cond in watch.when.iter().filter(|cond| !cond.latched) {
            let (slug, state) = match &cond.cond {
                Condition::Own(state) => (&watch.slug, *state),
                Condition::Worktree { slug, state } => (slug, *state),
                _ => continue,
            };
            let entry = wants.entry(slug.clone()).or_default();
            match state {
                RowState::Staging => entry.staging = true,
                RowState::Production => entry.production = true,
                _ => {}
            }
        }
    }
    wants
}

pub async fn plan(ctx: &AppContext, rows: Vec<WorktreeRecord>) -> Result<RemovalPlans> {
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let branches = rows
        .iter()
        .map(|row| row.target.branch.clone())
        .collect::<Vec<_>>();
    let client = GithubClient::new(
        ctx.processes.clone(),
        ctx.config.paths.main_clone.clone(),
        GithubOptions::from_config(&ctx.config, false),
    );
    let (github, warning) = if std::env::var("WT_GITHUB").as_deref() == Ok("off") {
        (GithubData::default(), None)
    } else {
        match client.fetch_worktrees(&branches, &ctx.cancellation).await {
            Ok(data) => (data, None),
            Err(error) if !ctx.cancellation.is_cancelled() => (
                GithubData::default(),
                Some(format!("GitHub merge evidence unavailable: {error}")),
            ),
            Err(error) => return Err(error.into()),
        }
    };
    crate::other_branches::observe(
        ctx,
        &rows,
        &state,
        Some(&github),
        crate::other_branches::Ask::Github,
    )
    .await;
    plan_with_evidence(ctx, rows, &state, &github, warning).await
}

/// Read local hazards with the host's confirmed GitHub evidence. A merged PR
/// must still match the current HEAD; Git ancestry keeps its published-base
/// check. `None` (GitHub still loading, or only restored data) is no evidence:
/// no branch lands through a PR.
pub async fn plan_cached(
    ctx: &AppContext,
    rows: Vec<WorktreeRecord>,
    github: Option<&Confirmed<GithubData>>,
) -> Result<RemovalPlans> {
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    // A person asked for this removal: the board's GitHub data covers each
    // row's own branch, so its other branches get one read of their own.
    crate::other_branches::observe(
        ctx,
        &rows,
        &state,
        github.map(|github| &**github),
        crate::other_branches::Ask::Github,
    )
    .await;
    plan_with_facts(ctx, rows, &state, github, None).await
}

/// Build removal evidence from caller-prepared wtstate and confirmed GitHub
/// data. This keeps automation evaluation on the same authoritative safety
/// path as explicit cleanup without launching another GitHub fetch.
pub async fn plan_with_facts(
    ctx: &AppContext,
    rows: Vec<WorktreeRecord>,
    state: &serde_json::Value,
    github: Option<&Confirmed<GithubData>>,
    warning: Option<String>,
) -> Result<RemovalPlans> {
    let empty = GithubData::default();
    let github = github.map_or(&empty, |github| &**github);
    plan_with_evidence(ctx, rows, state, github, warning).await
}

/// Local evidence failure belongs to its checkout. Automations for other rows
/// can proceed, but the failed row contributes no cleanup or landing proof.
/// GitHub remains a single confirmed, all-or-nothing batch input.
///
/// Facts use only `local_merged` and "landed with no hazards", so this reads
/// the hazards alone and never captures a removal revision: that digests the
/// whole checkout, untracked contents included, and this pass reruns on every
/// input change for every landed checkout awaiting cleanup. The one hazard a
/// capture adds (untracked content it could not hash) implies untracked files,
/// which the hazards already report as uncommitted changes. That is not a
/// safety shortcut: the fired cleanup plans afresh, and removal recaptures and
/// rechecks everything under its own lock.
pub async fn plan_automation_facts(
    ctx: &AppContext,
    rows: Vec<WorktreeRecord>,
    state: &serde_json::Value,
    github: Option<&Confirmed<GithubData>>,
) -> Result<AutomationLifecycleEvidence> {
    let empty = GithubData::default();
    let github = github.map_or(&empty, |github| &**github);
    landing_facts(ctx, rows, state, github, &watch_wants(state)).await
}

/// [`plan_automation_facts`] over a GitHub read the caller made itself, for
/// the worktrees and stages `wants` names.
pub async fn landing_facts(
    ctx: &AppContext,
    rows: Vec<WorktreeRecord>,
    state: &serde_json::Value,
    github: &GithubData,
    wants: &BTreeMap<String, WatchWants>,
) -> Result<AutomationLifecycleEvidence> {
    let lifecycle = service(ctx)?;
    // Timer-driven: record what is at hand and ask GitHub nothing more.
    crate::other_branches::observe(
        ctx,
        &rows,
        state,
        Some(github),
        crate::other_branches::Ask::Nothing,
    )
    .await;
    let mut published_bases = BTreeMap::new();
    let mut warning = None;
    let mut evidence = AutomationLifecycleEvidence::default();
    for row in rows {
        if row.is_main {
            continue;
        }
        let slug = row.target.slug().to_owned();
        let stored = &state["slugs"][row.target.slug()];
        let landing = landing_evidence(
            ctx,
            &row,
            stored,
            github,
            &mut published_bases,
            &mut warning,
        )
        .await;
        let landing = match landing {
            Ok(landing) => landing,
            Err(error) if !ctx.cancellation.is_cancelled() => {
                evidence.row_errors.insert(slug, format!("{error:#}"));
                continue;
            }
            Err(error) => return Err(error),
        };
        let clean_candidate = if landing.landed() && !wt_lifecycle::checkout_missing(&row).await {
            let base_tip = landing
                .on_published_base()
                .then(|| published_bases.get(proof_repository(ctx, Path::new(&row.target.path))))
                .flatten()
                .and_then(Option::clone);
            match lifecycle
                .removal_hazards_on(&row.target, true, base_tip.as_deref(), &ctx.cancellation)
                .await
            {
                Ok(hazards) => hazards.is_empty(),
                Err(error) if !ctx.cancellation.is_cancelled() => {
                    evidence
                        .row_errors
                        .insert(slug, format!("{:#}", anyhow::Error::from(error)));
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            false
        };
        let owed = landing.landed().then(|| owed_check_stages(stored));
        // A watch asks the same questions an owed check does, of the stages
        // its conditions name.
        let watched = wants.get(&slug).copied().filter(|_| landing.landed());
        let asked = |stage| {
            owed.is_some_and(|owed| owed.stage(stage))
                || watched.is_some_and(|watched| watched.stage(stage))
        };
        let StageReach {
            in_production,
            deploys,
            commits,
        } = stage_reach(ctx, &row, github, asked, watched.is_some()).await;
        // The merge commit when the PR merged at HEAD, else HEAD itself.
        if watched.is_some()
            && let Some(commit) = commits.as_ref().and_then(|commits| commits.last())
        {
            evidence
                .landing_commits
                .insert(slug.clone(), commit.clone());
        }
        evidence.rows.insert(
            slug,
            LifecycleFacts {
                local_merged: landing.on_published_base().then_some(true),
                clean_candidate,
                in_production,
                deploys,
            },
        );
    }
    if let Some(warning) = warning {
        tracing::debug!(%warning, "automation landing evidence is partial");
    }
    Ok(evidence)
}

/// How far a landed row's work travelled past the base branch.
struct StageReach {
    in_production: bool,
    deploys: wt_core::StageDeploys,
    /// The row's landing commits, read only when something needed them.
    commits: Option<Vec<String>>,
}

/// Production and deploy proof for the stages `asked` about. The probes
/// cost several Git calls per row, so a stage nobody asks about is not
/// read: it reads not promoted and, where it has a probe, not deployed.
async fn stage_reach(
    ctx: &AppContext,
    row: &WorktreeRecord,
    github: &GithubData,
    asked: impl Fn(wt_core::VerifyStage) -> bool,
    want_commits: bool,
) -> StageReach {
    let separate_production = ctx.config.branch.separate_production();
    let in_production = separate_production
        && asked(wt_core::VerifyStage::Production)
        && reached_production(ctx, row, github).await;
    let probed = wt_core::VerifyStage::ALL
        .into_iter()
        .filter(|stage| asked(*stage) && ctx.config.environments.get(*stage).is_some())
        .collect::<Vec<_>>();
    let commits = if want_commits || !probed.is_empty() {
        landing_commits(ctx, row, github).await
    } else {
        None
    };
    // A probed stage nobody asked about is still not shown deployed.
    let mut deploys = wt_core::StageDeploys::default()
        .for_probes(|stage| ctx.config.environments.get(stage).is_some());
    for stage in probed {
        // A deploy cannot be ahead of the Git landing it ships.
        let landed = match stage {
            wt_core::VerifyStage::Staging => true,
            wt_core::VerifyStage::Production => in_production || !separate_production,
        };
        let deployed = landed
            && match &commits {
                Some(commits) => {
                    crate::deploys::landing_deployed(ctx, stage, commits, &ctx.cancellation).await
                        == Some(true)
                }
                None => false,
            };
        deploys.set(stage, Some(deployed));
    }
    StageReach {
        in_production,
        deploys,
        commits,
    }
}

/// What this process can prove of each row's landing, for saying what the
/// row owes: the base landing, and production and deploy proof for the
/// stages its checks name. `github` is the caller's own fresh read; without
/// one only Git ancestry proves a landing, or the row's own history where it
/// records the pull request merged and the work landed at the current HEAD
/// (see [`history_landed`]). A squash-merged row with neither reads as not
/// landed. A row whose evidence cannot be read is left out. Display only:
/// removal takes its proof from [`plan`].
pub async fn row_landings(
    ctx: &AppContext,
    rows: &[WorktreeRecord],
    state: &serde_json::Value,
    github: Option<&GithubData>,
) -> BTreeMap<String, wt_core::Landing> {
    let empty = GithubData::default();
    let github = github.unwrap_or(&empty);
    let mut published_bases = BTreeMap::new();
    let mut warning = None;
    let mut landings = BTreeMap::new();
    let mut read = Vec::new();
    for row in rows.iter().filter(|row| !row.is_main) {
        let stored = &state["slugs"][row.target.slug()];
        let Ok(landing) =
            landing_evidence(ctx, row, stored, github, &mut published_bases, &mut warning).await
        else {
            continue;
        };
        read.push((row, landing.landed()));
    }
    // GitHub not returning a pull request is no answer about it. One that
    // it returns open or closed is, and stands against the history.
    let silent: Vec<String> = read
        .iter()
        .filter(|(row, landed)| !landed && !github.prs.contains_key(&row.target.branch))
        .map(|(row, _)| row.target.slug().to_owned())
        .collect();
    let history = if silent.is_empty() {
        BTreeMap::new()
    } else {
        ctx.database
            .call(move |store| Ok(store.read_worktree_events_for(&silent, HISTORY_PROOF_EVENTS)?))
            .await
            .unwrap_or_default()
    };
    for (row, landed) in read {
        let slug = row.target.slug();
        let stored = &state["slugs"][slug];
        let landed = landed
            || match history.get(slug) {
                Some(events) => history_landed(ctx, row, events).await,
                None => false,
            };
        let owed = landed.then(|| owed_check_stages(stored));
        let asked = |stage| owed.is_some_and(|owed| owed.stage(stage));
        let reach = stage_reach(ctx, row, github, asked, false).await;
        landings.insert(
            slug.to_owned(),
            wt_core::Landing::new(
                landed,
                reach.in_production,
                ctx.config.branch.separate_production(),
            )
            .with_deploys(reach.deploys),
        );
    }
    landings
}

/// How much of a row's newest history is read for [`history_landed`].
const HISTORY_PROOF_EVENTS: usize = 200;

/// Whether the row's own history proves the landing GitHub is silent about:
/// it records the pull request merged, and the work landed at the HEAD the
/// checkout is on now (see [`wt_core::history_landed_at`]). A stopped rebase
/// or merge has a transient HEAD and proves nothing.
async fn history_landed(
    ctx: &AppContext,
    row: &WorktreeRecord,
    events: &[wt_core::WorktreeEvent],
) -> bool {
    if wt_vcs::operation_in_progress(row.git_dir.as_deref()).await
        || wt_lifecycle::checkout_missing(row).await
    {
        return false;
    }
    let head = run_git(
        ctx,
        Path::new(&row.target.path),
        ["rev-parse", "--verify", "HEAD"],
    )
    .await
    .ok()
    .filter(|output| output.status.success())
    .map(|output| output.stdout_text().trim().to_owned());
    head.is_some_and(|head| wt_core::history_landed_at(events, &head))
}

/// The board's "in production" proof: HEAD, or the exact merge commit of the
/// PR merged at HEAD, is contained in the production branch. The production
/// branch is in `keep_fresh`, so its local head is current; its remote-tracking
/// ref is the fallback. Any failure is unknown, which reads as not yet.
async fn reached_production(ctx: &AppContext, row: &WorktreeRecord, github: &GithubData) -> bool {
    let Some(production) = ctx.config.branch.production.as_deref() else {
        return false;
    };
    let path = Path::new(&row.target.path);
    let mut production_ref = None;
    for candidate in [production.to_owned(), format!("origin/{production}")] {
        let spec = format!("{candidate}^{{commit}}");
        if let Ok(output) = run_git(ctx, path, ["rev-parse", "--verify", "--quiet", &spec]).await
            && output.status.success()
        {
            production_ref = Some(candidate);
            break;
        }
    }
    let Some(production_ref) = production_ref else {
        tracing::debug!(
            slug = row.target.slug(),
            production,
            "production branch unresolved; landing reads as not yet promoted"
        );
        return false;
    };
    let Some(commits) = landing_commits(ctx, row, github).await else {
        return false;
    };
    for commit in commits {
        if let Ok(output) = run_git(
            ctx,
            path,
            ["merge-base", "--is-ancestor", &commit, &production_ref],
        )
        .await
            && output.status.success()
        {
            return true;
        }
    }
    false
}

/// The commits that prove where the row's work landed: HEAD, and the exact
/// merge commit of the PR merged at HEAD. `None` when HEAD cannot be read.
async fn landing_commits(
    ctx: &AppContext,
    row: &WorktreeRecord,
    github: &GithubData,
) -> Option<Vec<String>> {
    let path = Path::new(&row.target.path);
    let head = run_git(ctx, path, ["rev-parse", "--verify", "HEAD"])
        .await
        .ok()
        .filter(|output| output.status.success())?
        .stdout_text()
        .trim()
        .to_owned();
    let merge = github.prs.get(&row.target.branch).and_then(|pr| {
        wt_core::exact_merged_pr_oid(
            pr.head_ref_oid.as_deref(),
            &head,
            &pr.state,
            pr.merge_commit_oid.as_deref(),
        )
        .map(str::to_owned)
    });
    Some(std::iter::once(head).chain(merge).collect())
}

/// The stages of the row's undischarged checks. The production and deploy
/// probes cost several Git calls per row, so they run only where their
/// answer decides something: those stages.
fn owed_check_stages(stored: &serde_json::Value) -> WatchWants {
    let mut wants = WatchWants::default();
    for (stage, _) in wt_core::parse_work_status(&stored["work"])
        .map(|work| work.open_checks())
        .unwrap_or_default()
    {
        match stage {
            wt_core::VerifyStage::Staging => wants.staging = true,
            wt_core::VerifyStage::Production => wants.production = true,
        }
    }
    wants
}

/// The worktrees that owe a post-merge check, by slug, with the stages
/// owed: their landing is worth reading even where no rule asks for it,
/// because wt itself wakes a session whose check came due.
pub fn check_wants(state: &serde_json::Value) -> BTreeMap<String, WatchWants> {
    state["slugs"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(slug, stored)| {
            let wants = owed_check_stages(stored);
            (wants != WatchWants::default()).then(|| (slug.clone(), wants))
        })
        .collect()
}

#[derive(Default)]
struct Landing {
    pr_landed: bool,
    local_merged: bool,
    /// The pull request merged from an earlier head and the branch was
    /// rebased since: every commit it has beyond the advertised base is one
    /// whose patch that base already holds.
    patch_merged: bool,
}

impl Landing {
    fn landed(&self) -> bool {
        self.pr_landed || self.local_merged || self.patch_merged
    }

    /// Whether the proof names the base tip origin advertised, which the
    /// removal then carries as its witness.
    fn on_published_base(&self) -> bool {
        self.local_merged || self.patch_merged
    }
}

/// The single landing proof shared by explicit cleanup and automation facts.
async fn landing_evidence(
    ctx: &AppContext,
    row: &WorktreeRecord,
    stored: &serde_json::Value,
    github: &GithubData,
    published_bases: &mut BTreeMap<PathBuf, Option<String>>,
    warning: &mut Option<String>,
) -> Result<Landing> {
    let path = Path::new(&row.target.path);
    // A stopped rebase or merge has a transient HEAD; it proves nothing.
    // Without a checkout there is no HEAD to prove anything with.
    if wt_vcs::operation_in_progress(row.git_dir.as_deref()).await
        || wt_lifecycle::checkout_missing(row).await
    {
        return Ok(Landing::default());
    }
    let head = run_git(ctx, path, ["rev-parse", "--verify", "HEAD"])
        .await?
        .checked("git")?
        .stdout_text()
        .trim()
        .to_owned();
    let own_work = if let Some(base_sha) = stored["baseSha"].as_str() {
        // A corrupted fork anchor is unknown, never evidence of landing. An
        // empty branch must not inherit its parent's landing.
        let own = run_git(
            ctx,
            path,
            ["rev-list", "--count", &format!("{base_sha}..HEAD")],
        )
        .await?;
        own.status.success() && own.stdout_text().trim().parse::<u64>().is_ok_and(|n| n > 0)
    } else {
        // A worktree adopted from an existing branch before adoption
        // recorded an anchor has none. The proofs below are then all there
        // is, and each is one an empty branch cannot give: a pull request
        // merged at this HEAD, or a HEAD the base holds off its own line.
        true
    };
    let merged_pr = github
        .prs
        .get(&row.target.branch)
        .filter(|pr| pr.state == "MERGED");
    let pr_landed =
        own_work && merged_pr.is_some_and(|pr| pr.head_ref_oid.as_deref() == Some(head.as_str()));
    let local_merged = own_work
        && !pr_landed
        && published_base_contains(
            ctx,
            path,
            &head,
            &row.target.branch,
            published_bases,
            warning,
        )
        .await?;
    let patch_merged = own_work
        && !pr_landed
        && !local_merged
        && merged_pr.is_some()
        && published_base_holds_patches(ctx, path, &head, published_bases, warning).await?;
    Ok(Landing {
        pr_landed,
        local_merged,
        patch_merged,
    })
}

/// `github` is either this process's fresh fetch or confirmed leaf data; the
/// public entry points never accept displayed or restored data.
async fn plan_with_evidence(
    ctx: &AppContext,
    rows: Vec<WorktreeRecord>,
    state: &serde_json::Value,
    github: &GithubData,
    mut warning: Option<String>,
) -> Result<RemovalPlans> {
    let lifecycle = service(ctx)?;
    let mut plans = Vec::with_capacity(rows.len());
    let mut published_bases = BTreeMap::new();
    for row in rows {
        if row.is_main {
            continue;
        }
        let result: Result<RemovalPlan> = async {
            let path = Path::new(&row.target.path);
            let stored = &state["slugs"][row.target.slug()];
            let landing = landing_evidence(
                ctx,
                &row,
                stored,
                github,
                &mut published_bases,
                &mut warning,
            )
            .await?;
            let landed = landing.landed();
            let pr_landed = landing.pr_landed;
            let mut extra = serde_json::Map::new();
            let pr = github.prs.get(&row.target.branch);
            if let Some(title) = stored["manualTitle"]
                .as_str()
                .filter(|title| !title.trim().is_empty())
                .or_else(|| {
                    pr.map(|pr| pr.title.as_str())
                        .filter(|title| !title.trim().is_empty())
                })
            {
                extra.insert("title".into(), serde_json::json!(title));
            }
            if let Some(issue_id) =
                wt_app_core::issue_identity::resolve(row.target.slug(), stored["issueId"].as_str())
            {
                extra.insert("issueId".into(), serde_json::json!(issue_id));
            }
            if let Some(github_issue) = stored.get("githubIssue") {
                extra.insert("githubIssue".into(), github_issue.clone());
            }
            // The removal record is where the worktree's other branches
            // stay findable once its reflog is gone.
            if let Some(others) = stored
                .get(wt_core::OTHER_BRANCHES)
                .filter(|others| others.as_object().is_some_and(|others| !others.is_empty()))
            {
                extra.insert(wt_core::OTHER_BRANCHES.into(), others.clone());
            }
            if landed {
                extra.insert("gitState".into(), serde_json::json!("merged"));
            }
            // A branch may have advanced after an older PR merged. Persist the
            // current revision's verdict so historical PR metadata cannot turn
            // those unlanded commits into a claim that all work landed.
            extra.insert(
                "landedOnAtRemoval".into(),
                serde_json::json!(if landed { "base" } else { "unlanded" }),
            );
            if let Some(pr) = pr {
                extra.insert("prNumber".into(), serde_json::json!(pr.number));
                extra.insert("prUrl".into(), serde_json::json!(pr.url));
                extra.insert("prState".into(), serde_json::json!(pr.state));
                if pr_landed && let Some(merge_oid) = &pr.merge_commit_oid {
                    extra.insert("prMergeCommitOid".into(), serde_json::json!(merge_oid));
                }
            }
            let removed_snapshot = wt_store::RemovedWorktree {
                slug: row.target.slug().to_owned(),
                branch: row.target.branch.clone(),
                removed_at: String::new(),
                work: None,
                automations_paused: None,
                extra,
            };
            let base_tip = landing
                .on_published_base()
                .then(|| published_bases.get(proof_repository(ctx, path)))
                .flatten()
                .and_then(Option::clone);
            let mut revision = lifecycle
                .removal_revision_on(&row.target, landed, base_tip.as_deref(), &ctx.cancellation)
                .await?;
            revision.published_base = base_tip
                .map(|tip| Box::new((format!("refs/heads/{}", ctx.config.branch.base), tip)));
            let hazards = revision.hazards.clone();
            let stage_path = path.to_path_buf();
            let stage = row.target.stage.clone();
            let prefix = ctx.config.stage.prefix.clone();
            let default_stage = ctx.config.stage.default_personal.clone();
            let has_sst = ctx.config.sst.is_some();
            let destroy_stage = tokio::task::spawn_blocking(move || {
                has_sst
                    && stage != default_stage
                    && crate::commands::remove::is_our_stage_deployed(&stage_path, &stage, &prefix)
            })
            .await?;
            Ok(RemovalPlan {
                row,
                landed,
                hazards,
                destroy_stage,
                revision,
                removed_snapshot,
            })
        }
        .await;
        plans.push(result?);
    }
    Ok(RemovalPlans {
        rows: plans,
        warning,
    })
}

/// A cached ancestry match only nominates a candidate. Confirm that the origin
/// still advertises a containing base tip before it can waive the unpushed-work
/// guard. Query once per object store in a batch, without changing any checkout,
/// fetching every branch, or running keep-fresh/install hooks.
async fn published_base_contains(
    ctx: &AppContext,
    path: &Path,
    head: &str,
    branch: &str,
    published: &mut BTreeMap<PathBuf, Option<String>>,
    warning: &mut Option<String>,
) -> Result<bool> {
    let reference = format!("refs/remotes/origin/{}", ctx.config.branch.base);
    let cached = run_git(ctx, path, ["merge-base", "--is-ancestor", head, &reference]).await?;
    if !cached.status.success() {
        return Ok(false);
    }
    let Some(tip) = published_tip(ctx, path, published, warning).await? else {
        return Ok(false);
    };
    // A newly advertised object may not have been fetched yet. Missing objects
    // are unknown, not a reason to trust the older local base.
    if !run_git(ctx, path, ["merge-base", "--is-ancestor", head, &tip])
        .await?
        .status
        .success()
    {
        return Ok(false);
    }
    // Contained is not landed when HEAD is only a snapshot of the base: a
    // lagging fork anchor would otherwise count base commits as own work.
    let line = run_git(ctx, path, wt_vcs::mainline_args(&tip, head)).await?;
    if !line.status.success() || line.stdout_truncated {
        return Ok(true);
    }
    let reflog = run_git(ctx, path, wt_vcs::branch_reflog_args(branch)).await?;
    let reflog = if reflog.status.success() {
        reflog.stdout_text().to_string()
    } else {
        String::new()
    };
    Ok(wt_vcs::contained_head_landed(
        &line.stdout_text(),
        &reflog,
        head,
    ))
}

/// Whether the base tip origin advertises holds, by patch, every commit
/// `head` has beyond it, and there is at least one. A tip not fetched yet
/// or a failed read is unknown, which is not landed.
async fn published_base_holds_patches(
    ctx: &AppContext,
    path: &Path,
    head: &str,
    published: &mut BTreeMap<PathBuf, Option<String>>,
    warning: &mut Option<String>,
) -> Result<bool> {
    let Some(tip) = published_tip(ctx, path, published, warning).await? else {
        return Ok(false);
    };
    let held = wt_vcs::base_holds_everything(&tip, head, None, |args| async {
        let output = run_git(ctx, path, args).await.ok()?;
        (!output.stdout_truncated).then(|| wt_vcs::GitRead {
            code: output.status.code(),
            stdout: output.stdout_text(),
        })
    })
    .await;
    Ok(matches!(held, wt_vcs::BeyondBase::Nothing(commits) if commits > 0))
}

/// The object store whose origin a landing proof is read from and rechecked
/// against: linked worktrees share the main clone's, a Rift clone has its own.
fn proof_repository<'a>(ctx: &'a AppContext, path: &'a Path) -> &'a Path {
    match ctx.config.backend.kind {
        wt_config::BackendKind::GitWorktree => ctx.config.paths.main_clone.as_path(),
        wt_config::BackendKind::Rift => path,
    }
}

/// The base tip origin advertises now, asked once per object store in a
/// batch. `None` when origin could not say.
async fn published_tip(
    ctx: &AppContext,
    path: &Path,
    published: &mut BTreeMap<PathBuf, Option<String>>,
    warning: &mut Option<String>,
) -> Result<Option<String>> {
    let repository = proof_repository(ctx, path);
    if !published.contains_key(repository) {
        let remote_ref = format!("refs/heads/{}", ctx.config.branch.base);
        let result = run_git(
            ctx,
            repository,
            ["ls-remote", "--exit-code", "--refs", "origin", &remote_ref],
        )
        .await?;
        let tip = if result.status.success() {
            result.stdout_text().lines().find_map(|line| {
                let (oid, name) = line.split_once('\t')?;
                (name == remote_ref
                    && matches!(oid.len(), 40 | 64)
                    && oid.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then(|| oid.to_owned())
            })
        } else {
            None
        };
        if tip.is_none() {
            let detail = format!(
                "Current origin/{} landing proof unavailable; cached refs are not sufficient",
                ctx.config.branch.base
            );
            *warning = Some(match warning.take() {
                Some(previous) => format!("{previous}; {detail}"),
                None => detail,
            });
        }
        published.insert(repository.to_path_buf(), tip);
    }
    Ok(published.get(repository).cloned().flatten())
}

/// Re-plan the exact confirmed set. Never expand a user's confirmation because
/// another row became eligible while the dialog was open.
pub async fn cleanup_confirmed(
    ctx: &AppContext,
    confirmed: &[wt_tui::RemovalRevision],
) -> Result<String> {
    cleanup_with_evidence(ctx, confirmed, Evidence::Fresh, None).await
}

/// Re-plan with the host's confirmed GitHub evidence instead of a new fetch.
/// `None` proves nothing, so PR-only landings are kept.
pub async fn cleanup_cached(
    ctx: &AppContext,
    confirmed: &[wt_tui::RemovalRevision],
    github: Option<&Confirmed<GithubData>>,
) -> Result<String> {
    cleanup_with_evidence(ctx, confirmed, Evidence::Cached(github), None).await
}

/// Check the exact board-selected rows after confirmation. Only confirmed
/// GitHub evidence can prove a PR landing; local hazards are checked again.
pub async fn cleanup_targets_cached(
    ctx: &AppContext,
    targets: &[wt_tui::CleanupTarget],
    github: Option<&Confirmed<GithubData>>,
) -> Result<String> {
    cleanup_with_evidence(ctx, &[], Evidence::Cached(github), Some(targets)).await
}

enum Evidence<'a> {
    Fresh,
    Cached(Option<&'a Confirmed<GithubData>>),
}

async fn cleanup_with_evidence(
    ctx: &AppContext,
    confirmed: &[wt_tui::RemovalRevision],
    evidence: Evidence<'_>,
    targets: Option<&[wt_tui::CleanupTarget]>,
) -> Result<String> {
    let live = ctx.repository.inventory(&ctx.cancellation).await?;
    let selected = live
        .into_iter()
        .filter(|row| {
            targets.is_some_and(|targets| targets.iter().any(|item| target_matches(row, item)))
                || confirmed.iter().any(|item| {
                    item.key == wt_core::worktree_target_key(&row.target)
                        && item.path == row.target.path
                        && item.branch == row.target.branch
                        && item.head == row.head_sha.as_deref().unwrap_or_default()
                })
        })
        .collect();
    let plans = match evidence {
        Evidence::Cached(github) => plan_cached(ctx, selected, github).await?,
        Evidence::Fresh => plan(ctx, selected).await?,
    };
    let mut by_key: BTreeMap<_, _> = plans
        .rows
        .into_iter()
        .map(|plan| (wt_core::worktree_target_key(&plan.row.target), plan))
        .collect();
    let mut requests = Vec::new();
    let mut kept = Vec::new();
    // Board selections carry identity only. Current hazards and landing proof
    // must pass before the worker receives its locked removal revision.
    let selections = if let Some(targets) = targets {
        targets
            .iter()
            .map(CleanupSelection::Target)
            .collect::<Vec<_>>()
    } else {
        confirmed.iter().map(CleanupSelection::Revision).collect()
    };
    for expected in selections {
        let key = expected.key();
        match by_key.remove(key) {
            Some(plan)
                if expected.matches_revision(&plan.revision)
                    && plan.landed
                    && plan.hazards.is_empty() =>
            {
                requests.push((
                    plan.row,
                    crate::commands::_destroy::DestroyOptions {
                        force: false,
                        delete_branch: true,
                        landed: true,
                        destroy_stage: plan.destroy_stage,
                        expected_revision: Some(plan.revision),
                        removed_snapshot: Some(plan.removed_snapshot),
                    },
                ))
            }
            Some(plan) if !expected.matches_revision(&plan.revision) => kept.push(format!(
                "{}: checkout changed after confirmation",
                plan.row.target.slug()
            )),
            Some(plan) => kept.push(format!(
                "{}: {}",
                plan.row.target.slug(),
                if plan.hazards.is_empty() {
                    "landing no longer proven".into()
                } else {
                    plan.hazards.join(", ")
                }
            )),
            None => kept.push(format!("{}: no longer present or identity changed", key)),
        }
    }
    let mut removed = 0;
    let mut uncertain = false;
    for (request, result) in requests
        .iter()
        .zip(crate::commands::_destroy::start_removals(ctx, &requests).await)
    {
        match result {
            Ok(job) => match crate::commands::_destroy::wait_for_completion(ctx, &job).await {
                Ok(()) => removed += 1,
                Err(error) => {
                    uncertain |= error
                        .downcast_ref::<crate::commands::_destroy::RemovalUncertain>()
                        .is_some();
                    kept.push(format!("{}: {error:#}", request.0.target.slug()));
                }
            },
            Err(error) => kept.push(format!("{}: {error:#}", request.0.target.slug())),
        }
    }
    if !kept.is_empty() {
        if uncertain {
            return Err(crate::commands::_destroy::RemovalUncertain(format!(
                "Removed {removed}; unresolved: {}",
                kept.join("; ")
            ))
            .into());
        }
        bail!("Removed {removed}; kept {}", kept.join("; "));
    }
    Ok(format!("Removed {removed} worktrees"))
}

enum CleanupSelection<'a> {
    Revision(&'a wt_tui::RemovalRevision),
    Target(&'a wt_tui::CleanupTarget),
}

impl CleanupSelection<'_> {
    fn key(&self) -> &str {
        match self {
            Self::Revision(revision) => &revision.key,
            Self::Target(target) => &target.key,
        }
    }

    fn matches_revision(&self, revision: &wt_lifecycle::RemovalRevision) -> bool {
        match self {
            Self::Revision(expected) => same_revision(revision, expected),
            Self::Target(target) => {
                target.key == revision.key
                    && target.path == revision.path
                    && target.branch == revision.branch
                    && target.head == revision.head
                    && !target.head.is_empty()
            }
        }
    }
}

fn target_matches(row: &WorktreeRecord, target: &wt_tui::CleanupTarget) -> bool {
    target.key == wt_core::worktree_target_key(&row.target)
        && target.path == row.target.path
        && target.branch == row.target.branch
        && Some(target.head.as_str()) == row.head_sha.as_deref()
        && !target.head.is_empty()
}

fn same_revision(plan: &wt_lifecycle::RemovalRevision, expected: &wt_tui::RemovalRevision) -> bool {
    plan.key == expected.key
        && plan.path == expected.path
        && plan.branch == expected.branch
        && plan.head == expected.head
        && plan.digest == expected.digest
        && plan.hazards == expected.hazards
        && plan.published_base == expected.published_base
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_app_core::test_support::CommandFixture;

    #[test]
    fn board_confirmation_rejects_head_changes_during_planning() {
        let target = wt_tui::CleanupTarget {
            key: "one".into(),
            path: "/worktrees/one".into(),
            branch: "feature/one".into(),
            head: "old-head".into(),
        };
        let mut revision = wt_lifecycle::RemovalRevision {
            key: target.key.clone(),
            path: target.path.clone(),
            branch: target.branch.clone(),
            head: target.head.clone(),
            digest: "current-digest".into(),
            hazards: vec![],
            published_base: None,
        };
        let selected = CleanupSelection::Target(&target);
        assert!(selected.matches_revision(&revision));
        revision.head = "new-head".into();
        assert!(!selected.matches_revision(&revision));
    }

    async fn git(ctx: &AppContext, path: &Path, args: &[&str]) -> String {
        run_git(ctx, path, args.iter().copied())
            .await
            .unwrap()
            .checked("fixture git")
            .unwrap()
            .stdout_text()
            .trim()
            .to_owned()
    }

    /// Worktree "one" with `origin` added and its HEAD pushed as the base.
    /// Returns the origin path and that base commit.
    async fn with_origin(fixture: &CommandFixture) -> (PathBuf, String) {
        let ctx = &fixture.ctx;
        let origin = fixture._root.path().join("origin.git");
        git(ctx, &ctx.cwd, &["init", "--bare", origin.to_str().unwrap()]).await;
        git(
            ctx,
            &ctx.cwd,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        )
        .await;
        git(ctx, &ctx.cwd, &["push", "origin", "HEAD:refs/heads/main"]).await;
        let base = git(ctx, &ctx.cwd, &["rev-parse", "HEAD"]).await;
        (origin, base)
    }

    async fn row_one(ctx: &AppContext) -> WorktreeRecord {
        ctx.repository
            .inventory(&ctx.cancellation)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.target.slug() == "one")
            .unwrap()
    }

    /// Commits "feature work" on worktree "one" and pushes its branch, then
    /// moves the base on and merges that pushed head into it with a merge
    /// commit. Returns the pushed head and the base commit before the merge.
    async fn merged_with_a_merge_commit(fixture: &CommandFixture, base: &str) -> (String, String) {
        let ctx = &fixture.ctx;
        let path = &ctx.cwd;
        let branch = row_one(ctx).await.target.branch;
        std::fs::write(path.join("work.txt"), "merged work\n").unwrap();
        git(ctx, path, &["add", "work.txt"]).await;
        git(ctx, path, &["commit", "-m", "feature work"]).await;
        let pushed = git(ctx, path, &["rev-parse", "HEAD"]).await;
        git(
            ctx,
            path,
            &["push", "origin", &format!("HEAD:refs/heads/{branch}")],
        )
        .await;
        git(ctx, path, &["checkout", "-q", "--detach", base]).await;
        std::fs::write(path.join("other.txt"), "base moved\n").unwrap();
        git(ctx, path, &["add", "other.txt"]).await;
        git(ctx, path, &["commit", "-m", "base moves"]).await;
        let moved = git(ctx, path, &["rev-parse", "HEAD"]).await;
        git(ctx, path, &["merge", "--no-ff", "-m", "merge", &pushed]).await;
        git(ctx, path, &["push", "origin", "HEAD:refs/heads/main"]).await;
        git(ctx, path, &["fetch", "-q", "origin"]).await;
        git(ctx, path, &["checkout", "-q", &branch]).await;
        (pushed, moved)
    }

    fn merged_pr(branch: &str, head: &str) -> GithubData {
        let mut data = GithubData::default();
        data.prs.insert(
            branch.to_owned(),
            serde_json::from_value(serde_json::json!({
                "number": 42, "url": "https://example.invalid/pull/42",
                "headRefName": branch, "headRefOid": head,
                "baseRefName": "main", "mergeCommitOid": "f".repeat(40),
                "title": "Feature", "isDraft": false, "state": "MERGED",
                "checks": "pass", "failedChecks": [], "review": "none",
                "reviewRequests": 0, "requestedReviewers": [], "suggestedReviewers": [],
                "comments": [], "unresolvedThreads": 0, "unresolvedThreadsTotal": 0
            }))
            .unwrap(),
        );
        data.covered_branches.insert(branch.to_owned());
        data
    }

    /// The pull request merged from the pushed head; the branch was then
    /// rebased onto the newer base and never pushed. HEAD is a new commit
    /// that no base contains, yet the base holds its patch.
    #[tokio::test]
    async fn a_branch_rebased_after_its_pull_request_merged_is_landed_and_removable() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let path = &ctx.cwd;
        let (_origin, base) = with_origin(&fixture).await;
        let anchor = base.clone();
        ctx.database
            .call(move |store| {
                store.set_slug_base("one", Some(("main", Some(&anchor))))?;
                Ok(())
            })
            .await
            .unwrap();
        let (pushed, moved) = merged_with_a_merge_commit(&fixture, &base).await;
        git(ctx, path, &["rebase", "-q", &moved]).await;
        let head = git(ctx, path, &["rev-parse", "HEAD"]).await;
        assert_ne!(head, pushed);
        let tip = git(ctx, path, &["rev-parse", "origin/main"]).await;
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let row = row_one(ctx).await;
        let github = merged_pr(&row.target.branch, &pushed);
        let plan = || async {
            plan_with_evidence(ctx, vec![row_one(ctx).await], &state, &github, None)
                .await
                .unwrap()
                .rows
                .remove(0)
        };

        let landed = plan().await;
        assert!(landed.landed);
        assert!(landed.hazards.is_empty(), "{:?}", landed.hazards);
        assert_eq!(landed.revision.published_base.as_ref().unwrap().1, tip);
        assert!(
            row_landings(ctx, std::slice::from_ref(&row), &state, Some(&github)).await["one"].base
        );
        // The merge is the proof that the work is this branch's: without
        // it, matching patches alone land nothing.
        let unproven = plan_with_evidence(ctx, vec![row], &state, &GithubData::default(), None)
            .await
            .unwrap();
        assert!(!unproven.rows[0].landed);

        // One commit the base does not hold is work that has not landed.
        std::fs::write(path.join("work.txt"), "after the merge\n").unwrap();
        git(ctx, path, &["commit", "-am", "local only"]).await;
        let advanced = plan().await;
        assert!(!advanced.landed);
        assert!(!advanced.hazards.is_empty());
        git(ctx, path, &["reset", "-q", "--hard", "HEAD~1"]).await;
        assert!(plan().await.landed);

        // A base that reverted the work no longer holds it, though the
        // reverted patch still matches the branch's commit.
        let branch = row_one(ctx).await.target.branch;
        git(ctx, path, &["checkout", "-q", "--detach", "origin/main"]).await;
        git(ctx, path, &["revert", "--no-edit", &pushed]).await;
        git(ctx, path, &["push", "origin", "HEAD:refs/heads/main"]).await;
        git(ctx, path, &["fetch", "-q", "origin"]).await;
        git(ctx, path, &["checkout", "-q", &branch]).await;
        let reverted = plan().await;
        assert!(!reverted.landed);
        assert!(!reverted.hazards.is_empty());
        fixture.close().await.unwrap();
    }

    /// A squash merge leaves no ancestry, and a later GitHub read may not
    /// return the merged pull request. The row's own history then proves
    /// the landing, at the HEAD it recorded and nowhere else, and never
    /// against a pull request GitHub returns unmerged.
    #[tokio::test]
    async fn history_proves_a_merged_landing_github_no_longer_returns() {
        use wt_core::{LandedOn, PrChange, WorktreeEventKind};
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let path = &ctx.cwd;
        with_origin(&fixture).await;
        std::fs::write(path.join("work.txt"), "squashed work\n").unwrap();
        git(ctx, path, &["add", "work.txt"]).await;
        git(ctx, path, &["commit", "-m", "feature work"]).await;
        let head = git(ctx, path, &["rev-parse", "HEAD"]).await;
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let row = row_one(ctx).await;
        let landed = |github: Option<GithubData>| {
            let row = row.clone();
            let state = state.clone();
            async move {
                row_landings(ctx, std::slice::from_ref(&row), &state, github.as_ref()).await["one"]
                    .base
            }
        };
        assert!(!landed(None).await);
        let record = |event: WorktreeEventKind| async move {
            ctx.database
                .call(move |store| Ok(store.append_worktree_event("one", 1, &event)?))
                .await
                .unwrap();
        };
        record(WorktreeEventKind::Pr {
            number: 42,
            change: PrChange::Merged,
        })
        .await;
        // A landing recorded without its HEAD says nothing of this one.
        record(WorktreeEventKind::Landed {
            on: LandedOn::Base,
            head: None,
            landed_at_ms: None,
        })
        .await;
        assert!(!landed(None).await);
        record(WorktreeEventKind::Landed {
            on: LandedOn::Base,
            head: Some(head.clone()),
            landed_at_ms: None,
        })
        .await;
        assert!(landed(None).await);
        // A read that covers the branch and returns no pull request is
        // silent, not a contradiction.
        let mut silent = GithubData::default();
        silent.covered_branches.insert(row.target.branch.clone());
        assert!(landed(Some(silent)).await);
        // GitHub's own answer that the pull request is open wins.
        let mut open = merged_pr(&row.target.branch, &head);
        open.prs.get_mut(&row.target.branch).unwrap().state = "OPEN".into();
        assert!(!landed(Some(open)).await);
        // Removal never takes this proof.
        let plan = plan_with_evidence(ctx, vec![row.clone()], &state, &GithubData::default(), None)
            .await
            .unwrap();
        assert!(!plan.rows[0].landed);
        // Work past the recorded HEAD has not landed.
        std::fs::write(path.join("work.txt"), "after the merge\n").unwrap();
        git(ctx, path, &["commit", "-am", "local only"]).await;
        assert!(!landed(None).await);
        fixture.close().await.unwrap();
    }

    /// A worktree adopted from an existing branch before adoption recorded
    /// an anchor has no fork-base record. Its merged HEAD still lands, and
    /// a branch with nothing of its own still does not.
    #[tokio::test]
    async fn a_worktree_without_a_fork_anchor_lands_by_what_the_base_holds() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let (_origin, base) = with_origin(&fixture).await;
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert!(state["slugs"]["one"]["baseSha"].is_null());
        let landed = || async {
            plan_with_facts(ctx, vec![row_one(ctx).await], &state, None, None)
                .await
                .unwrap()
                .rows[0]
                .landed
        };
        // HEAD is the base tip itself: a snapshot, not work that landed.
        assert!(!landed().await);
        merged_with_a_merge_commit(&fixture, &base).await;
        assert!(landed().await);
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn removal_landing_requires_the_current_published_base() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let path = &ctx.cwd;
        let origin = fixture._root.path().join("origin.git");
        git(ctx, path, &["init", "--bare", origin.to_str().unwrap()]).await;
        git(
            ctx,
            path,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        )
        .await;
        let base = git(ctx, path, &["rev-parse", "HEAD"]).await;
        let anchor = base.clone();
        ctx.database
            .call(move |store| {
                store.set_slug_base("one", Some(("main", Some(&anchor))))?;
                Ok(())
            })
            .await
            .unwrap();
        std::fs::write(path.join("work.txt"), "unpublished branch work\n").unwrap();
        git(ctx, path, &["add", "work.txt"]).await;
        git(ctx, path, &["commit", "-m", "feature work"]).await;
        let head = git(ctx, path, &["rev-parse", "HEAD"]).await;
        git(ctx, path, &["push", "origin", "HEAD:refs/heads/main"]).await;
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let row = ctx
            .repository
            .inventory(&ctx.cancellation)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.target.slug() == "one")
            .unwrap();
        let plan = plan_with_facts(ctx, vec![row.clone()], &state, None, None)
            .await
            .unwrap();
        // Only a local-ancestry landing carries a published-base witness.
        assert!(plan.rows[0].revision.published_base.is_some());
        assert!(plan.rows[0].hazards.is_empty());
        assert!(wt_store::is_merged_removal(&plan.rows[0].removed_snapshot));
        // Detached workers deserialize this revision after the planning process
        // exits. Do not reduce the published-base witness to a `landed` bool.
        let delayed: wt_lifecycle::RemovalRevision =
            serde_json::from_slice(&serde_json::to_vec(&plan.rows[0].revision).unwrap()).unwrap();
        assert_eq!(delayed.published_base.as_ref().unwrap().1, head);

        // A linked checkout can override origin independently. Its alternate
        // remote retaining the commit must not validate the main clone's proof.
        let alternate = fixture._root.path().join("alternate-origin.git");
        git(
            ctx,
            path,
            &[
                "clone",
                "--bare",
                origin.to_str().unwrap(),
                alternate.to_str().unwrap(),
            ],
        )
        .await;
        git(ctx, path, &["config", "extensions.worktreeConfig", "true"]).await;
        git(
            ctx,
            path,
            &[
                "config",
                "--worktree",
                &format!("url.{}.insteadOf", alternate.display()),
                origin.to_str().unwrap(),
            ],
        )
        .await;

        // Simulate a remote force-push without updating this checkout's cached
        // origin/main. The old cache still contains HEAD, but origin does not.
        git(ctx, &origin, &["update-ref", "refs/heads/main", &base]).await;
        assert_eq!(git(ctx, path, &["rev-parse", "origin/main"]).await, head);
        assert_eq!(
            git(ctx, path, &["ls-remote", "origin", "refs/heads/main"]).await,
            format!("{head}\trefs/heads/main")
        );
        assert_eq!(
            git(
                ctx,
                &ctx.config.paths.main_clone,
                &["ls-remote", "origin", "refs/heads/main"]
            )
            .await,
            format!("{base}\trefs/heads/main")
        );
        let refused = service(ctx)
            .unwrap()
            .remove_with_revision(
                &row.target,
                wt_lifecycle::RemoveOptions {
                    landed: true,
                    delete_branch: true,
                    ..Default::default()
                },
                &delayed,
                &ctx.cancellation,
            )
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("published base changed"),
            "{refused}"
        );
        assert!(path.exists());
        assert_eq!(
            git(ctx, path, &["rev-parse", "refs/heads/feature/one"]).await,
            head
        );
        let plan = plan_with_facts(ctx, vec![row.clone()], &state, None, None)
            .await
            .unwrap();
        assert!(!plan.rows[0].landed);
        assert!(
            plan.rows[0]
                .hazards
                .iter()
                .any(|hazard| hazard.contains("unpushed"))
        );
        let mut github = GithubData::default();
        github.prs.insert(
            row.target.branch.clone(),
            serde_json::from_value(serde_json::json!({
                "number": 42, "url": "https://example.invalid/pull/42",
                "headRefName": row.target.branch, "headRefOid": base,
                "baseRefName": "main", "mergeCommitOid": base,
                "title": "Previous work", "isDraft": false, "state": "MERGED",
                "checks": "pass", "failedChecks": [], "review": "none",
                "reviewRequests": 0, "requestedReviewers": [], "suggestedReviewers": [],
                "comments": [], "unresolvedThreads": 0, "unresolvedThreadsTotal": 0
            }))
            .unwrap(),
        );
        let reused = plan_cached(
            ctx,
            vec![row.clone()],
            Some(&Confirmed::for_test(github.clone())),
        )
        .await
        .unwrap();
        assert!(!reused.rows[0].landed);
        let history = &reused.rows[0].removed_snapshot;
        assert_eq!(history.extra["landedOnAtRemoval"], "unlanded");
        assert_eq!(history.extra["prState"], "MERGED");
        assert!(!history.extra.contains_key("prMergeCommitOid"));
        assert!(!wt_store::is_merged_removal(history));

        // Cached merge evidence for this exact HEAD is enough. An unavailable
        // origin must not trigger another network read for the merged PR.
        github.prs.get_mut(&row.target.branch).unwrap().head_ref_oid = Some(head.clone());
        git(
            ctx,
            path,
            &["remote", "set-url", "origin", "/missing-wt-origin"],
        )
        .await;
        let cached = plan_cached(
            ctx,
            vec![row.clone()],
            Some(&Confirmed::for_test(github.clone())),
        )
        .await
        .unwrap();
        assert!(cached.rows[0].landed);
        assert!(cached.rows[0].revision.published_base.is_none());
        assert!(cached.warning.is_none());
        assert!(cached.rows[0].hazards.is_empty());
        // The same merged PR, while GitHub is still loading or only restored
        // from the cache, is no evidence at all: nothing lands through it.
        let loading = plan_cached(ctx, vec![row.clone()], None).await.unwrap();
        assert!(!loading.rows[0].landed);
        assert!(!loading.rows[0].hazards.is_empty());
        assert!(
            !loading.rows[0]
                .removed_snapshot
                .extra
                .contains_key("prNumber")
        );
        assert!(!wt_store::is_merged_removal(
            &loading.rows[0].removed_snapshot
        ));
        let target = wt_tui::CleanupTarget {
            key: wt_core::worktree_target_key(&row.target),
            path: row.target.path.clone(),
            branch: row.target.branch.clone(),
            head: head.clone(),
        };
        std::fs::write(path.join("work.txt"), "new uncommitted work\n").unwrap();
        let dirty = plan_cached(
            ctx,
            vec![row.clone()],
            Some(&Confirmed::for_test(github.clone())),
        )
        .await
        .unwrap();
        assert!(dirty.rows[0].landed);
        assert!(!dirty.rows[0].hazards.is_empty());
        let refused = cleanup_targets_cached(
            ctx,
            std::slice::from_ref(&target),
            Some(&Confirmed::for_test(github.clone())),
        )
        .await
        .unwrap_err();
        assert!(refused.to_string().contains("uncommitted"), "{refused}");
        assert!(path.exists());
        std::fs::write(path.join("work.txt"), "unpublished branch work\n").unwrap();
        ctx.database
            .call(|store| {
                let mut status = wt_core::WorkStatusRecord::new(
                    wt_core::WorkState::Ready,
                    "2026-10-12T00:00:00Z",
                );
                status.verify_after_merge = Some("Check the deployed result".into());
                let stored = serde_json::from_value(serde_json::to_value(status)?)?;
                store.set_slug_work_status("one", Some(&stored))?;
                Ok(())
            })
            .await
            .unwrap();
        let refused = cleanup_targets_cached(
            ctx,
            std::slice::from_ref(&target),
            Some(&Confirmed::for_test(github.clone())),
        )
        .await
        .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("still owes: check: Check the deployed result"),
            "{refused}"
        );
        ctx.database
            .call(|store| {
                store.set_slug_work_status("one", None)?;
                Ok(())
            })
            .await
            .unwrap();
        for changed in [
            wt_tui::CleanupTarget {
                head: base.clone(),
                ..target.clone()
            },
            wt_tui::CleanupTarget {
                path: "/different-checkout".into(),
                ..target.clone()
            },
            wt_tui::CleanupTarget {
                branch: "different-branch".into(),
                ..target.clone()
            },
        ] {
            let refused =
                cleanup_targets_cached(ctx, &[changed], Some(&Confirmed::for_test(github.clone())))
                    .await
                    .unwrap_err();
            assert!(
                refused.to_string().contains("identity changed"),
                "{refused}"
            );
            assert!(path.exists());
        }
        git(
            ctx,
            path,
            &["remote", "set-url", "origin", origin.to_str().unwrap()],
        )
        .await;

        // Neither a deleted remote base nor a matching local main is a proof.
        git(ctx, &origin, &["update-ref", "-d", "refs/heads/main"]).await;
        let plan = plan_with_facts(ctx, vec![row.clone()], &state, None, None)
            .await
            .unwrap();
        assert!(!plan.rows[0].landed);
        assert!(plan.warning.is_some());
        let refused = cleanup_targets_cached(ctx, &[target], None)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("unpushed"), "{refused}");
        assert!(path.exists());
        git(ctx, path, &["update-ref", "-d", "refs/remotes/origin/main"]).await;
        git(ctx, path, &["update-ref", "refs/heads/main", &head]).await;
        let plan = plan_with_facts(ctx, vec![row], &state, None, None)
            .await
            .unwrap();
        assert!(!plan.rows[0].landed);
        fixture.close().await.unwrap();
    }

    /// The facts pass reruns on every board change for every landed checkout
    /// awaiting cleanup, so it must not read file contents: an unreadable
    /// untracked file is a hazard there, never a failed read.
    #[cfg(unix)]
    #[tokio::test]
    async fn automation_facts_judge_landed_checkouts_without_reading_contents() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let path = &ctx.cwd;
        let origin = fixture._root.path().join("origin.git");
        git(ctx, path, &["init", "--bare", origin.to_str().unwrap()]).await;
        git(
            ctx,
            path,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        )
        .await;
        let base = git(ctx, path, &["rev-parse", "HEAD"]).await;
        ctx.database
            .call(move |store| {
                store.set_slug_base("one", Some(("main", Some(&base))))?;
                Ok(())
            })
            .await
            .unwrap();
        std::fs::write(path.join("work.txt"), "landed work\n").unwrap();
        git(ctx, path, &["add", "work.txt"]).await;
        git(ctx, path, &["commit", "-m", "feature work"]).await;
        git(ctx, path, &["push", "origin", "HEAD:refs/heads/main"]).await;
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let row = ctx
            .repository
            .inventory(&ctx.cancellation)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.target.slug() == "one")
            .unwrap();
        let facts = || async {
            let evidence = plan_automation_facts(ctx, vec![row.clone()], &state, None)
                .await
                .unwrap();
            assert!(evidence.row_errors.is_empty(), "{:?}", evidence.row_errors);
            evidence.rows["one"].clean_candidate
        };
        assert!(facts().await);
        assert!(facts().await);

        let secret = path.join("secret.bin");
        std::fs::write(&secret, "untracked").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(!facts().await);
        std::fs::remove_file(&secret).unwrap();
        assert!(facts().await);

        std::fs::write(path.join("work.txt"), "edited\n").unwrap();
        assert!(!facts().await);
        git(ctx, path, &["add", "work.txt"]).await;
        assert!(!facts().await);
        // A commit beyond the published base is no longer landed at all.
        git(ctx, path, &["commit", "-m", "local only"]).await;
        assert!(!facts().await);
        git(ctx, path, &["reset", "--hard", "HEAD~1"]).await;
        assert!(facts().await);
        fixture.close().await.unwrap();
    }

    /// Deploy proof is read only for the stage of the check a landed row
    /// owes, and only where that stage has a probe.
    #[tokio::test]
    async fn automation_facts_wait_for_a_probed_stage_to_run_the_landing() {
        let mut fixture = CommandFixture::new().await.unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config)
            .environments
            .staging = Some(wt_config::EnvironmentConfig {
            deployed: vec!["unused".into()],
            ..Default::default()
        });
        let ctx = &fixture.ctx;
        let path = &ctx.cwd;
        let origin = fixture._root.path().join("origin.git");
        git(ctx, path, &["init", "--bare", origin.to_str().unwrap()]).await;
        git(
            ctx,
            path,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        )
        .await;
        let base = git(ctx, path, &["rev-parse", "HEAD"]).await;
        let anchor = base.clone();
        ctx.database
            .call(move |store| {
                store.set_slug_base("one", Some(("main", Some(&anchor))))?;
                Ok(())
            })
            .await
            .unwrap();
        std::fs::write(path.join("work.txt"), "landed work\n").unwrap();
        git(ctx, path, &["add", "work.txt"]).await;
        git(ctx, path, &["commit", "-m", "feature work"]).await;
        git(ctx, path, &["push", "origin", "HEAD:refs/heads/main"]).await;
        let head = git(ctx, path, &["rev-parse", "HEAD"]).await;
        let row = ctx
            .repository
            .inventory(&ctx.cancellation)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.target.slug() == "one")
            .unwrap();
        let stored = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let owe = |stage: Option<wt_core::VerifyStage>| {
            let mut work =
                wt_core::WorkStatusRecord::new(wt_core::WorkState::Ready, "2026-01-01T00:00:00Z");
            work.verify_after_merge = stage.map(|_| "check it".to_owned());
            work.verify_in = stage;
            let mut state = stored.clone();
            state["slugs"]["one"]["work"] = serde_json::to_value(work).unwrap();
            state
        };
        let deploys = |state: serde_json::Value| {
            let row = row.clone();
            async move {
                let evidence = plan_automation_facts(ctx, vec![row], &state, None)
                    .await
                    .unwrap();
                assert!(evidence.row_errors.is_empty(), "{:?}", evidence.row_errors);
                assert_eq!(evidence.rows["one"].local_merged, Some(true));
                evidence.rows["one"].deploys
            }
        };
        let staging = |deployed| wt_core::StageDeploys {
            staging: Some(deployed),
            production: None,
        };
        let main = &ctx.config.paths.main_clone;
        let record = |oid: String| async move {
            wt_vcs::record_deployed(
                &ctx.processes,
                main,
                wt_core::VerifyStage::Staging,
                &oid,
                &ctx.cancellation,
            )
            .await
            .unwrap();
        };
        // Nothing owed, so nothing is looked up: a probed stage then reads
        // not deployed, never as a stage without a probe.
        let state = owe(None);
        assert_eq!(deploys(state).await, staging(false));
        // Owed in staging, and the probe has reported nothing yet.
        let state = owe(Some(wt_core::VerifyStage::Staging));
        assert_eq!(deploys(state.clone()).await, staging(false));
        record(base).await;
        assert_eq!(deploys(state.clone()).await, staging(false));
        record(head).await;
        assert_eq!(deploys(state).await, staging(true));
        // A production check on a stage with no probe keeps the Git landing.
        let state = owe(Some(wt_core::VerifyStage::Production));
        assert_eq!(deploys(state).await.production, None);
        fixture.close().await.unwrap();
    }
}
