//! Finds each worktree's other branches (see `wt_core::other_branches`),
//! records them, and records what GitHub says of the pull request on each.
//! The record is what every surface shows and what removal checks under its
//! lock, so a sighting made here outlives the reflog and the worktree.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use wt_app_core::context::AppContext;
use wt_core::{OtherBranch, OtherPrState, OtherPrVerdict};
use wt_github::{GithubClient, GithubData, GithubOptions};
use wt_vcs::WorktreeRecord;

use crate::commands::resolve::run_git;

/// How long an explicit removal waits for GitHub to answer for the other
/// branches before leaving them unknown.
const GITHUB_WAIT: Duration = Duration::from_secs(8);

/// How many rows one pass reads at a time.
const ROWS_AT_ONCE: usize = 8;

/// Whether the caller may spend a GitHub read on branches the data it holds
/// does not cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    /// A person or a command asked for this removal or listing.
    Github,
    /// A timer or a board refresh: use what is at hand.
    Nothing,
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// What `github` says of `branch`, stamped with when that was observed: a
/// snapshot can be minutes old, and its age is what the verdict is worth.
/// Data that cannot say when it was read answers nothing.
fn answer(branch: &str, github: &GithubData) -> Option<OtherBranch> {
    let observed = i64::try_from(github.observed_at_ms?).ok()?;
    if !github.covered_branches.contains(branch) {
        return None;
    }
    let pr = github.prs.get(branch);
    Some(OtherBranch {
        branch: branch.to_owned(),
        number: pr.map(|pr| pr.number),
        state: pr.map_or(OtherPrState::None, |pr| OtherPrState::of_github(&pr.state)),
        draft: pr.is_some_and(|pr| pr.is_draft),
        checked_at_ms: Some(observed),
    })
}

/// Whether `answered` was observed no earlier than what is recorded.
fn not_older(answered: &OtherBranch, stored: Option<&OtherBranch>) -> bool {
    stored.is_none_or(|stored| stored.checked_at_ms <= answered.checked_at_ms)
}

