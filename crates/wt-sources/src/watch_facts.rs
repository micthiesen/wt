//! What the automation loop knows that watches depend on: the board's rows,
//! plus a cache of facts that are not on the board (pull requests outside
//! the fleet, commits reaching a stage, release markers, configured checks).
//!
//! Everything that reads GitHub, Git or a probe runs as its own task and
//! lands in the cache; the loop only reads what has arrived. A fact that has
//! not arrived, or whose read failed, is absent, and an absent fact is
//! unknown to a watch.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::{Duration, Instant},
};

use tokio::task::JoinSet;
use wt_app_core::context::AppContext;
use wt_automations::MetWatch;
use wt_core::{
    VerifyStage,
    watch::{
        CheckKey, Condition, LocalClock, Needs, PrFacts, PrGoal, PrLife, RowFacts, Watch,
        WatchFacts, WatchStamps, evaluate,
    },
};

/// GitHub is asked about pull requests outside the fleet at most this often.
pub(crate) const PR_INTERVAL: Duration = Duration::from_secs(60);
/// Commit reach and release markers are local Git reads.
pub const GIT_INTERVAL: Duration = Duration::from_secs(15);
const MAX_CHECKS_RUNNING: usize = 2;

/// A pull-request read that failed.
pub struct PrsFailed {
    pub error: String,
    /// When GitHub's exhausted rate limit resets, in epoch milliseconds,
    /// for a rate-limited read where that could be told.
    pub reset_at_ms: Option<u64>,
}

/// A read that finished.
pub enum Arrived {
    Prs(Result<BTreeMap<u64, PrFacts>, PrsFailed>),
    Git {
        commits: Vec<((String, VerifyStage), Option<bool>)>,
        releases: Vec<(VerifyStage, Option<String>)>,
    },
    Check(CheckKey, Result<bool, String>),
}

#[derive(Default)]
struct CheckRun {
    result: Option<Result<bool, String>>,
    started: Option<Instant>,
    running: bool,
}

/// The facts watches need that the board does not carry.
#[derive(Default)]
pub struct Externals {
    tasks: JoinSet<Arrived>,
    prs: BTreeMap<u64, PrFacts>,
    prs_asked: Option<Instant>,
    prs_running: bool,
    /// Consecutive failed pull-request reads, and the last one's error.
    prs_failures: u32,
    prs_error: Option<String>,
    /// The wait before the next pull-request read while reads fail.
    prs_backoff: Option<Duration>,
    commits: HashMap<(String, VerifyStage), bool>,
    releases: HashMap<VerifyStage, String>,
    git_asked: Option<Instant>,
    git_running: bool,
    checks: BTreeMap<CheckKey, CheckRun>,
}

impl Externals {
    pub fn busy(&self) -> bool {
        !self.tasks.is_empty()
    }

    /// The next finished read. Pending forever when none is running, so
    /// callers guard on [`Externals::busy`].
    pub async fn arrived(&mut self) -> Option<Arrived> {
        loop {
            match self.tasks.join_next().await? {
                Ok(arrived) => return Some(arrived),
                // A read that panicked answered nothing; its "running" flag
                // would otherwise block every later read of its kind.
                Err(error) => {
                    tracing::warn!(%error, "a watch fact read failed");
                    self.prs_running = false;
                    self.git_running = false;
                    for check in self.checks.values_mut() {
                        check.running = false;
                    }
                }
            }
        }
    }

