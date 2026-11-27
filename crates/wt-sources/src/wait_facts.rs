//! The board's part of what watches depend on, read by a process with no
//! board: `wt wait` judges its conditions itself, so it works while no TUI
//! is evaluating watches. One pass reads the inventory, the state and, only
//! where a condition asks about a pull request, GitHub once for exactly the
//! worktrees named: the events daemon's snapshot while it is fresh, else one
//! batched request at the pace every other watch read keeps. That pace is
//! shared by every waiting process through a file, so ten waits ask GitHub
//! as often as one. Rows are built by the same rule the engine uses, so a
//! word means the same in both.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use wt_app_core::context::AppContext;
use wt_core::watch::{Condition, PrFacts, RowFacts, RowState, Watch};
use wt_github::{GithubClient, GithubData, GithubOptions};
use wt_platform::lock::FileLock;

use crate::{
    automation_facts::{WatchRowInput, watch_row},
    watch_facts::{PR_INTERVAL, epoch_ms},
};

/// A branch stays in the shared read this long after a wait last asked
/// about it, so waits on different worktrees share one request.
const ASKED_KEEP_MS: u64 = 5 * 60 * 1000;
/// How stale a wait lets its own entry in `asked` get before rewriting it.
const ASKED_REFRESH_MS: u64 = 2 * 60 * 1000;
const UNHELD_KEEP_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const SHARED_LOCK: &str = "wait-github";

/// The GitHub read every `wt wait` of a repository shares, as stored in
/// `wait-github.json` under the cache root. Written only while holding the
/// `wait-github` lock, which a process keeps across its fetch, so a second
/// wait finds the first one's answer instead of asking again.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SharedRead {
    /// When the last fetch began, successful or not.
    fetch_started_ms: u64,
    /// No fetch before this: the last one failed.
    backoff_until_ms: u64,
    failures: u32,
    problem: Option<String>,
    /// `None` after a failed fetch: an answer is only as good as the read
    /// that produced it.
    data: Option<GithubData>,
    /// When a wait last asked about each branch.
    asked: BTreeMap<String, u64>,
    /// Armings a fetch showed GitHub not holding, as `slug|armedAtMs` to
    /// the start of the first such fetch.
    unheld: BTreeMap<String, u64>,
}

#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// The stored answer is young enough and covers the branches.
    Use,
    /// A failed fetch is still backing off.
    Hold,
    /// Ask GitHub about these branches.
    Fetch(Vec<String>),
}

impl SharedRead {
    fn plan(&self, branches: &[String], now_ms: u64) -> Plan {
        if now_ms < self.backoff_until_ms {
            return Plan::Hold;
        }
        // Waits poll in the background, so a low GitHub budget stretches
        // their interval as it does every other poller's. Judged as of the
        // fetch that reported the budget, which the interval counts from.
        let interval = wt_events::paced_interval_ms(
            u64::try_from(PR_INTERVAL.as_millis()).unwrap_or(u64::MAX),
            self.data
                .as_ref()
                .and_then(|data| data.rate_limit)
                .map(wt_ops::events::rate_budget),
            self.fetch_started_ms,
        );
        // A start in the future is a clock that moved, not a fresh answer.
        let fresh = self.fetch_started_ms <= now_ms && now_ms - self.fetch_started_ms < interval;
        let covers = self.data.as_ref().is_some_and(|data| {
            branches
                .iter()
                .all(|branch| data.covered_branches.contains(branch))
        });
        if fresh && covers {
            return Plan::Use;
        }
        let mut all = branches.iter().cloned().collect::<BTreeSet<_>>();
        all.extend(
            self.asked
                .iter()
                .filter(|(_, at)| now_ms.saturating_sub(**at) <= ASKED_KEEP_MS)
                .map(|(branch, _)| branch.clone()),
        );
        Plan::Fetch(all.into_iter().collect())
    }