/// Whether origin itself says it has no such branch. `None` when it could
/// not be asked, which is not "gone".
async fn gone_from_origin(ctx: &AppContext, path: &Path, branch: &str) -> Option<bool> {
    let output = run_git(
        ctx,
        path,
        [
            "ls-remote",
            "--exit-code",
            "--refs",
            "origin",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await
    .ok()?;
    match output.status.code() {
        Some(0) => Some(false),
        Some(2) => Some(true),
        _ => None,
    }
}

async fn ref_exists(ctx: &AppContext, path: &Path, reference: &str) -> bool {
    run_git(
        ctx,
        path,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )
    .await
    .is_ok_and(|output| output.status.success())
}

/// The branches one batched GitHub read asks about for `rows`: each linked
/// worktree's own branch and the other branches `state` records for it, so
/// a pull request on one is answered by the same read. A recorded merge is
/// left out: only a person's removal asks about it again (see [`observe`]).
/// Sorted and unique. The board's GitHub
/// leaf and the events daemon both ask for exactly this set, which is what
/// lets the daemon's snapshot stand in for the board's read.
pub fn github_branches<'a>(
    rows: impl IntoIterator<Item = &'a WorktreeRecord>,
    state: &Value,
) -> Vec<String> {
    let mut branches = BTreeSet::new();
    for row in rows.into_iter().filter(|row| !row.is_main) {
        if !row.target.branch.is_empty() {
            branches.insert(row.target.branch.clone());
        }
        branches.extend(
            wt_core::stored_other_branches(&state["slugs"][row.target.slug()])
                .into_iter()
                .filter(|other| other.state != OtherPrState::Merged)
                .map(|other| other.branch),
        );
    }
    branches.into_iter().collect()
}

/// The other branches of each of `rows`, by slug, after recording every new
/// sighting and every answer `github` holds. A branch `github` does not
/// cover keeps its recorded answer, or reads unknown: never "no PR". Rows
/// with none are left out.
///
/// With [`Ask::Github`] one read also asks again about every recorded
/// answer that is open, or older than [`wt_core::PR_VERDICT_FRESH_MS`],
/// while its branch still exists: a pull request merged or closed by hand
/// stops holding, and a second one on a merged branch is seen. An open
/// answer whose branch is gone here and from origin is forgotten, and so is
/// any recorded branch that another worktree now has checked out: its pull
/// request has a row of its own.
pub async fn observe(
    ctx: &AppContext,
    rows: &[WorktreeRecord],
    state: &Value,
    github: Option<&GithubData>,
    ask: Ask,
) -> BTreeMap<String, Vec<OtherBranch>> {
    observe_asking(ctx, rows, state, github, ask).await.others
}

async fn observe_asking(
    ctx: &AppContext,
    rows: &[WorktreeRecord],
    state: &Value,
    github: Option<&GithubData>,
    ask: Ask,
) -> Observed {
    observe_with(ctx, rows, state, github, ask, |branches| async move {
        if std::env::var("WT_GITHUB").as_deref() == Ok("off") {
            return None;
        }
        let client = GithubClient::new(
            ctx.processes.clone(),
            ctx.config.paths.main_clone.clone(),
            GithubOptions::from_config(&ctx.config, false),
        );
        tokio::time::timeout(
            GITHUB_WAIT,
            client.fetch_worktrees(&branches, &ctx.cancellation),
        )
        .await
        .ok()?
        .ok()
    })
    .await
}

#[derive(Debug, Default)]
struct Observed {
    others: BTreeMap<String, Vec<OtherBranch>>,
    /// The (slug, branch) records dropped by this pass.
    forgotten: Vec<(String, String)>,
}

/// What one row's pass found.
#[derive(Default)]
struct RowSeen {
    /// `None` when the row has nothing to record.
    others: Option<Vec<OtherBranch>>,
    again: Vec<String>,
    /// Recorded branches deleted here and on origin.
    gone: Vec<OtherBranch>,
    /// Recorded branches another worktree has checked out.
    adopted: Vec<OtherBranch>,
}

/// Whether `inventory` shows a worktree other than `slug` on `branch`.
fn adopted_elsewhere(inventory: &[WorktreeRecord], slug: &str, branch: &str) -> bool {
    inventory
        .iter()
        .any(|owner| owner.target.slug() != slug && owner.target.branch == branch)
}

/// The whole inventory, read at most once per pass and only when a row has
/// a candidate to check against it.
type Inventory = tokio::sync::OnceCell<Option<Vec<WorktreeRecord>>>;

struct Pass<'a> {
    ctx: &'a AppContext,
    lifecycle: &'a wt_lifecycle::LifecycleService,
    inventory: Inventory,
    github: Option<&'a GithubData>,
    ask: Ask,
    now: i64,
    /// Several rows share one inventory read; a single row with nothing
    /// recorded reads it only if its reflog names a candidate.
    shared: bool,
}

async fn observe_row(pass: &Pass<'_>, row: &WorktreeRecord, slug_state: &Value) -> RowSeen {
    let Pass {
        ctx,
        lifecycle,
        github,
        ask,
        now,
        ..
    } = *pass;
    let mut seen = RowSeen::default();
    let stored = wt_core::stored_other_branches(slug_state);
    let all = if pass.shared || !stored.is_empty() {
        pass.inventory
            .get_or_init(|| async { ctx.repository.inventory(&ctx.cancellation).await.ok() })
            .await
            .as_deref()
    } else {
        None
    };
    let Ok((names, _)) = lifecycle
        .other_branch_names_among(row, Some(slug_state), all, &ctx.cancellation)
        .await
    else {
        return seen;
    };
    // An unknown inventory proves no adoption, and neither does one that
    // disagrees with `row` about the row itself: the adopter and the row
    // have to be seen in the same listing.
    if let Some(all) = all
        && all.iter().any(|same| same.target == row.target)
    {
        seen.adopted.extend(
            stored
                .iter()
                .filter(|other| adopted_elsewhere(all, row.target.slug(), &other.branch))
                .cloned(),
        );
    }
    let unrecorded = wt_core::recorded_branch(slug_state).is_none()
        && !row.detached
        && !row.target.branch.is_empty();
    if names.is_empty() && !unrecorded {
        return seen;
    }
    let path = Path::new(&row.target.path);
    let mut others = Vec::new();
    for branch in names {
        let stored = stored.iter().find(|other| other.branch == branch);
        if let Some(answered) = github
            .and_then(|github| answer(&branch, github))
            .filter(|answered| not_older(answered, stored))
        {
            others.push(answered);
            continue;
        }
        // GitHub's own answer stands until it is asked again. "None"
        // without one was read off a branch origin did not have, which
        // a push changes.
        let settled = stored.is_some_and(|other| {
            other.number.is_some()
                || (other.state == OtherPrState::None && other.checked_at_ms.is_some())
        });
        if let Some(stored) = stored.filter(|_| settled) {
            let stale = stored
                .checked_at_ms
                .is_none_or(|at| now.saturating_sub(at) > wt_core::PR_VERDICT_FRESH_MS);
            if ask == Ask::Github && (stale || stored.state == OtherPrState::Open) {
                match whereabouts(ctx, path, row.target.slug(), &branch).await {
                    Whereabouts::Nowhere if stored.state != OtherPrState::Open => {}
                    Whereabouts::Nowhere
                        if gone_from_origin(ctx, path, &branch).await == Some(true) =>
                    {
                        seen.gone.push(stored.clone());
                        continue;
                    }
                    _ => seen.again.push(branch.clone()),
                }
            }
            others.push(stored.clone());
            continue;
        }
        match whereabouts(ctx, path, row.target.slug(), &branch).await {
            Whereabouts::Origin => others.push(OtherBranch::unknown(branch)),
            // A branch origin never had cannot be a pull request's head.
            Whereabouts::LocalOnly => others.push(OtherBranch {
                state: OtherPrState::None,
                ..OtherBranch::unknown(branch)
            }),
            // A name the reflog kept of a branch that exists nowhere
            // any more holds nothing and is not recorded.
            Whereabouts::Nowhere => others.extend(stored.cloned()),
        }
    }
    seen.others = Some(others);
    seen
}