    pub fn absorb(&mut self, arrived: Arrived) {
        match arrived {
            Arrived::Prs(result) => {
                self.prs_running = false;
                // A merged PR is final. Anything else is only as good as the
                // read that produced it, so a failed read forgets it.
                self.prs.retain(|_, pr| merged_for_good(pr));
                match result {
                    Ok(prs) => {
                        self.prs.extend(prs);
                        self.prs_failures = 0;
                        self.prs_backoff = None;
                        if self.prs_error.take().is_some() {
                            tracing::info!("pull requests for watches read again");
                        }
                    }
                    Err(PrsFailed { error, reset_at_ms }) => {
                        // The spacing every GitHub poller uses: until the
                        // rate limit resets when that is known, else doubling.
                        self.prs_failures = self.prs_failures.saturating_add(1);
                        let wait = Duration::from_millis(wt_events::failure_wait_ms(
                            u64::try_from(PR_INTERVAL.as_millis()).unwrap_or(u64::MAX),
                            self.prs_failures,
                            wt_events::rate_limited(&error),
                            reset_at_ms,
                            epoch_ms(),
                        ));
                        self.prs_backoff = Some(wait);
                        // The same failure repeats on every retry.
                        if self.prs_error.as_deref() != Some(error.as_str()) {
                            tracing::warn!(
                                %error,
                                retry_in_seconds = wait.as_secs(),
                                "cannot read pull requests for watches"
                            );
                            self.prs_error = Some(error);
                        }
                    }
                }
            }
            Arrived::Git { commits, releases } => {
                self.git_running = false;
                for (key, reached) in commits {
                    match reached {
                        Some(reached) => self.commits.insert(key, reached),
                        None => self.commits.remove(&key),
                    };
                }
                for (stage, marker) in releases {
                    match marker {
                        Some(marker) => self.releases.insert(stage, marker),
                        None => self.releases.remove(&stage),
                    };
                }
            }
            Arrived::Check(key, result) => {
                if let Some(check) = self.checks.get_mut(&key) {
                    check.running = false;
                    check.result = Some(result);
                }
            }
        }
    }

    /// Start whatever reads are due for what pending watches need, and
    /// forget what none of them needs any more. `fleet_prs` are the pull
    /// requests the board already reads; `branches` maps live slugs to
    /// their branch.
    pub fn request(
        &mut self,
        ctx: &AppContext,
        needs: &Needs,
        fleet_prs: &BTreeMap<u64, PrFacts>,
        branches: &BTreeMap<String, String>,
    ) {
        self.prs.retain(|number, _| needs.prs.contains(number));
        let prs = needs
            .prs
            .iter()
            .copied()
            .filter(|number| {
                !fleet_prs.contains_key(number)
                    && !self.prs.get(number).is_some_and(merged_for_good)
            })
            .collect::<Vec<_>>();
        if !prs.is_empty()
            && !self.prs_running
            && due(self.prs_asked, self.prs_backoff.unwrap_or(PR_INTERVAL))
        {
            self.prs_asked = Some(Instant::now());
            self.prs_running = true;
            let ctx = ctx.clone();
            self.tasks.spawn(async move {
                let cancel = &ctx.cancellation;
                let read = wt_ops::watch_probe::pull_requests(&ctx, &prs, cancel).await;
                Arrived::Prs(read.map_err(|error| PrsFailed {
                    reset_at_ms: error.rate_limit_reset_ms(),
                    error: error.to_string(),
                }))
            });
        }

        self.commits.retain(|key, _| needs.commits.contains(key));
        self.releases
            .retain(|stage, _| needs.releases.contains(stage));
        // Reaching a stage is final; "not yet" and unknown are asked again.
        let commits = needs
            .commits
            .iter()
            .filter(|key| self.commits.get(*key) != Some(&true))
            .cloned()
            .collect::<Vec<_>>();
        let releases = needs.releases.clone();
        if (!commits.is_empty() || !releases.is_empty())
            && !self.git_running
            && due(self.git_asked, GIT_INTERVAL)
        {
            self.git_asked = Some(Instant::now());
            self.git_running = true;
            let ctx = ctx.clone();
            self.tasks.spawn(async move {
                let cancel = &ctx.cancellation;
                let mut reached = Vec::with_capacity(commits.len());
                for (oid, stage) in commits {
                    let answer =
                        wt_ops::watch_probe::commit_reached(&ctx, &oid, stage, cancel).await;
                    reached.push(((oid, stage), answer));
                }
                let mut markers = Vec::with_capacity(releases.len());
                for stage in releases {
                    markers.push((
                        stage,
                        wt_ops::watch_probe::release_marker(&ctx, stage, cancel).await,
                    ));
                }
                Arrived::Git {
                    commits: reached,
                    releases: markers,
                }
            });
        }

        self.checks
            .retain(|key, check| check.running || needs.checks.contains(key));
        let mut running = self.checks.values().filter(|check| check.running).count();
        for key in &needs.checks {
            let check = self.checks.entry(key.clone()).or_default();
            let Some(definition) = ctx.config.checks.iter().find(|check| check.id == key.id) else {
                check.result = Some(Err(format!(
                    "no [[checks]] entry has id \"{}\" in this configuration",
                    key.id
                )));
                continue;
            };
            if check.running
                || running >= MAX_CHECKS_RUNNING
                || !due(check.started, Duration::from_secs(definition.every_seconds))
            {
                continue;
            }
            check.started = Some(Instant::now());
            check.running = true;
            running += 1;
            let (ctx, definition, key) = (ctx.clone(), definition.clone(), key.clone());
            let branch = branches.get(&key.slug).cloned().unwrap_or_default();
            self.tasks.spawn(async move {
                let result = wt_ops::watch_probe::run_check(
                    &ctx,
                    &definition,
                    &key,
                    &branch,
                    &ctx.cancellation,
                )
                .await;
                Arrived::Check(key, result)
            });
        }
    }