    /// Note that a wait asked about `branches` now. Returns whether the
    /// record is worth writing for it.
    fn note_asked(&mut self, branches: &[String], now_ms: u64) -> bool {
        let stale = branches.iter().any(|branch| {
            self.asked
                .get(branch)
                .is_none_or(|at| now_ms.saturating_sub(*at) >= ASKED_REFRESH_MS)
        });
        if stale {
            for branch in branches {
                self.asked.insert(branch.clone(), now_ms);
            }
            self.asked
                .retain(|_, at| now_ms.saturating_sub(*at) <= ASKED_KEEP_MS);
        }
        stale
    }

    /// Whether the arming `key`, which the fetch begun at `fetch_ms` shows
    /// GitHub not holding, was shown so by an earlier fetch too. One read
    /// can land between an arm and GitHub recording it; two cannot both.
    fn unheld_twice(&mut self, key: &str, fetch_ms: u64) -> (bool, bool) {
        match self.unheld.get(key) {
            Some(first) if *first < fetch_ms => (true, false),
            Some(_) => (false, false),
            None => {
                self.unheld.insert(key.to_owned(), fetch_ms);
                (false, true)
            }
        }
    }
}

fn shared_path(ctx: &AppContext) -> PathBuf {
    ctx.config.paths.cache_root.join("wait-github.json")
}

async fn load_shared(path: &Path) -> SharedRead {
    match tokio::fs::read(path).await {
        // A file this build cannot read is a missing answer, never an error.
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => SharedRead::default(),
    }
}

async fn save_shared(path: &Path, shared: &SharedRead) {
    let write = async {
        let bytes = serde_json::to_vec(shared).map_err(std::io::Error::other)?;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        tokio::fs::write(&tmp, bytes).await?;
        if let Err(error) = tokio::fs::rename(&tmp, path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(error);
        }
        Ok::<_, std::io::Error>(())
    };
    if let Err(error) = write.await {
        tracing::warn!(%error, path = %path.display(), "cannot store the shared GitHub read");
    }
}

/// What one pass established.
#[derive(Clone, Debug, Default)]
pub struct RowsRead {
    pub rows: BTreeMap<String, RowFacts>,
    /// The named worktrees' pull requests by number.
    pub prs: BTreeMap<u64, PrFacts>,
    /// Each live worktree's branch, for a check's `{{branch}}`.
    pub branches: BTreeMap<String, String>,
    /// Why pull-request answers are missing, when they are.
    pub problem: Option<String>,
}

/// Reads rows for one process's watches.
#[derive(Default)]
pub struct RowReader {
    problem: Option<String>,
    /// Armings GitHub was seen holding during this process, as
    /// `(slug, armed_at_ms)`: the engine records that on the arm itself,
    /// and none may be running.
    seen_armed: BTreeSet<(String, i64)>,
}

/// One GitHub answer and, when it came from the shared read, when the
/// fetch behind it began.
struct GithubRead {
    data: GithubData,
    fetch_started_ms: Option<u64>,
}

/// What the watches ask of each worktree they name.
#[derive(Clone, Copy, Default)]
struct Asked {
    pr: bool,
    landing: bool,
    queue: bool,
}

fn asked(watches: &[Watch]) -> BTreeMap<String, Asked> {
    let mut asked: BTreeMap<String, Asked> = BTreeMap::new();
    for watch in watches {
        for cond in watch.when.iter().filter(|cond| !cond.latched) {
            let (slug, state) = match &cond.cond {
                Condition::Own(state) => (&watch.slug, *state),
                Condition::Worktree { slug, state } => (slug, *state),
                _ => continue,
            };
            let entry = asked.entry(slug.clone()).or_default();
            match state {
                RowState::Ready | RowState::Verified => {}
                RowState::Merged | RowState::Staging | RowState::Production => {
                    entry.pr = true;
                    entry.landing = true;
                }
                RowState::Evicted => {
                    entry.pr = true;
                    entry.queue = true;
                }
                RowState::Green | RowState::Closed | RowState::Settled | RowState::Reviewed => {
                    entry.pr = true;
                }
            }
        }
    }
    asked
}