/// [`observe`] with the GitHub read given by the caller.
async fn observe_with<F, Fut>(
    ctx: &AppContext,
    rows: &[WorktreeRecord],
    state: &Value,
    github: Option<&GithubData>,
    ask: Ask,
    read: F,
) -> Observed
where
    F: FnOnce(Vec<String>) -> Fut,
    Fut: Future<Output = Option<GithubData>>,
{
    use futures_util::StreamExt as _;

    let Ok(lifecycle) = crate::lifecycle_ops::service(ctx) else {
        return Observed::default();
    };
    let now = now_ms();
    let pass = Pass {
        ctx,
        lifecycle: &lifecycle,
        inventory: Inventory::new(),
        github,
        ask,
        now,
        shared: rows.len() > 1,
    };
    let pass = &pass;
    // Each row costs a reflog read and a few Git calls of its own; a fleet
    // listing passes every row at once.
    let mut passes = Vec::new();
    for row in rows {
        if !row.is_main {
            let slug_state = &state["slugs"][row.target.slug()];
            passes.push(async move { (row, observe_row(pass, row, slug_state).await) });
        }
    }
    let seen: Vec<(&WorktreeRecord, RowSeen)> = futures_util::stream::iter(passes)
        .buffered(ROWS_AT_ONCE)
        .collect()
        .await;
    let mut found: Vec<(&WorktreeRecord, Vec<OtherBranch>)> = Vec::new();
    let mut again = BTreeSet::new();
    let mut forget: Vec<(String, OtherBranch)> = Vec::new();
    let mut adopted: Vec<(String, OtherBranch)> = Vec::new();
    for (row, seen) in seen {
        again.extend(seen.again);
        let slug = row.target.slug();
        forget.extend(seen.gone.into_iter().map(|other| (slug.to_owned(), other)));
        adopted.extend(
            seen.adopted
                .into_iter()
                .map(|other| (slug.to_owned(), other)),
        );
        if let Some(others) = seen.others {
            found.push((row, others));
        }
    }
    if ask == Ask::Github {
        let mut unanswered = again;
        unanswered.extend(
            found
                .iter()
                .flat_map(|(_, others)| others)
                .filter(|other| other.verdict(now) == OtherPrVerdict::Unknown)
                .map(|other| other.branch.clone()),
        );
        if !unanswered.is_empty()
            && let Some(fresh) = read(unanswered.into_iter().collect()).await
        {
            for other in found.iter_mut().flat_map(|(_, others)| others) {
                if let Some(answered) =
                    answer(&other.branch, &fresh).filter(|new| not_older(new, Some(other)))
                {
                    *other = answered;
                }
            }
        }
    }
    // Asked again right before the record goes: the adopter may have let
    // the branch go while GitHub was being read.
    if !adopted.is_empty()
        && let Ok(current) = ctx.repository.inventory(&ctx.cancellation).await
    {
        forget.extend(
            adopted
                .into_iter()
                .filter(|(slug, other)| adopted_elsewhere(&current, slug, &other.branch)),
        );
    }
    let mut observed = Observed::default();
    for (slug, other) in forget {
        let key = (slug.clone(), other.branch.clone());
        let forgotten = ctx
            .database
            .call(move |store| Ok(store.forget_other_branch(&slug, &other)?))
            .await;
        match forgotten {
            Ok(true) => {
                tracing::info!(slug = %key.0, branch = %key.1, "forgot a worktree's other branch");
                observed.forgotten.push(key);
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%error, "could not forget a worktree's other branch");
            }
        }
    }
    for (row, others) in found {
        let slug = row.target.slug().to_owned();
        let recorded = (!row.detached).then(|| row.target.branch.clone());
        let write = (slug.clone(), others.clone());
        let written = ctx
            .database
            .call(move |store| {
                Ok(store.record_other_branches(&write.0, recorded.as_deref(), &write.1)?)
            })
            .await;
        if let Err(error) = written {
            tracing::warn!(%slug, %error, "could not record a worktree's other branches");
        }
        if !others.is_empty() {
            observed.others.insert(slug, others);
        }
    }
    observed
}