    fn fill(&self, facts: &mut WatchFacts) {
        facts
            .prs
            .extend(self.prs.iter().map(|(number, pr)| (*number, pr.clone())));
        facts.commits.extend(
            self.commits
                .iter()
                .map(|(key, reached)| (key.clone(), *reached)),
        );
        facts.releases.extend(
            self.releases
                .iter()
                .map(|(stage, marker)| (*stage, marker.clone())),
        );
        facts.checks.extend(
            self.checks
                .iter()
                .filter_map(|(key, check)| Some((key.clone(), check.result.clone()?))),
        );
    }
}

pub(crate) fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

pub(crate) fn due(asked: Option<Instant>, interval: Duration) -> bool {
    asked.is_none_or(|asked| asked.elapsed() >= interval)
}

fn merged_for_good(pr: &PrFacts) -> bool {
    pr.life == Some(PrLife::Merged) && pr.merge_oid.is_some()
}

/// Everything known this pass. `rows` are the live worktrees; when they come
/// from a confirmed inventory, a worktree a watch names that is not among
/// them is known gone.
pub fn facts(
    watches: &[Watch],
    rows: &BTreeMap<String, RowFacts>,
    inventory_confirmed: bool,
    fleet_prs: &BTreeMap<u64, PrFacts>,
    externals: &Externals,
) -> WatchFacts {
    let mut facts = WatchFacts {
        rows: rows.clone(),
        ..WatchFacts::default()
    };
    if inventory_confirmed {
        for slug in named_worktrees(watches) {
            facts.rows.entry(slug).or_default();
        }
    }
    externals.fill(&mut facts);
    // The board's read of a PR is fresher than the cache's.
    facts
        .prs
        .extend(fleet_prs.iter().map(|(number, pr)| (*number, pr.clone())));
    facts
}

fn named_worktrees(watches: &[Watch]) -> BTreeSet<String> {
    let mut slugs = BTreeSet::new();
    for watch in watches {
        slugs.insert(watch.slug.clone());
        for cond in &watch.when {
            if let Condition::Worktree { slug, .. } | Condition::Gone { slug } = &cond.cond {
                slugs.insert(slug.clone());
            }
        }
    }
    slugs
}

