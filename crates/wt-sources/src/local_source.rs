//! Git facts and durable presentation state have independent invalidation.
//! Editing a title or status must not respawn Git across the whole fleet.
use std::{collections::BTreeSet, sync::Arc, time::Duration};

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use wt_config::Config;
use wt_core::Domain;
use wt_runtime::{
    DerivedPublisher, DiskCache, KeyedPolicy, KeyedReader, LeafHandle, Persistence, RefreshPolicy,
    SourceHandle, SourceSnapshot, SourceSpec, SourceState, TaskScope, derived_channel,
    start_keyed_source, start_source,
};
use wt_tui::Board;
use wt_vcs::{GitRepository, GitStatus, WorktreeRecord, WorktreeSnapshot};

use crate::{freshness, inventory};
use wt_app_core::context::AppContext;

pub type Metadata = (Value, BTreeSet<String>);

pub struct LocalSources {
    pub board: SourceHandle<Board>,
    pub metadata: LeafHandle<Metadata>,
    pub git: LeafHandle<Vec<WorktreeSnapshot>>,
    pub edits: SourceHandle<std::collections::BTreeMap<String, i64>>,
}

/// Spacing of background work, from the start of the preceding listing or
/// read of the same checkout. Checkouts whose files change continuously then
/// cost a few `git` runs and board publishes a second, where an isolated
/// change is still read at once. A read the user caused is never held.
const GIT_BACKGROUND_INTERVAL: Duration = Duration::from_millis(400);

/// The key a checkout has in the Git leaf: its row key on the board. A second
/// checkout that claims the same key keeps its row under a key of its own.
pub(crate) fn git_keys<'a>(records: impl IntoIterator<Item = &'a WorktreeRecord>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    records
        .into_iter()
        .map(|record| {
            let key = wt_core::worktree_target_key(&record.target);
            if seen.insert(key.clone()) {
                key
            } else {
                format!("{key}\0{}", record.target.path)
            }
        })
        .collect()
}

/// Reads the fleet one checkout at a time: `git worktree list` is the
/// listing, one `git status` is a row.
struct GitReader {
    repository: Arc<GitRepository>,
    /// The same repository, admitted ahead of background subprocesses: for
    /// reads that confirm a change the user is waiting on.
    confirming: GitRepository,
    coverage: freshness::Coverage,
}

impl GitReader {
    fn repository(&self, urgent: bool) -> &GitRepository {
        if urgent {
            &self.confirming
        } else {
            &self.repository
        }
    }
}

impl KeyedReader for GitReader {
    type Entry = WorktreeRecord;
    type Value = (Option<GitStatus>, Option<String>);
    type Row = WorktreeSnapshot;
    type Sweep = wt_vcs::FleetScan;

    /// The machine-wide turn for reading every checkout. It is bounded by
    /// its owner: when none comes the sweep runs without, since the sweep is
    /// what repairs a change no event was routed for.
    async fn sweep(&self, cancel: CancellationToken) -> Self::Sweep {
        self.repository.fleet_scan(&cancel).await
    }

    /// A second checkout claiming a key (`git_keys`) is confirmed with the
    /// first: a change to the row of that key has reached both or neither.
    fn stamp_key(key: &str) -> &str {
        key.split('\0').next().unwrap_or(key)
    }

    async fn index(
        &self,
        urgent: bool,
        cancel: CancellationToken,
    ) -> Result<Vec<(String, WorktreeRecord)>, String> {
        let records = self
            .repository(urgent)
            .inventory(&cancel)
            .await
            .map_err(|error| error.to_string())?;
        // A checkout is read only once its files are watched, so no change
        // can fall between its first read and the watch that reports the
        // next one.
        self.coverage.cover(&records).await;
        Ok(git_keys(&records).into_iter().zip(records).collect())
    }

    async fn read(
        &self,
        entry: WorktreeRecord,
        urgent: bool,
        cancel: CancellationToken,
    ) -> Self::Value {
        match self.repository(urgent).status(&entry, &cancel).await {
            Ok(status) => (Some(status), None),
            Err(error) => (None, Some(error.to_string())),
        }
    }

    fn same_subject(read: &WorktreeRecord, listed: &WorktreeRecord) -> bool {
        read.target.path == listed.target.path
    }

    fn row(entry: &WorktreeRecord, value: &Self::Value) -> WorktreeSnapshot {
        WorktreeSnapshot {
            worktree: entry.clone(),
            status: value.0.clone(),
            error: value.1.clone(),
        }
    }
}