/// [`observe`], then `state` brought up to date with what it recorded, for a
/// command that prints from the state it already read.
pub async fn refresh(
    ctx: &AppContext,
    rows: &[WorktreeRecord],
    state: &mut Value,
    github: Option<&GithubData>,
    ask: Ask,
) {
    let observed = observe_asking(ctx, rows, state, github, ask).await;
    let Some(slugs) = state.get_mut("slugs").and_then(Value::as_object_mut) else {
        return;
    };
    for (slug, branch) in observed.forgotten {
        if let Some(others) = slugs
            .get_mut(&slug)
            .and_then(|entry| entry.get_mut(wt_core::OTHER_BRANCHES))
            .and_then(Value::as_object_mut)
        {
            others.remove(&branch);
        }
    }
    for (slug, others) in observed.others {
        let entries: serde_json::Map<String, Value> = others
            .iter()
            .filter_map(|other| Some((other.branch.clone(), serde_json::to_value(other).ok()?)))
            .collect();
        let entry = slugs
            .entry(slug)
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Some(entry) = entry.as_object_mut() {
            entry.insert(wt_core::OTHER_BRANCHES.to_owned(), Value::Object(entries));
        }
    }
}

enum Whereabouts {
    Origin,
    LocalOnly,
    Nowhere,
}

/// Where `branch` still exists. "Nowhere" is remembered for a while: reflogs
/// keep the names of long-deleted branches, and every pass would ask Git
/// about each again.
async fn whereabouts(ctx: &AppContext, path: &Path, slug: &str, branch: &str) -> Whereabouts {
    const REMEMBER: Duration = Duration::from_secs(10 * 60);
    static GONE: std::sync::Mutex<BTreeMap<(String, String), std::time::Instant>> =
        std::sync::Mutex::new(BTreeMap::new());
    let key = (slug.to_owned(), branch.to_owned());
    if GONE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .is_some_and(|at| at.elapsed() < REMEMBER)
    {
        return Whereabouts::Nowhere;
    }
    let found = if ref_exists(ctx, path, &format!("refs/remotes/origin/{branch}")).await {
        Whereabouts::Origin
    } else if ref_exists(ctx, path, &format!("refs/heads/{branch}")).await {
        Whereabouts::LocalOnly
    } else {
        Whereabouts::Nowhere
    };
    let mut gone = GONE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if matches!(found, Whereabouts::Nowhere) {
        gone.insert(key, std::time::Instant::now());
    } else {
        gone.remove(&key);
    }
    found
}