/// What to store for every pending watch: newly held conditions, baselines,
/// "met", the reason a watch cannot progress when there is one, that it was
/// judged (at first, then hourly), and its owner's landing commit once known.
/// `landings` are the landing commits of live rows, by slug.
pub fn stamps(
    watches: &[Watch],
    facts: &WatchFacts,
    landings: &BTreeMap<String, String>,
    now_ms: i64,
) -> Vec<WatchStamps> {
    watches
        .iter()
        .filter(|watch| !watch.is_met())
        .filter_map(|watch| {
            let mut stamps =
                judge(watch, facts, now_ms).unwrap_or_else(|| WatchStamps::new(&watch.id));
            stamps.evaluated_at_ms = WatchStamps::evaluation_due(watch, now_ms);
            stamps.landing_commit = landing_to_keep(watch, facts, landings);
            (!stamps.is_empty()).then_some(stamps)
        })
        .collect()
}

/// The landing commit to put on the watch's record: this process forgets
/// it on exit, and the watch needs it once the worktree is removed.
fn landing_to_keep(
    watch: &Watch,
    facts: &WatchFacts,
    landings: &BTreeMap<String, String>,
) -> Option<String> {
    let oid = landings.get(&watch.slug)?;
    let row = facts.rows.get(&watch.slug)?;
    (!watch.owner_gone
        && watch.follows_own_landing()
        && row.exists
        && watch.owned_by(row.incarnation.as_deref())
        && watch.landing_commit() != Some(oid))
    .then(|| oid.clone())
}

fn judge(watch: &Watch, facts: &WatchFacts, now_ms: i64) -> Option<WatchStamps> {
    let evaluation = evaluate(watch, facts, now_ms);
    // The evaluation reports a failing check. A pull request closed without
    // merging is not an error to it, but the person waiting should know.
    let mut unset = watch.clone();
    unset.last_error = None;
    let check_error = evaluate(&unset, facts, now_ms)
        .stamps
        .and_then(|stamps| stamps.last_error)
        .flatten();
    let error = check_error.or_else(|| closed_unmerged(watch, facts));
    let mut stamps = evaluation
        .stamps
        .unwrap_or_else(|| WatchStamps::new(&watch.id));
    stamps.last_error = (watch.last_error != error).then_some(error);
    (stamps.met_at_ms.is_some() || !stamps.conditions.is_empty() || stamps.last_error.is_some())
        .then_some(stamps)
}

fn closed_unmerged(watch: &Watch, facts: &WatchFacts) -> Option<String> {
    watch.when.iter().find_map(|cond| match &cond.cond {
        Condition::Pr { number, goal }
            if !cond.latched
                && *goal != PrGoal::Closed
                && facts.prs.get(number).and_then(|pr| pr.life) == Some(PrLife::Closed) =>
        {
            Some(format!(
                "#{number} was closed without merging; still waiting in case it is reopened"
            ))
        }
        _ => None,
    })
}

/// Each live worktree's `createdAt`, for deciding whose watches and due
/// steps are whose. The inventory may be older than the state: a watch set
/// after it was read may belong to a worktree it has not seen yet, and so
/// may a worktree the state records as created (or restored, which carries
/// its due steps over) since. Their owners count as live.
pub fn live_owners(
    rows: &BTreeMap<String, RowFacts>,
    watches: &[Watch],
    state: &serde_json::Value,
    inventory_read_at_ms: i64,
) -> BTreeMap<String, Option<String>> {
    let mut live: BTreeMap<_, _> = rows
        .iter()
        .filter(|(_, row)| row.exists)
        .map(|(slug, row)| (slug.clone(), row.incarnation.clone()))
        .collect();
    for watch in watches {
        if watch.created_at_ms >= inventory_read_at_ms {
            live.insert(watch.slug.clone(), watch.incarnation.clone());
        }
    }
    for (slug, entry) in state["slugs"].as_object().into_iter().flatten() {
        let created = entry["createdAt"].as_str();
        if created
            .and_then(wt_core::parse_iso_millis)
            .is_some_and(|at| at >= inventory_read_at_ms)
        {
            live.insert(slug.clone(), created.map(str::to_owned));
        }
    }
    live
}