fn start_git(
    scope: &TaskScope,
    context: &AppContext,
    coverage: freshness::Coverage,
) -> LeafHandle<Vec<WorktreeSnapshot>> {
    start_keyed_source(
        scope,
        git_spec(context),
        KeyedPolicy {
            concurrency: context.repository.max_concurrent_status(),
            key_interval: GIT_BACKGROUND_INTERVAL,
        },
        GitReader {
            repository: context.repository.clone(),
            confirming: context
                .repository
                .with_process_class(wt_platform::process::ProcessClass::Confirming),
            coverage,
        },
    )
}

pub fn start(scope: &TaskScope, context: &AppContext) -> LocalSources {
    let (coverage, covering) = freshness::coverage();
    let git = start_git(scope, context, coverage);
    let metadata = start_source(scope, metadata_spec(), {
        let context = context.clone();
        move |_cancel| {
            let context = context.clone();
            async move {
                fn read(store: &mut wt_store::Store) -> anyhow::Result<Metadata> {
                    Ok((store.read_wt_state()?, store.read_archived_keys()?))
                }
                // On the read connection, so a refresh does not queue behind
                // a write waiting for another process's lock. It sees every
                // write whose call returned before the read began, and the
                // read begins a tick after this fetch's stamp: a fetch stamped
                // at or after an accepted write therefore shows it.
                next_tick().await;
                let metadata = context.database.read(read).await?;
                // Reconciliation acts only on recorded removals. Without one
                // it writes nothing, so what was just read is what a second
                // read would return.
                if metadata.0["removals"]
                    .as_object()
                    .is_none_or(serde_json::Map::is_empty)
                {
                    return Ok(metadata);
                }
                wt_ops::commands::_destroy::reconcile_removals(&context, &metadata.0).await?;
                context.database.read(read).await
            }
        }
    });
    let board = project(scope, context.config.clone(), git.view(), metadata.view());
    let edits = freshness::start(
        scope,
        context.config.clone(),
        board.clone(),
        git.view(),
        metadata.view(),
        covering,
    );
    LocalSources {
        board,
        metadata,
        git,
        edits,
    }
}

/// Wait for the stamp clock's next millisecond. A write is stamped as
/// accepted after it returned, and a fetch counts as following it when its
/// stamp is the same millisecond or later. A read that began in the
/// millisecond of its fetch's stamp could run before a write stamped in
/// that same millisecond and still count as following it.
async fn next_tick() {
    let tick = wt_runtime::Stamp::now();
    while tick.at_or_after(&wt_runtime::Stamp::now()) == Some(true) {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// The Git inventory survives restarts as restored display data; only a
/// live scan can confirm it.
fn git_spec(context: &AppContext) -> SourceSpec<Vec<WorktreeSnapshot>> {
    SourceSpec {
        name: "git",
        domain: Domain::Git,
        persistence: Persistence::Disk(DiskCache::new(&context.caches)),
        policy: RefreshPolicy {
            debounce: Duration::from_millis(25),
            minimum_interval: GIT_BACKGROUND_INTERVAL,
        },
    }
}

fn metadata_spec() -> SourceSpec<Metadata> {
    SourceSpec {
        name: "metadata",
        domain: Domain::Metadata,
        persistence: Persistence::Never("authoritative SQLite; read directly"),
        policy: RefreshPolicy {
            debounce: Duration::from_millis(15),
            minimum_interval: Duration::from_millis(50),
        },
    }
}

fn project(
    scope: &TaskScope,
    config: Arc<Config>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<Metadata>,
) -> SourceHandle<Board> {
    let (source, publisher) = derived_channel(scope, "local-board");
    let cancellation = scope.token();
    scope.spawn(run_projection(
        cancellation,
        publisher,
        config,
        git,
        metadata,
    ));
    source
}

async fn run_projection(
    cancellation: tokio_util::sync::CancellationToken,
    mut publisher: DerivedPublisher<Board>,
    config: Arc<Config>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<Metadata>,
) {
    let mut git_updates = git.subscribe();
    let mut metadata_updates = metadata.subscribe();
    git_updates.mark_changed();
    metadata_updates.mark_changed();
    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => break,
            request = publisher.requested() => {
                if request.is_none() { break; }
                // A refresh reads both inputs. A request that only names
                // checkouts (or just their listing) is about Git alone and
                // goes to the leaf that can read them one at a time.
                let demand = publisher.take_demand();
                if demand.whole.is_some() || demand.index.is_none() {
                    git.refresh(); metadata.refresh();
                } else if let Some(request) = demand.index {
                    git.request_keys(request, demand.keys.into_keys());
                }
                continue;
            }
            changed = git_updates.changed() => {
                if changed.is_err() { break; }
                git_updates.borrow_and_update();
            }
            changed = metadata_updates.changed() => {
                if changed.is_err() { break; }
                metadata_updates.borrow_and_update();
            }
        }
        let facts = git_updates.borrow().clone();
        let state = metadata_updates.borrow().clone();
        let config = config.clone();
        let failure = facts.derive(None).with_input(&state);
        let snapshot =
            match tokio::task::spawn_blocking(move || compose(&config, facts, state)).await {
                Ok(snapshot) if !cancellation.is_cancelled() => snapshot,
                Ok(_) => break,
                Err(error) => {
                    failure.with_state(SourceState::Failed(format!("local board: {error}").into()))
                }
            };
        publisher.publish_if_changed(snapshot);
    }
}