/// Tell the session of a worktree whose checkout left its recorded branch,
/// once per branch it lands on, and only a session that is already running.
pub async fn notify_off_branch(ctx: &AppContext, row: &WorktreeRecord, slug_state: &Value) {
    let current = row.target.branch.as_str();
    let Some(recorded) = wt_core::recorded_branch(slug_state) else {
        return;
    };
    if row.is_main
        || row.detached
        || current.is_empty()
        || current == recorded
        || slug_state[wt_core::OFF_BRANCH_NOTICE].as_str() == Some(current)
    {
        return;
    }
    let slug = row.target.slug().to_owned();
    let app = wt_app_core::harness::AppHarness::new(ctx);
    let Ok(routes) = app.routes(ctx).await else {
        return;
    };
    let Some(route) = routes.iter().find(|route| {
        route.target.slug == slug
            && !route.target.remote
            && route
                .choice
                .live
                .as_ref()
                .is_some_and(|live| !live.is_empty())
    }) else {
        return;
    };
    let claim = (slug.clone(), current.to_owned());
    let claimed = ctx
        .database
        .call(move |store| Ok(store.claim_off_branch_notice(&claim.0, &claim.1)?))
        .await
        .unwrap_or(false);
    if !claimed {
        return;
    }
    let text = wt_core::off_branch_notice(&slug, recorded, current);
    if let Err(error) = app.send(route, &text, Some("wt"), ctx).await {
        tracing::warn!(%slug, %error, "could not tell a session it left its worktree's branch");
        // The claim is the only thing that stops a retry, so a notice that
        // was never delivered gives it back, a few times at most.
        let key = (slug, current.to_owned());
        if note_failed_send(&key) {
            let released = ctx
                .database
                .call(move |store| Ok(store.release_off_branch_notice(&key.0, &key.1)?))
                .await;
            if let Err(error) = released {
                tracing::warn!(%error, "could not release an undelivered off-branch notice");
            }
        }
    }
}

/// How many times one notice is sent before a failing session is left alone.
const NOTICE_TRIES: u8 = 3;