/// Whether any watch or due step belongs to a worktree that is gone or was
/// replaced. A due step can be left behind with no watch at all: a build
/// from before due steps removes a worktree without moving them.
pub fn has_orphans(
    watches: &[Watch],
    due: &[wt_core::OwedRecord],
    live: &BTreeMap<String, Option<String>>,
) -> bool {
    watches.iter().any(|watch| !owned(watch, live))
        || due.iter().any(|step| {
            !live
                .get(&step.slug)
                .is_some_and(|created_at| step.owned_by(created_at.as_deref()))
        })
}

fn owned(watch: &Watch, live: &BTreeMap<String, Option<String>>) -> bool {
    watch.owner_gone
        || live
            .get(&watch.slug)
            .is_some_and(|created_at| watch.owned_by(created_at.as_deref()))
}

/// The met watches whose actions may be delivered: the owner is the worktree
/// the watch was set on, or the watch outlived it by design. A later
/// worktree with the same slug never receives another one's wake-up.
pub fn deliverable(
    watches: &[Watch],
    live: &BTreeMap<String, Option<String>>,
    now_ms: i64,
) -> Vec<MetWatch> {
    watches
        .iter()
        .filter(|watch| watch.is_met() && owned(watch, live))
        .map(|watch| {
            let clock = LocalClock {
                now_ms,
                utc_offset_minutes: watch.utc_offset_minutes,
            };
            MetWatch {
                id: watch.id.clone(),
                slug: watch.slug.clone(),
                by: watch.by.clone(),
                held: watch
                    .when
                    .iter()
                    .map(|cond| cond.describe(clock))
                    .collect::<Vec<_>>()
                    .join("; "),
                actions: watch
                    .then
                    .remaining()
                    .filter_map(|action| Some((action, watch.then.get(action)?.to_owned())))
                    .collect(),
                cleared_gate: wt_store::cleared_gate(watch).map(str::to_owned),
            }
        })
        .collect()
}