impl RowReader {
    /// One pass. `Err` when the worktrees or the state cannot be read
    /// at all; a failed GitHub read leaves its answers unknown instead.
    pub async fn read(&mut self, ctx: &AppContext, watches: &[Watch]) -> Result<RowsRead, String> {
        let records = ctx
            .repository
            .inventory(&ctx.cancellation)
            .await
            .map_err(|error| format!("cannot list the worktrees: {error:#}"))?
            .into_iter()
            .filter(|record| !record.is_main)
            .collect::<Vec<_>>();
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .map_err(|error| format!("cannot read wt's state: {error:#}"))?;
        let asked = asked(watches);
        let named = records
            .iter()
            .filter(|record| asked.contains_key(record.target.slug()))
            .cloned()
            .collect::<Vec<_>>();
        let branches = named
            .iter()
            .filter(|record| asked[record.target.slug()].pr)
            .map(|record| record.target.branch.clone())
            .filter(|branch| !branch.is_empty())
            .collect::<Vec<_>>();
        let queue = asked.values().any(|asked| asked.queue);
        let read_started_ms = now_ms();
        let github_read = self.github(ctx, &branches, queue).await;
        let fetch_started_ms = github_read.as_ref().and_then(|read| read.fetch_started_ms);
        let github = github_read.map(|read| read.data);
        let empty = GithubData::default();
        // Armings the fetch shows GitHub holding, and not holding.
        let mut held = Vec::new();
        let mut unheld = Vec::new();
        let lifecycle = if asked.values().any(|asked| asked.landing) {
            let rows = named
                .iter()
                .filter(|record| asked[record.target.slug()].landing)
                .cloned()
                .collect();
            wt_ops::lifecycle_ops::landing_facts(
                ctx,
                rows,
                &state,
                github.as_ref().unwrap_or(&empty),
                &wt_ops::lifecycle_ops::wants_of(watches),
            )
            .await
            .map(|evidence| evidence.rows)
            .unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        let probed = |stage| ctx.config.environments.get(stage).is_some();
        let mut read = RowsRead {
            problem: self.problem.clone().filter(|_| !branches.is_empty()),
            ..RowsRead::default()
        };
        for record in &records {
            let slug = record.target.slug();
            let branch = &record.target.branch;
            let (Some(asked), Some(data), Some(fetch_ms)) =
                (asked.get(slug), github.as_ref(), fetch_started_ms)
            else {
                continue;
            };
            let Ok(arm) = serde_json::from_value::<wt_core::MergeArm>(
                state["slugs"][slug]["mergeArm"].clone(),
            ) else {
                continue;
            };
            let Some(pr) = data
                .prs
                .get(branch)
                .filter(|pr| pr.number == arm.pr && pr.state == "OPEN")
            else {
                continue;
            };
            let queued = data.merge_queue.contains_key(branch);
            if pr.auto_merge.is_some() || queued {
                held.push(arm_key(slug, &arm));
            } else if asked.queue
                && arm.seen_armed_at_ms.is_none()
                && data.merge_queue_complete
                && data.covered_branches.contains(branch)
                && i64::try_from(fetch_ms).unwrap_or(i64::MAX)
                    >= arm
                        .armed_at_ms
                        .saturating_add(wt_automations::MERGE_ARM_GRACE_MS)
            {
                unheld.push(arm_key(slug, &arm));
            }
        }
        let dropped = match fetch_started_ms {
            Some(fetch_ms) if !held.is_empty() || !unheld.is_empty() => {
                settle_unheld(ctx, &held, &unheld, fetch_ms).await
            }
            _ => BTreeSet::new(),
        };
        for record in &records {
            let slug = record.target.slug();
            let branch = &record.target.branch;
            let stored = &state["slugs"][slug];
            read.branches.insert(slug.to_owned(), branch.clone());
            let created_at = stored["createdAt"].as_str();
            if !asked.contains_key(slug) {
                read.rows.insert(
                    slug.to_owned(),
                    RowFacts {
                        exists: true,
                        incarnation: created_at.map(str::to_owned),
                        ..RowFacts::default()
                    },
                );
                continue;
            }
            let data = github.as_ref().unwrap_or(&empty);
            let pr = data.prs.get(branch);
            let queue = data.merge_queue.get(branch);
            let mut merge_arm =
                serde_json::from_value::<wt_core::MergeArm>(stored["mergeArm"].clone()).ok();
            if let Some(arm) = &mut merge_arm {
                let key = (slug.to_owned(), arm.armed_at_ms);
                if read_started_ms > arm.armed_at_ms
                    && pr.is_some_and(|pr| {
                        pr.number == arm.pr
                            && pr.state == "OPEN"
                            && (pr.auto_merge.is_some() || queue.is_some())
                    })
                {
                    self.seen_armed.insert(key.clone());
                }
                if self.seen_armed.contains(&key) {
                    arm.seen_armed_at_ms.get_or_insert(read_started_ms);
                } else if dropped.contains(&arm_key(slug, arm)) {
                    // No engine recorded GitHub holding this merge, or an
                    // earlier wait saw it and exited. Two fetches past the
                    // grace period showing it gone stand in for that
                    // sighting here; the record itself is not rewritten.
                    arm.seen_armed_at_ms.get_or_insert(arm.armed_at_ms);
                }
            }
            let work = wt_core::parse_work_status(&stored["work"]);
            read.rows.insert(
                slug.to_owned(),
                watch_row(WatchRowInput {
                    created_at,
                    work: work.as_ref(),
                    pr,
                    pr_read: github.is_some() && data.covered_branches.contains(branch),
                    queue,
                    queue_complete: data.merge_queue_complete,
                    merge_arm,
                    now_ms: read_started_ms,
                    lifecycle: lifecycle.get(slug).copied().map(|mut lifecycle| {
                        lifecycle.deploys = lifecycle.deploys.for_probes(probed);
                        lifecycle
                    }),
                    separate_production: ctx.config.branch.separate_production(),
                }),
            );
        }
        if let Some(github) = &github {
            read.prs = github
                .prs
                .values()
                .map(|pr| {
                    (
                        pr.number,
                        wt_ops::watch_probe::pr_facts(&pr.state, pr.merge_commit_oid.clone()),
                    )
                })
                .collect();
        }
        Ok(read)
    }

    /// GitHub's answer covering `branches`, or `None` when there is no
    /// current one. The daemon's snapshot does not say whether it holds the
    /// whole merge queue, so a question about the queue goes to GitHub.
    async fn github(
        &mut self,
        ctx: &AppContext,
        branches: &[String],
        queue: bool,
    ) -> Option<GithubRead> {
        if branches.is_empty() {
            return None;
        }
        if std::env::var("WT_GITHUB").as_deref() == Ok("off") {
            self.problem = Some("GitHub is switched off (WT_GITHUB=off)".into());
            return None;
        }
        if !queue && let Some(read) = crate::github_events_source::load(ctx, branches).await {
            self.problem = None;
            return Some(GithubRead {
                data: read.data,
                fetch_started_ms: None,
            });
        }
        let _lock = match FileLock::acquire(
            &ctx.config.paths.lock_dir,
            SHARED_LOCK,
            "wait GitHub read",
            &ctx.cancellation,
        )
        .await
        {
            Ok(lock) => lock,
            Err(error) => {
                self.problem = Some(format!("GitHub could not be read: {error}"));
                return None;
            }
        };
        let path = shared_path(ctx);
        let mut shared = load_shared(&path).await;
        let now = epoch_ms();
        let all = match shared.plan(branches, now) {
            Plan::Hold => {
                self.problem = shared.problem.clone();
                return None;
            }
            Plan::Use => {
                if shared.note_asked(branches, now) {
                    save_shared(&path, &shared).await;
                }
                self.problem = None;
                return shared.data.map(|data| GithubRead {
                    data,
                    fetch_started_ms: Some(shared.fetch_started_ms),
                });
            }
            Plan::Fetch(all) => all,
        };
        shared.note_asked(branches, now);
        shared.fetch_started_ms = now;
        let client = GithubClient::new(
            ctx.processes.clone(),
            ctx.config.paths.main_clone.clone(),
            GithubOptions::from_config(&ctx.config, true),
        );
        match client.fetch_worktrees(&all, &ctx.cancellation).await {
            Ok(data) => {
                shared.failures = 0;
                shared.backoff_until_ms = 0;
                shared.problem = None;
                shared.data = Some(data);
            }
            Err(error) => {
                let reset_at_ms = error.rate_limit_reset_ms();
                let error = error.to_string();
                let limited = wt_events::rate_limited(&error);
                shared.failures = shared.failures.saturating_add(1);
                let failed_at = epoch_ms();
                shared.backoff_until_ms = failed_at.saturating_add(wt_events::failure_wait_ms(
                    u64::try_from(PR_INTERVAL.as_millis()).unwrap_or(u64::MAX),
                    shared.failures,
                    limited,
                    reset_at_ms,
                    failed_at,
                ));
                shared.problem = Some(format!(
                    "GitHub could not be read: {}",
                    wt_core::sanitize_terminal_text(&error)
                ));
                shared.data = None;
            }
        }
        save_shared(&path, &shared).await;
        self.problem = shared.problem.clone();
        shared.data.map(|data| GithubRead {
            data,
            fetch_started_ms: Some(shared.fetch_started_ms),
        })
    }
}

fn arm_key(slug: &str, arm: &wt_core::MergeArm) -> String {
    format!("{slug}|{}", arm.armed_at_ms)
}

/// Record what the fetch begun at `fetch_ms` showed of each arming, and
/// return those a second fetch has now shown GitHub not holding.
async fn settle_unheld(
    ctx: &AppContext,
    held: &[String],
    unheld: &[String],
    fetch_ms: u64,
) -> BTreeSet<String> {
    let Ok(_lock) = FileLock::acquire(
        &ctx.config.paths.lock_dir,
        SHARED_LOCK,
        "wait GitHub read",
        &ctx.cancellation,
    )
    .await
    else {
        return BTreeSet::new();
    };
    let path = shared_path(ctx);
    let mut shared = load_shared(&path).await;
    let mut changed = false;
    for key in held {
        changed |= shared.unheld.remove(key).is_some();
    }
    let mut dropped = BTreeSet::new();
    for key in unheld {
        let (twice, added) = shared.unheld_twice(key, fetch_ms);
        changed |= added;
        if twice {
            dropped.insert(key.clone());
        }
    }
    if changed {
        // An arming is cleared when its merge ends; its sighting is not.
        shared
            .unheld
            .retain(|_, first| fetch_ms.saturating_sub(*first) <= UNHELD_KEEP_MS);
        save_shared(&path, &shared).await;
    }
    dropped
}

fn now_ms() -> i64 {
    i64::try_from(epoch_ms()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_core::watch::{LocalClock, WatchActions, WatchCondition, parse};

    fn watch(slug: &str, conditions: &[&str]) -> Watch {
        let clock = LocalClock {
            now_ms: 0,
            utc_offset_minutes: 0,
        };
        Watch {
            id: "wait".into(),
            slug: slug.into(),
            incarnation: None,
            by: "one".into(),
            created_at_ms: 0,
            utc_offset_minutes: 0,
            when: conditions
                .iter()
                .map(|arg| parse(arg, clock).unwrap())
                .collect::<Vec<WatchCondition>>(),
            then: WatchActions::default(),
            met_at_ms: None,
            last_error: None,
            owner_gone: false,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn github_and_git_are_asked_only_for_what_conditions_name() {
        let asked = asked(&[watch(
            "one",
            &["ready", "wt:two:evicted", "three:merged", "pr:7", "5m"],
        )]);
        let of = |slug: &str| asked[slug];
        assert!(!of("one").pr && !of("one").landing);
        assert!(of("two").pr && of("two").queue && !of("two").landing);
        assert!(of("three").pr && of("three").landing && !of("three").queue);
        assert_eq!(asked.len(), 3);
    }

    fn covering(branches: &[&str], fetch_started_ms: u64) -> SharedRead {
        SharedRead {
            fetch_started_ms,
            data: Some(GithubData {
                covered_branches: branches.iter().map(|branch| (*branch).to_owned()).collect(),
                ..GithubData::default()
            }),
            ..SharedRead::default()
        }
    }

    #[test]
    fn waits_share_one_github_read_and_its_backoff() {
        let one = ["one".to_owned()];
        let minute = u64::try_from(PR_INTERVAL.as_millis()).unwrap();
        let start = 10 * minute;
        // Nothing stored: ask.
        assert_eq!(
            SharedRead::default().plan(&one, start),
            Plan::Fetch(vec!["one".into()])
        );
        // Another process's answer is used while it is young, whoever wrote it.
        let mut shared = covering(&["one", "two"], start);
        assert_eq!(shared.plan(&one, start + minute - 1), Plan::Use);
        assert_eq!(
            shared.plan(&one, start + minute),
            Plan::Fetch(vec!["one".into()])
        );
        // A clock that moved back does not make an answer last.
        assert!(matches!(shared.plan(&one, start - 1), Plan::Fetch(_)));
        // A branch it does not cover is asked for along with the branches
        // other waits asked about lately, so the next answer serves them all.
        shared.note_asked(&["two".to_owned()], start);
        shared.asked.insert("old".into(), 0);
        assert_eq!(
            shared.plan(&["three".to_owned()], start + 1),
            Plan::Fetch(vec!["three".into(), "two".into()])
        );
        // With only the reserve of GitHub's budget left, the stored answer
        // serves every wait until the budget resets.
        let mut low = covering(&["one"], start);
        low.data.as_mut().unwrap().rate_limit = Some(wt_github::RateLimit {
            limit: 5_000,
            remaining: 1_000,
            cost: 5,
            reset_at_ms: start + 20 * minute,
        });
        assert_eq!(low.plan(&one, start + 19 * minute), Plan::Use);
        assert!(matches!(
            low.plan(&one, start + 21 * minute),
            Plan::Fetch(_)
        ));
        // A stored backoff holds a process that never saw the failure.
        let failed = SharedRead {
            fetch_started_ms: start,
            backoff_until_ms: start + 5 * minute,
            failures: 3,
            problem: Some("GitHub could not be read: 502".into()),
            ..SharedRead::default()
        };
        assert_eq!(failed.plan(&one, start + 5 * minute - 1), Plan::Hold);
        assert!(matches!(
            failed.plan(&one, start + 5 * minute),
            Plan::Fetch(_)
        ));
    }

    #[test]
    fn an_asked_branch_is_rewritten_only_when_its_stamp_is_old() {
        let one = ["one".to_owned()];
        let mut shared = SharedRead::default();
        assert!(shared.note_asked(&one, 1_000));
        assert!(!shared.note_asked(&one, 1_000 + ASKED_REFRESH_MS - 1));
        shared.asked.insert("old".into(), 0);
        assert!(shared.note_asked(&one, 1_000 + ASKED_KEEP_MS));
        assert_eq!(shared.asked.keys().collect::<Vec<_>>(), ["one"]);
    }

    #[tokio::test]
    async fn the_shared_read_survives_the_process_and_an_unreadable_file_is_no_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wait-github.json");
        assert_eq!(load_shared(&path).await, SharedRead::default());
        let mut shared = covering(&["one"], 7);
        shared.unheld.insert("one|5".into(), 7);
        save_shared(&path, &shared).await;
        assert_eq!(load_shared(&path).await, shared);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load_shared(&path).await, SharedRead::default());
    }

    #[test]
    fn a_merge_is_dropped_only_when_a_second_fetch_shows_it_gone() {
        let mut shared = SharedRead::default();
        assert_eq!(shared.unheld_twice("one|5", 100), (false, true));
        // The same fetch read again, by this wait or another, proves nothing.
        assert_eq!(shared.unheld_twice("one|5", 100), (false, false));
        assert_eq!(shared.unheld_twice("one|5", 160), (true, false));
        // A new arming starts over.
        assert_eq!(shared.unheld_twice("one|900", 160), (false, true));
    }

    #[tokio::test]
    async fn rows_that_need_no_pull_request_are_read_without_github() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        let mut reader = RowReader::default();
        let read = reader
            .read(ctx, &[watch("one", &["ready", "wt:gone-one:gone"])])
            .await
            .unwrap();
        assert_eq!(read.rows["one"].ready, Some(false));
        assert!(read.rows["two"].exists);
        assert!(!read.rows.contains_key("gone-one"));
        assert_eq!(read.branches["one"], "feature/one");
        assert!(read.problem.is_none() && !shared_path(ctx).exists());
        fixture.close().await.unwrap();
    }
}