/// Count a failed send of the notice `key` names; true while it may be
/// tried again.
fn note_failed_send(key: &(String, String)) -> bool {
    static FAILED: std::sync::Mutex<BTreeMap<(String, String), u8>> =
        std::sync::Mutex::new(BTreeMap::new());
    let mut failed = FAILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tries = failed.entry(key.clone()).or_insert(0);
    *tries = tries.saturating_add(1);
    *tries < NOTICE_TRIES
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_app_core::test_support::CommandFixture;

    async fn git(ctx: &AppContext, args: &[&str]) -> String {
        let output = run_git(ctx, &ctx.cwd, args.iter().copied()).await.unwrap();
        assert!(output.status.success(), "git {args:?}");
        output.stdout_text().trim().to_owned()
    }

    async fn state(ctx: &AppContext) -> Value {
        ctx.database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap()
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

    fn open_pr(branch: &str, number: u64) -> GithubData {
        let mut data = GithubData::default();
        data.prs.insert(
            branch.to_owned(),
            serde_json::from_value(serde_json::json!({
                "number": number, "url": "https://example.invalid/pull/1",
                "headRefName": branch, "headRefOid": "a".repeat(40),
                "baseRefName": "main", "title": "Retire X", "isDraft": true, "state": "OPEN",
                "checks": "pass", "failedChecks": [], "review": "none",
                "reviewRequests": 0, "requestedReviewers": [], "suggestedReviewers": [],
                "comments": [], "unresolvedThreads": 0, "unresolvedThreadsTotal": 0
            }))
            .unwrap(),
        );
        data.covered_branches.insert(branch.to_owned());
        data.observed_at_ms = u64::try_from(now_ms()).ok();
        data
    }

    fn pr_in(mut data: GithubData, state: &str, observed_at_ms: i64) -> GithubData {
        for pr in data.prs.values_mut() {
            state.clone_into(&mut pr.state);
        }
        data.observed_at_ms = u64::try_from(observed_at_ms).ok();
        data
    }

    /// A recorded answer is only as good as its age. A person's removal asks
    /// again about an open one and about a stale one, an older snapshot
    /// never replaces a newer answer, and a branch deleted everywhere stops
    /// holding.
    #[tokio::test]
    async fn a_recorded_answer_is_asked_again_and_never_replaced_by_an_older_one() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let origin = fixture._root.path().join("origin.git");
        git(ctx, &["init", "--bare", origin.to_str().unwrap()]).await;
        git(ctx, &["remote", "add", "origin", origin.to_str().unwrap()]).await;
        let own = row_one(ctx).await.target.branch;
        git(ctx, &["push", "-q", "origin", &own]).await;
        git(ctx, &["switch", "-q", "-c", "second-pr"]).await;
        git(ctx, &["push", "-q", "origin", "second-pr"]).await;
        git(ctx, &["switch", "-q", &own]).await;
        let rows = [row_one(ctx).await];
        let stored = || async {
            wt_core::stored_other_branches(&state(ctx).await["slugs"]["one"]).remove(0)
        };
        let hazards = || async {
            crate::lifecycle_ops::plan_with_facts(
                ctx,
                vec![row_one(ctx).await],
                &state(ctx).await,
                None,
                None,
            )
            .await
            .unwrap()
            .rows
            .remove(0)
            .hazards
        };
        let never = |_: Vec<String>| async { panic!("nothing to ask GitHub") };
        let now = now_ms();

        let then = now - wt_core::PR_VERDICT_FRESH_MS - 60_000;
        let open = pr_in(open_pr("second-pr", 3961), "OPEN", then);
        observe(ctx, &rows, &state(ctx).await, Some(&open), Ask::Nothing).await;
        assert_eq!(stored().await.checked_at_ms, Some(then));
        // An older "no PR", and data that cannot say how old it is.
        let mut none = GithubData::default();
        none.covered_branches.insert("second-pr".into());
        for observed_at_ms in [u64::try_from(then - 60_000).ok(), None] {
            none.observed_at_ms = observed_at_ms;
            let seen = observe(ctx, &rows, &state(ctx).await, Some(&none), Ask::Nothing).await;
            assert_eq!(seen["one"][0].state, OtherPrState::Open);
            assert_eq!(stored().await.state, OtherPrState::Open);
        }

        // Merged by hand since: the removal asks, and the hold is gone.
        let merged = pr_in(open_pr("second-pr", 3961), "MERGED", then + 1_000);
        observe_with(
            ctx,
            &rows,
            &state(ctx).await,
            None,
            Ask::Github,
            |asked| async move {
                assert_eq!(asked, ["second-pr"]);
                Some(merged)
            },
        )
        .await;
        assert_eq!(stored().await.state, OtherPrState::Merged);
        assert!(hazards().await.is_empty(), "{:?}", hazards().await);
        // A background pass leaves a merge alone; a removal asks again once
        // the answer is old, and sees the second pull request.
        observe_with(ctx, &rows, &state(ctx).await, None, Ask::Nothing, never).await;
        let second = pr_in(open_pr("second-pr", 4000), "OPEN", now);
        observe_with(
            ctx,
            &rows,
            &state(ctx).await,
            None,
            Ask::Github,
            |asked| async move {
                assert_eq!(asked, ["second-pr"]);
                Some(second)
            },
        )
        .await;
        assert_eq!(stored().await.number, Some(4000));
        assert_eq!(hazards().await.len(), 1);

        // Deleted here and on origin: nothing is left to hold.
        git(ctx, &["push", "-q", "origin", "--delete", "second-pr"]).await;
        git(ctx, &["branch", "-q", "-D", "second-pr"]).await;
        let seen = observe_with(ctx, &rows, &state(ctx).await, None, Ask::Github, never).await;
        assert!(seen.others.is_empty(), "{seen:?}");
        assert!(wt_core::stored_other_branches(&state(ctx).await["slugs"]["one"]).is_empty());
        assert!(hazards().await.is_empty(), "{:?}", hazards().await);
        fixture.close().await.unwrap();
    }

    /// A recorded other branch that gets a worktree of its own has a row to
    /// show its pull request, so the first worktree stops listing it.
    #[tokio::test]
    async fn a_branch_adopted_by_another_worktree_is_forgotten() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let origin = fixture._root.path().join("origin.git");
        git(ctx, &["init", "--bare", origin.to_str().unwrap()]).await;
        git(ctx, &["remote", "add", "origin", origin.to_str().unwrap()]).await;
        let own = row_one(ctx).await.target.branch;
        git(ctx, &["switch", "-q", "-c", "second-pr"]).await;
        git(ctx, &["push", "-q", "origin", "second-pr"]).await;
        git(ctx, &["switch", "-q", &own]).await;
        let rows = [row_one(ctx).await];
        let open = open_pr("second-pr", 3961);
        observe(ctx, &rows, &state(ctx).await, Some(&open), Ask::Nothing).await;
        let before = state(ctx).await;
        assert_eq!(
            wt_core::stored_other_branches(&before["slugs"]["one"]).len(),
            1
        );

        let two = fixture._root.path().join("worktrees/two");
        git(
            ctx,
            &["-C", two.to_str().unwrap(), "switch", "-q", "second-pr"],
        )
        .await;
        let mut shown = before.clone();
        refresh(ctx, &rows, &mut shown, Some(&open), Ask::Nothing).await;
        assert!(wt_core::stored_other_branches(&shown["slugs"]["one"]).is_empty());
        assert!(wt_core::stored_other_branches(&state(ctx).await["slugs"]["one"]).is_empty());
        fixture.close().await.unwrap();
    }

    /// An agent opens a second branch inside a worktree, pushes it and goes
    /// back. The branch is found in the reflog, recorded, held against
    /// removal while its pull request is open or unknown, and stays
    /// recorded once the reflog is gone.
    #[tokio::test]
    async fn a_second_branch_in_a_worktree_is_recorded_and_holds_the_worktree() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let origin = fixture._root.path().join("origin.git");
        git(ctx, &["init", "--bare", origin.to_str().unwrap()]).await;
        git(ctx, &["remote", "add", "origin", origin.to_str().unwrap()]).await;
        let own = row_one(ctx).await.target.branch;
        git(ctx, &["push", "-q", "origin", &own]).await;

        // A branch that was only ever local cannot be a pull request's head.
        git(ctx, &["switch", "-q", "-c", "retire-x"]).await;
        git(ctx, &["switch", "-q", &own]).await;
        let rows = [row_one(ctx).await];
        let seen = observe(ctx, &rows, &state(ctx).await, None, Ask::Nothing).await;
        assert_eq!(seen["one"].len(), 1);
        assert_eq!(seen["one"][0].branch, "retire-x");
        assert_eq!(seen["one"][0].state, OtherPrState::None);
        let recorded = state(ctx).await;
        assert_eq!(recorded["slugs"]["one"]["branch"], own.as_str());
        let hazards = || async {
            crate::lifecycle_ops::plan_with_facts(
                ctx,
                vec![row_one(ctx).await],
                &state(ctx).await,
                None,
                None,
            )
            .await
            .unwrap()
            .rows
            .remove(0)
            .hazards
        };
        assert!(hazards().await.is_empty(), "{:?}", hazards().await);

        // Pushed, it may carry one: unknown until GitHub answers.
        git(ctx, &["push", "-q", "origin", "retire-x"]).await;
        assert_eq!(hazards().await, [wt_core::unknown_pr_hazard("retire-x")]);
        // The row's own data does not cover the branch, so it stays unknown.
        let own_only = {
            let mut data = GithubData::default();
            data.covered_branches.insert(own.clone());
            data.observed_at_ms = u64::try_from(now_ms()).ok();
            data
        };
        let seen = observe(ctx, &rows, &state(ctx).await, Some(&own_only), Ask::Nothing).await;
        assert_eq!(seen["one"][0].state, OtherPrState::Unknown);

        let github = open_pr("retire-x", 3961);
        let mut current = state(ctx).await;
        refresh(ctx, &rows, &mut current, Some(&github), Ask::Nothing).await;
        assert_eq!(
            wt_core::other_prs_json(&current["slugs"]["one"]),
            serde_json::json!([
                {"number": 3961, "branch": "retire-x", "state": "open", "draft": true}
            ])
        );
        assert_eq!(
            hazards().await,
            ["holds open PR #3961 on retire-x: give it its own worktree with wt new retire-x"]
        );

        // The record outlives the reflog, and a later read that does not
        // cover the branch does not turn the open pull request into none.
        let git_dir = row_one(ctx).await.git_dir.unwrap();
        std::fs::remove_file(git_dir.join("logs/HEAD")).unwrap();
        let seen = observe(ctx, &rows, &state(ctx).await, Some(&own_only), Ask::Nothing).await;
        assert_eq!(seen["one"][0].state, OtherPrState::Open);
        assert_eq!(hazards().await.len(), 1);

        // wt next leaves a landed branch behind on purpose: the worktree is
        // for the new one, and the old one is not an other branch.
        ctx.database
            .call(|store| Ok(store.set_slug_branch("one", "retire-x")?))
            .await
            .unwrap();
        let next = state(ctx).await;
        assert_eq!(
            next["slugs"]["one"]["leftBranches"],
            serde_json::json!([own])
        );
        assert!(wt_core::stored_other_branches(&next["slugs"]["one"]).is_empty());
        fixture.close().await.unwrap();
    }
}