fn compose(
    config: &Config,
    git: SourceSnapshot<Vec<WorktreeSnapshot>>,
    metadata: SourceSnapshot<Metadata>,
) -> SourceSnapshot<Board> {
    let state = SourceState::combine([("Git", git.state()), ("State", metadata.state())]);
    let data = git
        .data()
        .zip(metadata.data())
        .map(|(git, metadata)| Arc::new(inventory::board(config, git, &metadata.0, &metadata.1)));
    git.derive(data).with_input(&metadata).with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_runtime::{Stamp, leaf_channel};

    #[tokio::test]
    async fn metadata_changes_reuse_git_facts_while_git_is_busy_or_failed() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let (git, mut git_publisher) = leaf_channel(&scope, git_spec(&fixture.ctx));
        let (metadata, mut metadata_publisher) = leaf_channel(&scope, metadata_spec());
        let facts = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        git_publisher.publish_live(facts.clone(), Stamp::now());
        let mut state = serde_json::json!({"slugs":{}});
        metadata_publisher.publish_live((state.clone(), BTreeSet::new()), Stamp::now());
        let source = project(
            &scope,
            fixture.ctx.config.clone(),
            git.view(),
            metadata.view(),
        );
        let mut updates = source.subscribe();
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|s| s.data().is_some()),
        )
        .await
        .unwrap()
        .unwrap();
        git_publisher.set_state(SourceState::Refreshing);
        state["slugs"]["one"] = serde_json::json!({"manualTitle":"Edited during refresh"});
        metadata_publisher.publish_live((state.clone(), BTreeSet::new()), Stamp::now());
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|s| {
                s.data()
                    .is_some_and(|b| b.rows.iter().any(|r| r.title == "Edited during refresh"))
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), git_publisher.requested())
                .await
                .is_err()
        );
        git_publisher.set_state(SourceState::Failed("fixture Git failure".into()));
        state["slugs"]["one"]["manualTitle"] = "Edited after failure".into();
        metadata_publisher.publish_live((state, BTreeSet::new()), Stamp::now());
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|s| {
                matches!(s.state(), SourceState::Failed(_))
                    && s.data()
                        .is_some_and(|b| b.rows.iter().any(|r| r.title == "Edited after failure"))
            }),
        )
        .await
        .unwrap()
        .unwrap();
        // Explicit refresh remains an all-source operation.
        source.refresh();
        assert_eq!(git_publisher.requested().await, Some(()));
        assert_eq!(metadata_publisher.requested().await, Some(()));
        assert!(git_publisher.take_demand().whole.is_some());
        // A request that names checkouts, or only their listing, is about
        // Git alone and reaches its leaf as asked.
        source.request_keys(wt_runtime::Request::Recheck, ["one"]);
        assert_eq!(git_publisher.requested().await, Some(()));
        let demand = git_publisher.take_demand();
        assert_eq!(demand.whole, None);
        assert_eq!(
            demand.keys.into_iter().collect::<Vec<_>>(),
            [("one".to_owned(), wt_runtime::Request::Recheck)]
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(30), metadata_publisher.requested())
                .await
                .is_err()
        );
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn restored_git_inventory_publishes_a_restored_board_before_any_fetch() {
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let caches = tempfile::tempdir().unwrap();
        fixture.ctx.caches = wt_runtime::CacheStore::new(caches.path().to_path_buf(), "test");
        let facts = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        // A previous process saved its live inventory.
        {
            let scope = TaskScope::new();
            let (_git, publisher) = leaf_channel(&scope, git_spec(&fixture.ctx));
            publisher.publish_live(facts.clone(), Stamp::now());
            let path = caches.path().join("git.json");
            tokio::time::timeout(Duration::from_secs(5), async {
                while !path.exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("git inventory cache written");
            scope.shutdown(Duration::from_secs(2)).await.unwrap();
        }
        // This process has not fetched Git at all; its publisher never runs.
        let scope = TaskScope::new();
        let (git, _git_publisher) = leaf_channel(&scope, git_spec(&fixture.ctx));
        let (metadata, metadata_publisher) = leaf_channel(&scope, metadata_spec());
        metadata_publisher.publish_live(
            (serde_json::json!({"slugs":{}}), BTreeSet::new()),
            Stamp::now(),
        );
        let source = project(
            &scope,
            fixture.ctx.config.clone(),
            git.view(),
            metadata.view(),
        );
        let mut updates = source.subscribe();
        let board = tokio::time::timeout(
            Duration::from_secs(5),
            updates.wait_for(|s| s.data().is_some()),
        )
        .await
        .unwrap()
        .unwrap()
        .clone();
        assert!(
            board.is_restored(),
            "the board says its Git facts are restored"
        );
        assert!(!board.domains()[&Domain::Git].is_live());
        assert!(board.domains()[&Domain::Metadata].is_live());
        assert_eq!(*board.state(), SourceState::Refreshing);
        assert!(
            board
                .data()
                .unwrap()
                .rows
                .iter()
                .any(|row| row.slug == "one")
        );
        assert!(
            git.confirmed().is_none(),
            "restored Git facts never authorize actions"
        );
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn git_inventory_meets_the_persisted_source_contract() {
        let row = |slug: &str, branch: &str, main: bool, dirty: bool| {
            let path = if main {
                "/repo/main".to_owned()
            } else {
                format!("/repo/worktrees/{slug}")
            };
            let git_dir = if main {
                "/repo/main/.git".to_owned()
            } else {
                format!("/repo/main/.git/worktrees/{slug}")
            };
            serde_json::json!({
                "worktree": {
                    "target": {
                        "ref": {"kind": "local", "slug": slug},
                        "slug": slug,
                        "branch": branch,
                        "path": path,
                        "stage": "stage-7692c3ad35",
                        "location": {"kind": "local"}
                    },
                    "isMain": main,
                    "headSha": "a980b25659467cf7e44cb1db69e0118b049ece62",
                    "detached": false,
                    "kind": "linkedWorktree",
                    "locked": false,
                    "prunable": false,
                    "gitDir": git_dir,
                    "commonDir": "/repo/main/.git"
                },
                "status": {
                    "branch": branch,
                    "headSha": "a980b25659467cf7e44cb1db69e0118b049ece62",
                    "upstream": if main { None } else { Some(format!("origin/{branch}")) },
                    "ahead": if main { None } else { Some(2) },
                    "behind": if main { None } else { Some(0) },
                    "trackedChanges": u32::from(dirty),
                    "untrackedFiles": 0,
                    "dirty": dirty
                },
                "error": null
            })
        };
        let facts: Vec<WorktreeSnapshot> = serde_json::from_value(serde_json::json!([
            row("main", "main", true, false),
            row("one", "feature/one", false, true),
            row("two", "feature/two", false, false),
        ]))
        .unwrap();
        wt_runtime::conformance::persisted_source("git", Domain::Git, facts).await;
    }

    fn row_stamp(git: &LeafHandle<Vec<WorktreeSnapshot>>, key: &str) -> Stamp {
        git.snapshot().domains()[&Domain::Git].observed_for(key)
    }

    fn dirty(snapshot: &SourceSnapshot<Vec<WorktreeSnapshot>>, slug: &str) -> Option<bool> {
        snapshot
            .data()?
            .iter()
            .find(|row| row.worktree.target.slug() == slug)?
            .status
            .as_ref()
            .map(|status| status.dirty)
    }

    #[tokio::test]
    async fn the_first_load_is_one_pass_and_a_changed_checkout_is_read_alone() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let local = start(&scope, &fixture.ctx);
        let mut updates = local.git.subscribe();
        // The host and the terminal each ask for the first load.
        local.board.refresh();
        local.board.refresh();
        tokio::time::timeout(
            Duration::from_secs(30),
            updates.wait_for(|snapshot| {
                *snapshot.state() == SourceState::Ready && snapshot.data().is_some()
            }),
        )
        .await
        .expect("first Git load")
        .unwrap();
        let keys = ["main", "one", "two"];
        assert!(
            keys.iter()
                .all(|key| row_stamp(&local.git, key).is_ordered())
        );
        // Watching the checkouts, the first samples of their files and the
        // second request must not read any of them again.
        let loaded = Stamp::now();
        tokio::time::sleep(Duration::from_secs(3)).await;
        for key in keys {
            assert_eq!(
                row_stamp(&local.git, key).at_or_after(&loaded),
                Some(false),
                "{key} was read twice at start"
            );
        }

        let edited = Stamp::now();
        let worktree = fixture.ctx.config.paths.worktree_root.join("one");
        tokio::fs::write(worktree.join("tracked.txt"), "edited\n")
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(20),
            updates.wait_for(|snapshot| dirty(snapshot, "one") == Some(true)),
        )
        .await
        .expect("the edit reaches the Git leaf")
        .unwrap();
        assert_eq!(
            row_stamp(&local.git, "one").at_or_after(&edited),
            Some(true)
        );
        for key in ["main", "two"] {
            assert_eq!(
                row_stamp(&local.git, key).at_or_after(&edited),
                Some(false),
                "only the edited row is read, not {key}"
            );
        }
        // What a safety decision takes from the leaf is as old as its oldest
        // row, however fresh the edited one is.
        let confirmed = local.git.confirmed().unwrap();
        assert_eq!(confirmed.observed().at_or_after(&edited), Some(false));
        assert_eq!(dirty(&local.git.snapshot(), "two"), Some(false));

        // A user-caused read of one row answers for that row alone.
        let asked = Stamp::now();
        local.git.request_keys(wt_runtime::Request::Direct, ["two"]);
        tokio::time::timeout(
            Duration::from_secs(20),
            updates.wait_for(|snapshot| {
                snapshot.domains()[&Domain::Git]
                    .observed_for("two")
                    .at_or_after(&asked)
                    == Some(true)
            }),
        )
        .await
        .expect("the named row is read")
        .unwrap();
        assert_eq!(
            row_stamp(&local.git, "main").at_or_after(&asked),
            Some(false)
        );

        // An explicit refresh still reads everything.
        let swept = Stamp::now();
        local.board.refresh();
        tokio::time::timeout(
            Duration::from_secs(30),
            updates.wait_for(|snapshot| {
                snapshot.domains()[&Domain::Git]
                    .observed()
                    .at_or_after(&swept)
                    == Some(true)
            }),
        )
        .await
        .expect("a refresh sweeps every row")
        .unwrap();
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_created_and_a_removed_checkout_settle_on_the_listing() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let local = start(&scope, &fixture.ctx);
        let mut updates = local.git.subscribe();
        local.git.refresh();
        tokio::time::timeout(
            Duration::from_secs(30),
            updates.wait_for(|snapshot| *snapshot.state() == SourceState::Ready),
        )
        .await
        .expect("first Git load")
        .unwrap();
        let first = row_stamp(&local.git, "one");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&fixture.ctx.config.paths.main_clone)
                .output()
                .unwrap();
            assert!(status.status.success(), "git {args:?}: {status:?}");
        };
        let three = fixture.ctx.config.paths.worktree_root.join("three");
        git(&[
            "worktree",
            "add",
            "-b",
            "feature/three",
            three.to_str().unwrap(),
            "main",
        ]);
        // Creation names no row: the listing finds it.
        let accepted = Stamp::now();
        local
            .git
            .request_keys(wt_runtime::Request::Direct, Vec::<String>::new());
        tokio::time::timeout(
            Duration::from_secs(20),
            updates.wait_for(|snapshot| dirty(snapshot, "three") == Some(false)),
        )
        .await
        .expect("the created checkout is listed and read")
        .unwrap();
        assert_eq!(
            row_stamp(&local.git, "three").at_or_after(&accepted),
            Some(true)
        );

        git(&["worktree", "remove", "--force", three.to_str().unwrap()]);
        let accepted = Stamp::now();
        local
            .git
            .request_keys(wt_runtime::Request::Direct, ["three"]);
        tokio::time::timeout(
            Duration::from_secs(20),
            updates.wait_for(|snapshot| {
                snapshot.data().is_some_and(|rows| rows.len() == 3)
                    && snapshot.domains()[&Domain::Git]
                        .observed_for("three")
                        .at_or_after(&accepted)
                        == Some(true)
            }),
        )
        .await
        .expect("the removed checkout leaves the listing")
        .unwrap();
        // Neither needed the rows nobody touched; they may have been read
        // since for other reasons (the shared ref directory changed), but
        // the domain stamp never claimed more than its oldest row.
        let meta = local.git.snapshot().domains()[&Domain::Git].clone();
        assert_eq!(meta.observed_for("one").at_or_after(&first), Some(true));
        assert_eq!(
            meta.observed_for("one").at_or_after(&meta.observed()),
            Some(true)
        );
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
        fixture.close().await.unwrap();
    }

    /// Measurement, not a check. Run with
    /// `cargo test -p wt-sources timing_of_one_changed_checkout -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "timing measurement over a fixture of 45 checkouts"]
    async fn timing_of_one_changed_checkout_among_many() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let root = fixture.ctx.config.paths.worktree_root.clone();
        let main = fixture.ctx.config.paths.main_clone.clone();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&main)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
        };
        // Enough tracked files that one status costs what it does in a
        // working repository rather than a few milliseconds.
        for directory in 0..40 {
            let path = main.join(format!("src/d{directory:02}"));
            std::fs::create_dir_all(&path).unwrap();
            for file in 0..100 {
                std::fs::write(path.join(format!("f{file:03}.txt")), "content\n").unwrap();
            }
        }
        git(&["add", "src"]);
        git(&["commit", "-q", "-m", "files"]);
        for index in 0..42 {
            let path = root.join(format!("n{index:02}"));
            let output = std::process::Command::new("git")
                .args(["worktree", "add", "-b"])
                .arg(format!("feature/n{index:02}"))
                .arg(&path)
                .arg("main")
                .current_dir(&fixture.ctx.config.paths.main_clone)
                .output()
                .unwrap();
            assert!(output.status.success());
        }
        let median = |mut samples: Vec<Duration>| {
            samples.sort();
            samples[samples.len() / 2]
        };

        // Before: every Git change, action effect and watcher event ran this.
        let mut passes = Vec::new();
        for _ in 0..5 {
            let started = std::time::Instant::now();
            let rows = fixture
                .ctx
                .repository
                .inventory_status(&fixture.ctx.cancellation)
                .await
                .unwrap();
            assert_eq!(rows.len(), 45);
            passes.push(started.elapsed());
        }
        // And published it through a leaf that reads the domain whole.
        let before_scope = TaskScope::new();
        let whole = start_source(
            &before_scope,
            SourceSpec {
                name: "git-whole",
                domain: Domain::Git,
                persistence: Persistence::Never("measurement"),
                // What the Git leaf used before it read row by row.
                policy: RefreshPolicy {
                    debounce: Duration::from_millis(25),
                    minimum_interval: Duration::from_millis(100),
                },
            },
            {
                let repository = fixture.ctx.repository.clone();
                move |cancel| {
                    let repository = repository.clone();
                    async move { repository.inventory_status(&cancel).await }
                }
            },
        );
        let mut whole_updates = whole.subscribe();
        whole.refresh();
        whole_updates
            .wait_for(|snapshot| *snapshot.state() == SourceState::Ready)
            .await
            .unwrap();
        let mut whole_changes = Vec::new();
        for round in 0..5 {
            tokio::fs::write(root.join("n07/tracked.txt"), format!("whole {round}\n"))
                .await
                .unwrap();
            let asked = Stamp::now();
            let started = std::time::Instant::now();
            whole.refresh();
            whole_updates
                .wait_for(|snapshot| {
                    snapshot.domains()[&Domain::Git]
                        .observed()
                        .at_or_after(&asked)
                        == Some(true)
                })
                .await
                .unwrap();
            whole_changes.push(started.elapsed());
        }
        before_scope.shutdown(Duration::from_secs(5)).await.unwrap();

        // After.
        let scope = TaskScope::new();
        let started = std::time::Instant::now();
        let local = start(&scope, &fixture.ctx);
        let mut updates = local.git.subscribe();
        local.board.refresh();
        updates
            .wait_for(|snapshot| {
                *snapshot.state() == SourceState::Ready && snapshot.data().is_some()
            })
            .await
            .unwrap();
        let first_load = started.elapsed();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let mut named = Vec::new();
        for round in 0..5 {
            tokio::fs::write(root.join("n07/tracked.txt"), format!("named {round}\n"))
                .await
                .unwrap();
            let asked = Stamp::now();
            let started = std::time::Instant::now();
            local.git.request_keys(wt_runtime::Request::Direct, ["n07"]);
            updates
                .wait_for(|snapshot| {
                    snapshot.domains()[&Domain::Git]
                        .observed_for("n07")
                        .at_or_after(&asked)
                        == Some(true)
                })
                .await
                .unwrap();
            named.push(started.elapsed());
            tokio::time::sleep(Duration::from_millis(1370)).await;
        }
        // A file change with nobody asking: found by the watcher, then read.
        let mut watched = Vec::new();
        for round in 0..5 {
            let committed = round % 2 == 1;
            let content = if committed {
                "base\n".to_owned()
            } else {
                format!("watched {round}\n")
            };
            let started = std::time::Instant::now();
            tokio::fs::write(root.join("n11/tracked.txt"), content)
                .await
                .unwrap();
            updates
                .wait_for(|snapshot| dirty(snapshot, "n11") == Some(!committed))
                .await
                .unwrap();
            watched.push(started.elapsed());
            tokio::time::sleep(Duration::from_millis(1370)).await;
        }
        // A user-caused read while a sweep of the fleet is in flight.
        let mut during_sweep = Vec::new();
        for _ in 0..3 {
            local.git.refresh();
            tokio::time::sleep(Duration::from_millis(60)).await;
            let asked = Stamp::now();
            let started = std::time::Instant::now();
            local.git.request_keys(wt_runtime::Request::Direct, ["n41"]);
            updates
                .wait_for(|snapshot| {
                    snapshot.domains()[&Domain::Git]
                        .observed_for("n41")
                        .at_or_after(&asked)
                        == Some(true)
                })
                .await
                .unwrap();
            during_sweep.push(started.elapsed());
            updates
                .wait_for(|snapshot| *snapshot.state() == SourceState::Ready)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        println!("checkouts: 45");
        println!(
            "before  full pass (inventory_status):        {:?}",
            median(passes)
        );
        println!(
            "before  refresh to published (whole leaf):   {:?}",
            median(whole_changes)
        );
        println!("after   first load, one pass:                {first_load:?}");
        println!(
            "after   named row, request to published:     {:?}",
            median(named)
        );
        println!(
            "after   named row during a sweep:            {:?}",
            median(during_sweep)
        );
        println!(
            "after   file change to published (watcher):  {:?}",
            median(watched)
        );
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn rows_a_git_backed_action_names_are_keys_the_git_leaf_publishes() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let local = start(&scope, &fixture.ctx);
        local.board.refresh();
        let mut boards = local.board.subscribe();
        let board = tokio::time::timeout(
            Duration::from_secs(30),
            boards.wait_for(|snapshot| {
                *snapshot.state() == SourceState::Ready && snapshot.data().is_some()
            }),
        )
        .await
        .expect("first board")
        .unwrap()
        .data()
        .unwrap()
        .clone();
        let meta = local.git.snapshot().domains()[&Domain::Git].clone();
        let published: BTreeSet<String> = meta
            .keyed()
            .expect("the Git leaf stamps its rows")
            .keys()
            .map(str::to_owned)
            .collect();
        assert_eq!(board.rows.len(), 2, "one row per linked checkout");
        // A change settles on the stamp of the keys its effect names, and
        // its host re-reads those keys. A key from another key space would
        // read as "absent since the last listing" and settle on nothing.
        for row in &board.rows {
            let key = row.key.clone();
            let actions = [
                wt_tui::UiAction::Remove {
                    key: key.clone(),
                    force: false,
                    revision: None,
                    seen: None,
                },
                wt_tui::UiAction::Restack { key: key.clone() },
                wt_tui::UiAction::CleanupTargets {
                    targets: vec![wt_tui::CleanupTarget {
                        key: key.clone(),
                        path: row.path.clone(),
                        branch: String::new(),
                        head: String::new(),
                    }],
                },
                wt_tui::UiAction::Cleanup {
                    revisions: vec![wt_tui::RemovalRevision {
                        key: key.clone(),
                        path: row.path.clone(),
                        branch: String::new(),
                        head: String::new(),
                        digest: String::new(),
                        hazards: Vec::new(),
                        published_base: None,
                    }],
                },
            ];
            for action in actions {
                let rows = action
                    .effect(&board)
                    .rows()
                    .unwrap_or_else(|| panic!("{action:?} names no row"));
                assert_eq!(rows, std::slice::from_ref(&key), "{action:?}");
                assert!(
                    published.contains(&key),
                    "{key} is not a key the Git leaf publishes: {published:?}"
                );
            }
        }
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
        fixture.close().await.unwrap();
    }

    /// The metadata leaf reads beside the write queue. The read that confirms
    /// a write still shows it: the host stamps an accepted write after its
    /// call returned, so every fetch that starts at or after that stamp must
    /// show the write, with no fetch in between showing less. And a refresh
    /// no longer waits for a write that is stuck.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_metadata_read_shows_every_returned_write_and_passes_a_stuck_one() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let local = start(&scope, &fixture.ctx);
        local.metadata.refresh();
        let mut updates = local.metadata.subscribe();
        updates
            .wait_for(|snapshot| *snapshot.state() == SourceState::Ready)
            .await
            .unwrap();
        let fresh = |snapshot: &SourceSnapshot<Metadata>, since: &Stamp| {
            snapshot.domains()[&Domain::Metadata]
                .observed()
                .at_or_after(since)
                == Some(true)
        };
        for write in 1..=40u64 {
            fixture
                .ctx
                .database
                .call(move |store| Ok(store.set_attention_seen(write)?))
                .await
                .unwrap();
            let accepted = Stamp::now();
            local.metadata.refresh();
            let confirming = updates
                .wait_for(|snapshot| fresh(snapshot, &accepted))
                .await
                .unwrap()
                .clone();
            assert_eq!(
                confirming.data().unwrap().0["attentionSeenTs"],
                write,
                "a read that started after the write returned did not show it"
            );
        }

        // A writer held up, as by another process's lock.
        let (entered, held) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
        );
        let stuck = tokio::spawn({
            let (database, entered, held) =
                (fixture.ctx.database.clone(), entered.clone(), held.clone());
            async move {
                database
                    .call(move |_| {
                        entered.notify_one();
                        while held.load(std::sync::atomic::Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Ok(())
                    })
                    .await
            }
        });
        entered.notified().await;
        let asked = Stamp::now();
        local.metadata.refresh();
        let read = tokio::time::timeout(
            Duration::from_secs(5),
            updates.wait_for(|snapshot| fresh(snapshot, &asked)),
        )
        .await
        .map(|read| read.unwrap().data().unwrap().0["attentionSeenTs"].clone());
        held.store(false, std::sync::atomic::Ordering::SeqCst);
        stuck.await.unwrap().unwrap();
        assert_eq!(
            read.expect("the read waited behind a stuck write"),
            40,
            "a read beside the write queue shows what was written before it"
        );
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
        fixture.close().await.unwrap();
    }

    /// Measurement, not a check. Run with
    /// `cargo test -p wt-sources timing_of_metadata_reads -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "timing measurement of metadata reads under status traffic"]
    async fn timing_of_metadata_reads_under_status_traffic() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let local = start(&scope, &fixture.ctx);
        local.board.refresh();
        let mut updates = local.metadata.subscribe();
        updates
            .wait_for(|snapshot| *snapshot.state() == SourceState::Ready)
            .await
            .unwrap();
        // What the tick alone asks for: the read rate before pacing, since
        // the leaf's own floor (50 ms) is under the tick's period.
        let signals = Arc::new(AtomicUsize::new(0));
        let database = fixture.ctx.config.paths.state_db.clone();
        let mut wal = database.clone().into_os_string();
        wal.push("-wal");
        freshness::watch_signal_files(&scope, vec![database, wal.into()], {
            let signals = signals.clone();
            move || {
                signals.fetch_add(1, Ordering::SeqCst);
            }
        });
        let reads = Arc::new(AtomicUsize::new(0));
        let counting = tokio::spawn({
            let reads = reads.clone();
            let mut updates = local.metadata.subscribe();
            async move {
                let mut last = updates.borrow_and_update().domains()[&Domain::Metadata].observed();
                while updates.changed().await.is_ok() {
                    let observed =
                        updates.borrow_and_update().domains()[&Domain::Metadata].observed();
                    if observed != last {
                        last = observed;
                        reads.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        signals.store(0, Ordering::SeqCst);
        reads.store(0, Ordering::SeqCst);
        // Forty agents writing status: a write every 25 ms for five seconds.
        let seconds = 5;
        for write in 0..(seconds * 40) {
            fixture
                .ctx
                .database
                .call(move |store| Ok(store.set_attention_seen(write)?))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        let (signalled, read) = (signals.load(Ordering::SeqCst), reads.load(Ordering::SeqCst));
        // A lone write after a quiet spell.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mut lone = Vec::new();
        for write in 0..5u64 {
            let written = Stamp::now();
            let started = std::time::Instant::now();
            fixture
                .ctx
                .database
                .call(move |store| Ok(store.set_attention_seen(10_000 + write)?))
                .await
                .unwrap();
            updates
                .wait_for(|snapshot| {
                    snapshot.domains()[&Domain::Metadata]
                        .observed()
                        .at_or_after(&written)
                        == Some(true)
                })
                .await
                .unwrap();
            lone.push(started.elapsed());
            tokio::time::sleep(Duration::from_millis(730)).await;
        }
        lone.sort();
        println!("writes: 40/s for {seconds}s");
        println!(
            "before  metadata reads (one per tick signal): {:.1}/s",
            signalled as f64 / seconds as f64
        );
        println!(
            "after   metadata reads:                       {:.1}/s",
            read as f64 / seconds as f64
        );
        println!(
            "after   lone write to read started, median:   {:?}",
            lone[lone.len() / 2]
        );
        counting.abort();
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
        fixture.close().await.unwrap();
    }
}