/// What decides which facts watches need: who they are and what they wait
/// for, not what they have seen so far.
pub fn identity(state: &serde_json::Value) -> serde_json::Value {
    wt_store::stored_watches(state)
        .iter()
        .map(|watch| {
            serde_json::json!([
                watch.id,
                watch.slug,
                watch.owner_gone,
                watch
                    .when
                    .iter()
                    .map(|cond| cond.cond.to_string())
                    .collect::<Vec<_>>(),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_core::watch::{WatchAction, WatchActions, WatchCondition, parse};

    const NOON: i64 = 1_795_521_600_000;

    fn watch(id: &str, slug: &str, conditions: &[&str]) -> Watch {
        let clock = LocalClock {
            now_ms: NOON,
            utc_offset_minutes: 0,
        };
        Watch {
            id: id.into(),
            slug: slug.into(),
            incarnation: Some("first".into()),
            by: slug.into(),
            created_at_ms: NOON,
            utc_offset_minutes: 0,
            when: conditions
                .iter()
                .map(|arg| parse(arg, clock).unwrap())
                .collect(),
            then: WatchActions {
                wake: Some("go".into()),
                ..WatchActions::default()
            },
            met_at_ms: None,
            last_error: None,
            owner_gone: false,
            extra: Default::default(),
        }
    }

    fn row(incarnation: &str) -> RowFacts {
        RowFacts {
            exists: true,
            incarnation: Some(incarnation.into()),
            ..RowFacts::default()
        }
    }

    fn apply(watch: &mut Watch, facts: &WatchFacts, now_ms: i64) -> bool {
        match judge(watch, facts, now_ms) {
            Some(stamps) => stamps.apply(watch).changed,
            None => false,
        }
    }

    /// A release watch set while the stage's release was unknown waits for
    /// the release after the first one it sees, not for that one.
    #[test]
    fn a_release_with_no_baseline_takes_the_first_marker_it_sees_as_one() {
        let mut watch = watch("aaaaaa", "one", &["release:production"]);
        let mut facts = WatchFacts::default();
        assert!(!apply(&mut watch, &facts, NOON));
        facts.releases.insert(VerifyStage::Production, "old".into());
        assert!(apply(&mut watch, &facts, NOON + 1));
        assert_eq!(watch.when[0].baseline.as_deref(), Some("old"));
        assert!(!watch.is_met());
        assert!(!apply(&mut watch, &facts, NOON + 2));
        facts.releases.insert(VerifyStage::Production, "new".into());
        assert!(apply(&mut watch, &facts, NOON + 3));
        assert_eq!(watch.met_at_ms, Some(NOON + 3));
    }

    /// The loop learns a row's landing commit and that it judged a watch;
    /// both go on the record, each written once rather than every pass.
    #[test]
    fn a_pass_records_the_landing_commit_and_that_it_judged_the_watch() {
        let mut follows = watch("aaaaaa", "one", &["production"]);
        follows.then = WatchActions {
            notify: Some(String::new()),
            ..WatchActions::default()
        };
        let other = watch("bbbbbb", "one", &["green"]);
        let known = WatchFacts {
            rows: BTreeMap::from([("one".to_owned(), row("first"))]),
            ..WatchFacts::default()
        };
        let landings = BTreeMap::from([("one".to_owned(), "0123abcdef".to_owned())]);
        let at = NOON + 90_000;
        let both = [follows.clone(), other.clone()];
        let first = stamps(&both, &known, &landings, at);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].landing_commit.as_deref(), Some("0123abcdef"));
        assert_eq!(first[0].evaluated_at_ms, Some(NOON + 60_000));
        // A watch that is not about its own landing has no use for it.
        assert_eq!(first[1].landing_commit, None);
        assert_eq!(first[1].evaluated_at_ms, Some(NOON + 60_000));
        assert!(first[0].apply(&mut follows).changed);
        assert!(first[1].apply(&mut other.clone()).changed);
        assert_eq!(follows.landing_commit(), Some("0123abcdef"));

        // Nothing more to write for the next hour.
        assert_eq!(
            stamps(&[follows.clone()], &known, &landings, at + 3_600_000),
            []
        );
        let next = stamps(&[follows.clone()], &known, &landings, at + 3_630_000);
        assert_eq!(next[0].evaluated_at_ms, Some(NOON + 3_720_000));
        assert_eq!(next[0].landing_commit, None);

        // Another worktree took the slug: its landing is not this watch's.
        let reborn = WatchFacts {
            rows: BTreeMap::from([("one".to_owned(), row("second"))]),
            ..WatchFacts::default()
        };
        let mut fresh = watch("cccccc", "one", &["staging"]);
        fresh.incarnation = Some("first".into());
        assert_eq!(
            stamps(&[fresh], &reborn, &landings, at)[0].landing_commit,
            None
        );
    }

    #[test]
    fn a_pull_request_closed_unmerged_keeps_waiting_and_says_so() {
        let mut watch = watch("aaaaaa", "one", &["#7"]);
        let closed = |life| WatchFacts {
            prs: [(
                7,
                PrFacts {
                    life: Some(life),
                    ..PrFacts::default()
                },
            )]
            .into(),
            ..WatchFacts::default()
        };
        assert!(apply(&mut watch, &closed(PrLife::Closed), NOON));
        assert!(!watch.is_met());
        assert!(
            watch
                .last_error
                .as_deref()
                .unwrap()
                .starts_with("#7 was closed without merging")
        );
        // Written once, not on every pass.
        assert_eq!(judge(&watch, &closed(PrLife::Closed), NOON + 1), None);
        // Reopened: the note goes.
        assert!(apply(&mut watch, &closed(PrLife::Open), NOON + 2));
        assert_eq!(watch.last_error, None);
        assert!(apply(&mut watch, &closed(PrLife::Merged), NOON + 3));
        assert!(watch.is_met());
    }

    /// Slugs are reused. A watch belongs to the worktree it was set on.
    #[test]
    fn a_later_worktree_with_the_same_slug_gets_nothing_of_an_earlier_ones_watch() {
        let mut met = watch("aaaaaa", "one", &["12:30"]);
        met.met_at_ms = Some(NOON);
        let watches = [met.clone()];
        let first = BTreeMap::from([("one".to_owned(), row("first"))]);
        let second = BTreeMap::from([("one".to_owned(), row("second"))]);
        let read_later = NOON + 60_000;

        let none = serde_json::json!({});
        let live = live_owners(&first, &watches, &none, read_later);
        assert!(!has_orphans(&watches, &[], &live));
        let delivered = deliverable(&watches, &live, NOON);
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].held, "12:30");
        assert_eq!(delivered[0].actions, [(WatchAction::Wake, "go".to_owned())]);

        for rows in [&second, &BTreeMap::new()] {
            let live = live_owners(rows, &watches, &none, read_later);
            assert!(has_orphans(&watches, &[], &live));
            assert_eq!(deliverable(&watches, &live, NOON), []);
        }
        // Its conditions about itself are unknown too, never held by the
        // newcomer's state.
        let mut own = watch("bbbbbb", "one", &["ready"]);
        let mut newcomer = row("second");
        newcomer.ready = Some(true);
        let known = facts(
            std::slice::from_ref(&own),
            &BTreeMap::from([("one".to_owned(), newcomer)]),
            true,
            &BTreeMap::new(),
            &Externals::default(),
        );
        apply(&mut own, &known, NOON);
        assert!(!own.is_met());

        // An inventory read before the watch was set says nothing about its
        // owner; a watch that outlived its owner needs none.
        let live = live_owners(&BTreeMap::new(), &watches, &none, NOON - 1);
        assert!(!has_orphans(&watches, &[], &live));
        met.owner_gone = true;
        let live = live_owners(
            &BTreeMap::new(),
            std::slice::from_ref(&met),
            &none,
            read_later,
        );
        assert_eq!(deliverable(&[met], &live, NOON).len(), 1);
    }

    /// A due step has no watch left. One whose worktree is gone must still
    /// be settled, and one whose worktree the inventory has not seen yet
    /// must not be.
    #[test]
    fn a_due_step_is_an_orphan_only_when_its_worktree_is_known_gone() {
        let step = |incarnation: &str| wt_core::OwedRecord {
            id: "93co6q".into(),
            slug: "one".into(),
            incarnation: Some(incarnation.into()),
            by: "one".into(),
            what: "confirm prod logs".into(),
            when_text: "in production".into(),
            due_at_ms: NOON - 3_600_000,
            utc_offset_minutes: 0,
            unconfirmed: false,
            before_release: false,
            extra: Default::default(),
        };
        let none = serde_json::json!({});
        let read_at = NOON;
        // A build from before due steps removed the worktree: no watch is
        // left to say so, and the step alone is the orphan.
        let live = live_owners(&BTreeMap::new(), &[], &none, read_at);
        assert!(has_orphans(&[], &[step("first")], &live));
        let rows = BTreeMap::from([("one".to_owned(), row("first"))]);
        let live = live_owners(&rows, &[], &none, read_at);
        assert!(!has_orphans(&[], &[step("first")], &live));
        // Restored after the inventory was read: the state records the new
        // worktree, the inventory does not show it yet, and its step stays.
        let restored = "2026-11-24T12:00:05Z";
        assert!(wt_core::parse_iso_millis(restored).unwrap() > read_at);
        let state = serde_json::json!({ "slugs": { "one": { "createdAt": restored } } });
        let live = live_owners(&BTreeMap::new(), &[], &state, read_at);
        assert!(!has_orphans(&[], &[step(restored)], &live));
        // A worktree created before the inventory that does not list it is gone.
        let state = serde_json::json!({ "slugs": { "one": { "createdAt": "first" } } });
        let live = live_owners(&BTreeMap::new(), &[], &state, read_at);
        assert!(has_orphans(&[], &[step("first")], &live));
    }

    #[test]
    fn a_named_worktree_missing_from_a_confirmed_inventory_is_gone() {
        let watches = [watch("aaaaaa", "one", &["two:gone"])];
        let rows = BTreeMap::from([("one".to_owned(), row("first"))]);
        let externals = Externals::default();
        let unconfirmed = facts(&watches, &rows, false, &BTreeMap::new(), &externals);
        assert!(!unconfirmed.rows.contains_key("two"));
        let confirmed = facts(&watches, &rows, true, &BTreeMap::new(), &externals);
        assert!(!confirmed.rows["two"].exists);
        let mut pending = watches[0].clone();
        assert!(!apply(&mut pending, &unconfirmed, NOON));
        assert!(apply(&mut pending, &confirmed, NOON));
        assert!(pending.is_met());
    }

    #[test]
    fn identity_ignores_what_a_watch_has_seen() {
        let mut stored = watch("aaaaaa", "one", &["#7:production"]);
        let state = |watch: &Watch| serde_json::json!({"watches": [watch]});
        let before = identity(&state(&stored));
        stored.when[0].first_held_at_ms = Some(NOON);
        stored.last_error = Some("x".into());
        assert_eq!(identity(&state(&stored)), before);
        stored.when[0] = WatchCondition::new(
            Condition::Commit {
                oid: "abcdef0".into(),
                stage: VerifyStage::Production,
            },
            None,
        );
        assert_ne!(identity(&state(&stored)), before);
    }

    #[test]
    fn a_failed_pull_request_read_forgets_all_but_final_answers() {
        let mut externals = Externals::default();
        let pr = |life, oid: Option<&str>| PrFacts {
            life: Some(life),
            merge_oid: oid.map(str::to_owned),
            ..PrFacts::default()
        };
        externals.absorb(Arrived::Prs(Ok(BTreeMap::from([
            (1, pr(PrLife::Open, None)),
            (2, pr(PrLife::Merged, Some("abc"))),
        ]))));
        let failed = |reset_at_ms| {
            Arrived::Prs(Err(PrsFailed {
                error: "API rate limit exceeded".into(),
                reset_at_ms,
            }))
        };
        externals.absorb(failed(None));
        // Not asked again every minute while GitHub refuses: the shared
        // rate-limit wait, or until the reset when GitHub names one.
        assert_eq!(externals.prs_backoff, Some(Duration::from_secs(5 * 60)));
        externals.absorb(failed(Some(epoch_ms() + 40 * 60 * 1000)));
        assert!(externals.prs_backoff > Some(Duration::from_secs(39 * 60)));
        assert!(externals.prs_backoff < Some(Duration::from_secs(41 * 60)));
        assert_eq!(externals.prs_failures, 2);
        let mut known = WatchFacts::default();
        externals.fill(&mut known);
        assert_eq!(known.prs.keys().copied().collect::<Vec<_>>(), [2]);
        externals.absorb(Arrived::Git {
            commits: vec![(("abc".into(), VerifyStage::Staging), Some(true))],
            releases: vec![(VerifyStage::Staging, Some("abc".into()))],
        });
        externals.absorb(Arrived::Git {
            commits: vec![(("abc".into(), VerifyStage::Staging), None)],
            releases: vec![(VerifyStage::Staging, None)],
        });
        let mut known = WatchFacts::default();
        externals.fill(&mut known);
        assert!(known.commits.is_empty() && known.releases.is_empty());
    }
}
