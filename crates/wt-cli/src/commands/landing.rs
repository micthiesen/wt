//! How far each worktree's own work has landed, as the commands that say
//! what a worktree owes read it. One reading for all of them, so `wt owed`,
//! `wt done`, `wt status` and the listings agree with each other and, as
//! far as a command can, with the board.
//!
//! Git ancestry proves an ordinary merge. A squash merge leaves no ancestry:
//! only the pull request merged at the worktree's HEAD proves it. That is
//! looked for where it is cheapest first: the board's saved GitHub data,
//! then the events daemon's snapshot, then one bounded GitHub read for the
//! rows still open. When none of them answers, the landing is unknown and
//! is reported as unknown, never as "not landed". A command that prints
//! the landing also takes a snapshot GitHub's budget is holding back; one
//! that decides a write from it ([`read_one_live`]) never does.

use std::{collections::BTreeMap, time::Duration};

use serde_json::Value;
use wt_app_core::context::AppContext;
use wt_core::Landing;
use wt_github::{GithubClient, GithubData, GithubOptions};
use wt_ops::lifecycle_ops::PublishedBases;
use wt_vcs::WorktreeRecord;

/// How long one command waits for GitHub before calling a landing unknown.
const GITHUB_WAIT: Duration = Duration::from_secs(8);

/// What a command could establish of one row's landing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RowLanding {
    landing: Landing,
    known: bool,
}

impl RowLanding {
    pub(crate) const fn known(landing: Landing) -> Self {
        Self {
            landing,
            known: true,
        }
    }

    pub(crate) const fn unknown(landing: Landing) -> Self {
        Self {
            landing,
            known: false,
        }
    }

    /// Whether the landing could not be established either way.
    pub(crate) const fn is_unknown(self) -> bool {
        !self.known
    }

    /// The landing to derive what the row owes under. An unknown landing
    /// derives as not landed: nothing is called due that may not be, an
    /// assertion closes no more than it would before landing, and the
    /// reader is told the landing is unknown.
    pub(crate) const fn shown(self) -> Landing {
        self.landing
    }

    /// Whether the row's own work is known to have landed.
    pub(crate) const fn is_landed(self) -> bool {
        self.known && self.landing.base
    }

    /// The `landing` value of the JSON surfaces.
    pub(crate) const fn word(self) -> &'static str {
        match (self.known, self.landing.base) {
            (false, _) => "unknown",
            (true, true) => "landed",
            (true, false) => "not_landed",
        }
    }
}

/// Which rows a command needs the landing of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Want {
    /// Rows whose list reads differently once landed: those that owe a
    /// check or hold a merge gate. Enough for the printed list, and for
    /// nothing else: no other row is read at all, and its landing comes
    /// back as not landed whatever it is.
    Lists,
    /// [`Want::Lists`], and for every other row what Git alone proves at
    /// no GitHub read: enough for the phase of a row already known landed.
    ListsAndGit,
    /// Every row: the command reports each row's phase.
    Phases,
}

/// Whether what `slug` is listed as owing depends on its landing.
fn list_depends_on_landing(state: &Value, slug: &str) -> bool {
    wt_core::parse_work_status(&state["slugs"][slug]["work"]).is_some_and(|work| {
        !work.open_checks().is_empty()
            || (!work.is_terminal()
                && work
                    .blocked_on
                    .as_deref()
                    .is_some_and(|gate| !gate.is_empty()))
    })
}

/// Merges are final, so saved data that shows one still proves it; saved
/// data that shows none proves nothing. Only the merged pull requests of
/// the board's saved GitHub data are kept.
fn saved_merges(ctx: &AppContext) -> Option<GithubData> {
    let (mut data, _) = wt_runtime::DiskCache::<GithubData>::new(&ctx.caches).peek("github")?;
    data.prs.retain(|_, pr| pr.state == "MERGED");
    (!data.prs.is_empty()).then_some(data)
}

/// What a command does with the landing it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    /// Prints it: a snapshot the budget is holding back will do.
    Display,
    /// Decides a write from it: only what the board itself would use.
    Decide,
}

impl Use {
    /// The base tip Git ancestry is proven against, read once for every
    /// pass of one command: origin's own for a write, the base as last
    /// fetched for a printed landing, which costs no round trip.
    fn bases(self) -> PublishedBases {
        match self {
            Self::Display => PublishedBases::last_fetched(),
            Self::Decide => PublishedBases::default(),
        }
    }
}

/// One batched read for exactly these branches: the events daemon's
/// snapshot while it covers them and `usage` accepts its age, else GitHub
/// itself. `None` when neither answers in time; a partial answer is no
/// answer.
async fn fresh_github(ctx: &AppContext, branches: &[String], usage: Use) -> Option<GithubData> {
    // With GitHub switched off for the process, Git is all the evidence
    // there is: its answer stands.
    if std::env::var("WT_GITHUB").as_deref() == Ok("off") {
        return Some(GithubData::default());
    }
    let snapshot = match usage {
        Use::Display => wt_sources::github_events_source::load_for_display(ctx, branches)
            .await
            .map(|shown| shown.read),
        Use::Decide => wt_sources::github_events_source::load(ctx, branches).await,
    };
    if let Some(read) = snapshot {
        return Some(read.data);
    }
    // A read another command or the board just made answers a printed
    // landing as well as a new one would. A write is decided from neither.
    if usage == Use::Display
        && let Some((data, _)) = wt_sources::wait_facts::recent_read(ctx, branches).await
    {
        return Some(data);
    }
    let github = GithubClient::new(
        ctx.processes.clone(),
        ctx.config.paths.main_clone.clone(),
        GithubOptions::from_config(&ctx.config, true),
    );
    let started_ms = now_ms();
    let data = tokio::time::timeout(
        GITHUB_WAIT,
        github.fetch_worktrees(branches, &ctx.cancellation),
    )
    .await
    .ok()?
    .ok()?;
    wt_sources::wait_facts::record_read(ctx, &data, started_ms).await;
    Some(data)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The landing of each of `rows`, by slug, for a command that prints it.
/// The main clone is left out.
pub(crate) async fn read(
    ctx: &AppContext,
    rows: &[WorktreeRecord],
    state: &Value,
    want: Want,
) -> BTreeMap<String, RowLanding> {
    read_for(ctx, rows, state, want, Use::Display).await
}

async fn read_for(
    ctx: &AppContext,
    rows: &[WorktreeRecord],
    state: &Value,
    want: Want,
    usage: Use,
) -> BTreeMap<String, RowLanding> {
    let saved = || async {
        tokio::task::spawn_blocking({
            let ctx = ctx.clone();
            move || saved_merges(&ctx)
        })
        .await
        .ok()
        .flatten()
    };
    resolve(
        ctx,
        rows,
        state,
        want,
        saved,
        &usage.bases(),
        |branches| async move { fresh_github(ctx, &branches, usage).await },
    )
    .await
}

/// `saved` is the board's saved GitHub data, read only when Git leaves a
/// row open.
async fn resolve<S, F>(
    ctx: &AppContext,
    rows: &[WorktreeRecord],
    state: &Value,
    want: Want,
    saved: impl FnOnce() -> S,
    bases: &PublishedBases,
    fresh: impl FnOnce(Vec<String>) -> F,
) -> BTreeMap<String, RowLanding>
where
    S: Future<Output = Option<GithubData>>,
    F: Future<Output = Option<GithubData>>,
{
    let landings = |rows: Vec<WorktreeRecord>, github: Option<GithubData>| async move {
        wt_ops::lifecycle_ops::row_landings(ctx, &rows, state, github.as_ref(), bases).await
    };
    let mut result = BTreeMap::new();
    let mut rows: Vec<WorktreeRecord> = rows.iter().filter(|row| !row.is_main).cloned().collect();
    if want == Want::Lists {
        // Each landing read is several Git subprocesses per row, and a
        // fleet's list names only a few rows that owe a check or hold a gate.
        rows.retain(|row| {
            let slug = row.target.slug();
            let read = list_depends_on_landing(state, slug);
            if !read {
                result.insert(slug.to_owned(), RowLanding::known(Landing::NONE));
            }
            read
        });
        if rows.is_empty() {
            return result;
        }
    }
    let local = landings(rows.clone(), None).await;
    let mut open = Vec::new();
    for row in rows {
        let slug = row.target.slug().to_owned();
        let found = local.get(&slug).copied();
        match found {
            Some(landing) if landing.base => {
                result.insert(slug, RowLanding::known(landing));
            }
            _ if want == Want::Phases || list_depends_on_landing(state, &slug) => open.push(row),
            // Nothing this row is listed as owing depends on the answer.
            _ => {
                result.insert(slug, RowLanding::known(found.unwrap_or(Landing::NONE)));
            }
        }
    }
    let local = LocalLandings {
        result,
        open,
        local,
    };
    settle(ctx, state, local, saved, bases, fresh).await
}

/// What Git alone settled, and the rows it left open for GitHub.
pub(crate) struct LocalLandings {
    result: BTreeMap<String, RowLanding>,
    open: Vec<WorktreeRecord>,
    /// What the Git pass read of every row it could read.
    local: BTreeMap<String, Landing>,
}

/// The rows GitHub's data can read differently from Git alone: those whose
/// branch has a merged pull request in it, which is the only thing a
/// landing proof takes from GitHub, and those Git could not read at all.
/// The Git reads of every other row would come out as they already did.
fn worth_rereading(
    open: &[WorktreeRecord],
    local: &BTreeMap<String, Landing>,
    github: &GithubData,
) -> Vec<WorktreeRecord> {
    open.iter()
        .filter(|row| {
            !local.contains_key(row.target.slug())
                || github
                    .prs
                    .get(&row.target.branch)
                    .is_some_and(|pr| pr.state == "MERGED")
        })
        .cloned()
        .collect()
}

async fn settle<S, F>(
    ctx: &AppContext,
    state: &Value,
    local: LocalLandings,
    saved: impl FnOnce() -> S,
    bases: &PublishedBases,
    fresh: impl FnOnce(Vec<String>) -> F,
) -> BTreeMap<String, RowLanding>
where
    S: Future<Output = Option<GithubData>>,
    F: Future<Output = Option<GithubData>>,
{
    let landings = |rows: Vec<WorktreeRecord>, github: Option<GithubData>| async move {
        wt_ops::lifecycle_ops::row_landings(ctx, &rows, state, github.as_ref(), bases).await
    };
    let LocalLandings {
        mut result,
        mut open,
        local,
    } = local;
    if !open.is_empty()
        && let Some(saved) = saved().await
    {
        let proven = landings(worth_rereading(&open, &local, &saved), Some(saved)).await;
        open.retain(|row| {
            let slug = row.target.slug();
            match proven.get(slug).filter(|landing| landing.base) {
                Some(landing) => {
                    result.insert(slug.to_owned(), RowLanding::known(*landing));
                    false
                }
                None => true,
            }
        });
    }
    if open.is_empty() {
        return result;
    }
    let branches = open
        .iter()
        .map(|row| row.target.branch.clone())
        .collect::<Vec<_>>();
    let fresh = fresh(branches).await;
    let reread = match &fresh {
        Some(github) => worth_rereading(&open, &local, github),
        None => Vec::new(),
    };
    let read = match &fresh {
        Some(_) if !reread.is_empty() => landings(reread.clone(), fresh.clone()).await,
        _ => BTreeMap::new(),
    };
    for row in open {
        let slug = row.target.slug().to_owned();
        let reread = reread.iter().any(|again| again.target.slug() == slug);
        let landing = match (read.get(&slug), local.get(&slug)) {
            (Some(landing), _) => RowLanding::known(*landing),
            // GitHub answered and holds nothing that could change what Git
            // read of this row.
            (None, Some(landing)) if fresh.is_some() && !reread => RowLanding::known(*landing),
            // With an answer from GitHub, a row it could not place has
            // evidence that could not be read: unknown, like no answer at
            // all.
            (None, found) => RowLanding::unknown(found.copied().unwrap_or(Landing::NONE)),
        };
        result.insert(slug, landing);
    }
    result
}

/// The Git half of [`read_with`], for a command whose GitHub read is still
/// under way: the two do not depend on each other.
pub(crate) async fn read_local(
    ctx: &AppContext,
    rows: &[WorktreeRecord],
    state: &Value,
) -> LocalLandings {
    let bases = Use::Display.bases();
    let rows: Vec<WorktreeRecord> = rows.iter().filter(|row| !row.is_main).cloned().collect();
    let local = wt_ops::lifecycle_ops::row_landings(ctx, &rows, state, None, &bases).await;
    let mut result = BTreeMap::new();
    let mut open = Vec::new();
    for row in rows {
        match local.get(row.target.slug()).filter(|landing| landing.base) {
            Some(landing) => {
                result.insert(row.target.slug().to_owned(), RowLanding::known(*landing));
            }
            None => open.push(row),
        }
    }
    LocalLandings {
        result,
        open,
        local,
    }
}

/// Finish [`read_local`] with the command's own GitHub read: that read,
/// when it succeeded, is the fresh answer.
pub(crate) async fn finish_with(
    ctx: &AppContext,
    state: &Value,
    local: LocalLandings,
    fetched: Option<&GithubData>,
) -> BTreeMap<String, RowLanding> {
    let fetched = fetched.cloned();
    settle(
        ctx,
        state,
        local,
        || async { None },
        &Use::Display.bases(),
        |_| async move { fetched },
    )
    .await
}

/// The landing of one row, for a command that prints it.
pub(crate) async fn read_one(
    ctx: &AppContext,
    row: &WorktreeRecord,
    state: &Value,
    want: Want,
) -> RowLanding {
    read(ctx, std::slice::from_ref(row), state, want)
        .await
        .remove(row.target.slug())
        .unwrap_or(RowLanding::unknown(Landing::NONE))
}

/// [`read_one`] for a command that closes or refuses something by the
/// answer: a snapshot older than the board would use is not asked.
pub(crate) async fn read_one_live(
    ctx: &AppContext,
    row: &WorktreeRecord,
    state: &Value,
    want: Want,
) -> RowLanding {
    read_for(ctx, std::slice::from_ref(row), state, want, Use::Decide)
        .await
        .remove(row.target.slug())
        .unwrap_or(RowLanding::unknown(Landing::NONE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::owed;
    use wt_app_core::test_support::CommandFixture;
    use wt_core::{OwedPhase, OwedState};

    async fn git(ctx: &AppContext, args: &[&str]) -> String {
        let output = wt_ops::commands::resolve::run_git(ctx, &ctx.cwd, args.iter().copied())
            .await
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        output.stdout_text().trim().to_owned()
    }

    /// Worktree "one" with a commit of its own that no base branch
    /// contains, as a squash merge leaves it, owing a staging check.
    /// Returns its row, the state and its HEAD.
    async fn squashed(fixture: &CommandFixture) -> (WorktreeRecord, Value, String) {
        let ctx = &fixture.ctx;
        let anchor = git(ctx, &["rev-parse", "HEAD"]).await;
        std::fs::write(ctx.cwd.join("work.txt"), "squashed work\n").unwrap();
        git(ctx, &["add", "work.txt"]).await;
        git(ctx, &["commit", "-m", "feature work"]).await;
        let head = git(ctx, &["rev-parse", "HEAD"]).await;
        let state = ctx
            .database
            .call(move |store| {
                store.set_slug_base("one", Some(("main", Some(&anchor))))?;
                let owing = wt_core::WorkStatusRecord::new(
                    wt_core::WorkState::Ready,
                    "2026-11-24T12:00:00Z",
                )
                .with_check(wt_core::VerifyStage::Staging, "1. open the page");
                let stored: wt_store::WorkStatusRecord =
                    serde_json::from_value(serde_json::to_value(&owing)?)?;
                store.set_slug_work_status("one", Some(&stored))?;
                Ok(store.read_wt_state()?)
            })
            .await
            .unwrap();
        let row = wt_ops::commands::resolve::resolve_named_worktree(ctx, "one")
            .await
            .unwrap();
        (row, state, head)
    }

    /// GitHub data in which `branch` has a pull request in `state` whose
    /// head is `head`.
    fn github(branch: &str, state: &str, head: &str) -> GithubData {
        let mut data = GithubData::default();
        data.prs.insert(
            branch.to_owned(),
            serde_json::from_value(serde_json::json!({
                "number": 42, "url": "https://example.invalid/pull/42",
                "headRefName": branch, "headRefOid": head,
                "baseRefName": "main", "mergeCommitOid": "f".repeat(40),
                "title": "Feature", "isDraft": false, "state": state,
                "checks": "pass", "failedChecks": [], "review": "none",
                "reviewRequests": 0, "requestedReviewers": [], "suggestedReviewers": [],
                "comments": [], "unresolvedThreads": 0, "unresolvedThreadsTotal": 0
            }))
            .unwrap(),
        );
        data.covered_branches.insert(branch.to_owned());
        data
    }

    async fn one(
        fixture: &CommandFixture,
        row: &WorktreeRecord,
        state: &Value,
        want: Want,
        saved: Option<GithubData>,
        fresh: Option<Option<GithubData>>,
    ) -> RowLanding {
        resolve(
            &fixture.ctx,
            std::slice::from_ref(row),
            state,
            want,
            || async move { saved },
            &PublishedBases::default(),
            |branches| async move {
                assert_eq!(branches, std::slice::from_ref(&row.target.branch));
                fresh.expect("no GitHub read is needed here")
            },
        )
        .await
        .remove("one")
        .unwrap()
    }

    #[tokio::test]
    async fn a_squash_merge_is_landed_by_its_pull_request_and_unknown_without_one() {
        let fixture = CommandFixture::new().await.unwrap();
        let (row, state, head) = squashed(&fixture).await;
        let branch = row.target.branch.clone();
        let clock = crate::commands::watch::clock();
        let now = clock.now_ms;
        let phase = |landing: RowLanding| owed::derive(&state, "one", landing, now);

        // Git alone cannot see a squash merge. The pull request merged at
        // this HEAD proves it: the row is landed and its check is due.
        let merged = github(&branch, "MERGED", &head);
        let landing = one(
            &fixture,
            &row,
            &state,
            Want::Lists,
            None,
            Some(Some(merged.clone())),
        )
        .await;
        assert_eq!(landing.word(), "landed");
        let owed = phase(landing);
        assert_eq!(owed.phase, Some(OwedPhase::Due));
        assert_eq!(owed.items[0].state, OwedState::Due);

        // The board's saved data already shows the merge: no read is made.
        let landing = one(&fixture, &row, &state, Want::Phases, Some(merged), None).await;
        assert_eq!(landing.word(), "landed");

        // Saved data that shows no merge proves nothing, and a merged pull
        // request for an earlier HEAD is not this work's: GitHub is asked.
        let open = github(&branch, "OPEN", &head);
        let earlier = github(&branch, "MERGED", &"0".repeat(40));
        for saved in [open.clone(), earlier.clone()] {
            let landing = one(
                &fixture,
                &row,
                &state,
                Want::Lists,
                Some(saved),
                Some(Some(open.clone())),
            )
            .await;
            assert_eq!(landing.word(), "not_landed");
            let owed = phase(landing);
            assert_eq!(owed.phase, None);
            assert_eq!(owed.items[0].state, OwedState::Waiting);
        }
        let landing = one(
            &fixture,
            &row,
            &state,
            Want::Lists,
            None,
            Some(Some(earlier)),
        )
        .await;
        assert_eq!(landing.word(), "not_landed");
        // GitHub answered and names no pull request for the branch.
        let none = one(
            &fixture,
            &row,
            &state,
            Want::Lists,
            None,
            Some(Some(GithubData::default())),
        )
        .await;
        assert_eq!(none.word(), "not_landed");

        // No answer: unknown, never "not landed". Nothing is called due,
        // no phase is reported, and the list says why.
        let unknown = one(&fixture, &row, &state, Want::Lists, None, Some(None)).await;
        assert!(unknown.is_unknown());
        assert_eq!(unknown.word(), "unknown");
        let owed = phase(unknown);
        assert_eq!(owed.phase, None);
        assert_eq!(owed.items[0].state, OwedState::Waiting);
        let text = owed::owed_text("one", "one", &owed, clock);
        assert!(text.ends_with(owed::LANDING_UNKNOWN), "{text}");
        assert!(
            wt_core::checks_to_close(
                wt_core::parse_work_status(&state["slugs"]["one"]["work"]).as_ref(),
                unknown.shown(),
                now
            ) == [wt_core::VerifyStage::Staging]
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn github_is_read_only_where_the_answer_changes_what_is_shown() {
        let fixture = CommandFixture::new().await.unwrap();
        let (row, _, _) = squashed(&fixture).await;
        // The row owes nothing and holds no gate: its list reads the same
        // landed or not, so printing it asks nobody.
        let state = fixture
            .ctx
            .database
            .call(|store| {
                store.set_slug_work_status("one", None)?;
                Ok(store.read_wt_state()?)
            })
            .await
            .unwrap();
        let landing = one(&fixture, &row, &state, Want::Lists, None, None).await;
        assert_eq!(landing.word(), "not_landed");
        // A surface that reports the phase needs the answer for every row.
        let landing = one(&fixture, &row, &state, Want::Phases, None, Some(None)).await;
        assert_eq!(landing.word(), "unknown");
        // A gate is listed only before landing, so it needs the answer too.
        let state = fixture
            .ctx
            .database
            .call(|store| {
                let mut gated = wt_core::WorkStatusRecord::new(
                    wt_core::WorkState::Ready,
                    "2026-11-24T12:00:00Z",
                );
                gated.blocked_on = Some("the release".into());
                let stored: wt_store::WorkStatusRecord =
                    serde_json::from_value(serde_json::to_value(&gated)?)?;
                store.set_slug_work_status("one", Some(&stored))?;
                Ok(store.read_wt_state()?)
            })
            .await
            .unwrap();
        let landing = one(&fixture, &row, &state, Want::Lists, None, Some(None)).await;
        assert_eq!(landing.word(), "unknown");
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_row_whose_list_cannot_change_with_its_landing_is_not_read() {
        let fixture = CommandFixture::new().await.unwrap();
        let (row, _, _) = squashed(&fixture).await;
        let ctx = &fixture.ctx;
        // The work is on the base origin advertises: Git alone proves it.
        let origin = fixture._root.path().join("origin.git");
        git(ctx, &["init", "--bare", origin.to_str().unwrap()]).await;
        git(ctx, &["remote", "add", "origin", origin.to_str().unwrap()]).await;
        git(ctx, &["push", "origin", "HEAD:refs/heads/main"]).await;
        let state = ctx
            .database
            .call(|store| {
                store.set_slug_work_status("one", None)?;
                Ok(store.read_wt_state()?)
            })
            .await
            .unwrap();
        for want in [Want::Phases, Want::ListsAndGit] {
            let landing = one(&fixture, &row, &state, want, None, None).await;
            assert_eq!(landing.word(), "landed", "{want:?}");
        }
        // It owes no check and holds no gate, so its list is the same
        // either way and no Git runs for it: the answer is not a landing.
        let landing = one(&fixture, &row, &state, Want::Lists, None, None).await;
        assert!(!landing.is_landed() && !landing.is_unknown());
        fixture.close().await.unwrap();
    }

    /// GitHub's answer changes what Git read of a row only through a merged
    /// pull request, so only such rows (and rows Git could not read) are
    /// read again once it arrives.
    #[tokio::test]
    async fn only_rows_githubs_answer_can_change_are_read_a_second_time() {
        let fixture = CommandFixture::new().await.unwrap();
        let (row, state, head) = squashed(&fixture).await;
        let ctx = &fixture.ctx;
        let two = wt_ops::commands::resolve::resolve_named_worktree(ctx, "two")
            .await
            .unwrap();
        let rows = [row.clone(), two.clone()];
        let local = read_local(ctx, &rows, &state).await;
        assert_eq!(local.open.len(), 2);
        let branch = row.target.branch.clone();
        let reread = |github: &GithubData| {
            worth_rereading(&local.open, &local.local, github)
                .iter()
                .map(|row| row.target.slug().to_owned())
                .collect::<Vec<_>>()
        };
        assert!(reread(&GithubData::default()).is_empty());
        assert!(reread(&github(&branch, "OPEN", &head)).is_empty());
        assert_eq!(reread(&github(&branch, "MERGED", &head)), ["one"]);
        // A row Git could not read is asked again whatever GitHub says.
        let mut unread = BTreeMap::new();
        unread.insert("one".to_owned(), local.local["one"]);
        assert_eq!(
            worth_rereading(&local.open, &unread, &GithubData::default())
                .iter()
                .map(|row| row.target.slug())
                .collect::<Vec<_>>(),
            ["two"]
        );

        // The answers are the ones a full second pass gave: the merged row
        // lands, the other stays not landed, and with no answer from GitHub
        // both are unknown.
        let merged = github(&branch, "MERGED", &head);
        let landed = finish_with(
            ctx,
            &state,
            read_local(ctx, &rows, &state).await,
            Some(&merged),
        )
        .await;
        assert_eq!(
            (landed["one"].word(), landed["two"].word()),
            ("landed", "not_landed")
        );
        let unanswered = finish_with(ctx, &state, read_local(ctx, &rows, &state).await, None).await;
        assert_eq!(
            (unanswered["one"].word(), unanswered["two"].word()),
            ("unknown", "unknown")
        );
        fixture.close().await.unwrap();
    }
}
