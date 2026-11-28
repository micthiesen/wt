//! The host's side of the action palettes. The board lists the entries
//! (`wt_tui` builds every palette from it); a commit is checked here against
//! this host's own state before anything is dispatched.
use anyhow::{Result, bail};
use wt_config::ActionDef;
use wt_github::GithubData;
use wt_runtime::Confirmed;
use wt_tui::{ActionSeen, ActionSurface, Board, Seen, UiAction, UiModal, UiReply};

use wt_app_core::context::AppContext;
use wt_ops::action_dispatch::{self, PreparedContext};

pub const CUSTOM_ID: &str = wt_app_core::action_builtins::CUSTOM_ID;

pub async fn execute(
    ctx: &AppContext,
    request: UiAction,
    board: Option<&Board>,
    github: Option<&Confirmed<GithubData>>,
) -> Result<UiReply> {
    match request {
        UiAction::PrepareDevLogs { key } => dev_logs(ctx, key, board, github).await,
        UiAction::RunAction {
            surface,
            id,
            arg,
            extras,
            seen,
        } => {
            let commit = Commit {
                surface,
                id,
                arg,
                extras,
                seen,
            };
            run(ctx, commit, board, github).await
        }
        _ => bail!("unexpected request at the action palette boundary"),
    }
}

/// A palette commit as it arrived.
struct Commit {
    surface: ActionSurface,
    id: String,
    arg: Option<String>,
    extras: String,
    seen: Option<Seen<ActionSeen>>,
}

/// A commit that passed [`check`], with the context it was checked against.
struct Checked {
    definition: ActionDef,
    prepared: PreparedContext,
}

/// Everything a commit must pass before it dispatches. The palette offered
/// the entry from the board; this reads the host's state once and refuses
/// with a stale reply, which reopens the palette, when the entry is gone,
/// names another checkout or pull request than the one shown, or is not
/// available now. Availability is [`wt_tui::blocker`], the rule the palette
/// listed the entry by, over the facts of this host's own read.
async fn check(
    ctx: &AppContext,
    commit: &Commit,
    board: Option<&Board>,
    github: Option<&Confirmed<GithubData>>,
) -> Result<Checked> {
    if let ActionSurface::Slot { target } = &commit.surface
        && let Some(reason) = wt_sources::action_source::slot_unavailable(&ctx.config, *target)
    {
        return Err(wt_app_core::host_errors::stale(format!(
            "{} slot unavailable: {reason}",
            wt_tui::slot_name(*target)
        )));
    }
    let definition = action_dispatch::definitions(ctx, &commit.surface)
        .into_iter()
        .find(|definition| definition.id == commit.id)
        .ok_or_else(|| {
            wt_app_core::host_errors::stale(format!(
                "action {:?} is no longer available",
                commit.id
            ))
        })?;
    if commit.id == CUSTOM_ID && commit.extras.trim().is_empty() {
        bail!("custom prompt is empty");
    }
    if definition.arg_prompt.is_some()
        && commit
            .arg
            .as_deref()
            .is_none_or(|arg| arg.trim().is_empty())
    {
        bail!("{} requires a value", definition.name);
    }
    let prepared =
        action_dispatch::prepare_dispatch(ctx, &commit.surface, github, board, commit.arg.clone())
            .await?;
    let context = &prepared.context;
    if let Some(Seen(seen)) = &commit.seen {
        if let Some(row) = &seen.row
            && (context.worktree_ref.is_none()
                || row.path != wt_core::sanitize_terminal_text(&context.cwd.to_string_lossy())
                || row.branch != wt_core::sanitize_terminal_text(&context.branch))
        {
            return Err(wt_app_core::host_errors::stale(format!(
                "{} is not the checkout the palette showed",
                context.slug
            )));
        }
        let needs_pr = definition.requires.iter().any(|requirement| {
            matches!(
                requirement,
                wt_config::RequireTag::Pr | wt_config::RequireTag::PrReady
            )
        });
        // The run names a pull request: only the one the person was shown.
        if needs_pr && context.worktree_ref.is_some() {
            let Some(number) = seen.pr else {
                return Err(refused(&definition, "the palette showed no pull request"));
            };
            if context
                .row
                .pr_number
                .as_deref()
                .is_some_and(|current| current != number.to_string())
            {
                return Err(refused(
                    &definition,
                    "its pull request changed since the palette opened",
                ));
            }
        }
    }
    let entry = wt_sources::inventory::action_entry(&definition);
    let facts = wt_sources::action_source::action_facts(&context.row);
    if let Some(blocker) = wt_tui::blocker(&entry, &commit.surface, &facts) {
        return Err(refused(&definition, blocker.reason()));
    }
    Ok(Checked {
        definition,
        prepared,
    })
}

async fn run(
    ctx: &AppContext,
    commit: Commit,
    board: Option<&Board>,
    github: Option<&Confirmed<GithubData>>,
) -> Result<UiReply> {
    let Checked {
        definition,
        prepared,
    } = check(ctx, &commit, board, github).await?;
    if let Some(arg) = &commit.arg {
        wt_actions::record_value(
            &ctx.config.paths.cache_root.join("action-history.json"),
            &commit.id,
            arg,
            None,
            (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64,
            &ctx.cancellation,
        )
        .await?;
    }
    let message = action_dispatch::dispatch_prepared(
        ctx,
        &commit.surface,
        &definition,
        prepared,
        &commit.extras,
        Vec::new(),
    )
    .await
    .map_err(|error| {
        // A run started after the palette was built; the palette would
        // have offered to stop it instead.
        let started = error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<wt_actions::ActionServiceError>(),
                Some(wt_actions::ActionServiceError::AlreadyRunning(_))
            )
        });
        if started {
            wt_app_core::host_errors::stale(format!("{} is already running", definition.name))
        } else {
            error
        }
    })?;
    Ok(UiReply {
        message,
        ..Default::default()
    })
}

/// A palette commit the host re-checked and refused. The palette the user
/// chose from no longer matches the host's state, so the stale reply reopens
/// it from the current board.
fn refused(def: &ActionDef, reason: &str) -> anyhow::Error {
    wt_app_core::host_errors::stale(format!("{}: {reason}", def.name))
}

/// Most dev-server output kept in the `! l` overlay.
const DEV_LOG_LINES: u32 = 500;

async fn dev_logs(
    ctx: &AppContext,
    key: String,
    board: Option<&Board>,
    github: Option<&Confirmed<GithubData>>,
) -> Result<UiReply> {
    // The overlay asks again every second. A row this host's board lists
    // names its dev server without another worktree listing.
    let listed = board
        .and_then(|board| board.rows.iter().find(|row| row.key == key))
        .filter(|row| row.host.is_none() && row.removal.is_none())
        .map(|row| row.slug.clone());
    let slug = match listed {
        Some(slug) => slug,
        None => {
            let surface = ActionSurface::Row { key: key.clone() };
            action_dispatch::prepare(ctx, &surface, github, board, None)
                .await?
                .slug
        }
    };
    let text = wt_app_core::dev::service(ctx)?
        .logs(&slug, DEV_LOG_LINES, &ctx.cancellation)
        .await?
        .ok_or_else(|| wt_app_core::host_errors::notice("dev server is not running"))?;
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let skip = lines.len().saturating_sub(DEV_LOG_LINES as usize);
    Ok(UiReply {
        modal: Some(UiModal::Log {
            title: format!("{slug} dev logs"),
            lines: lines.into_iter().skip(skip).collect(),
            close_key: Some('l'),
            refresh: Some(Box::new(UiAction::PrepareDevLogs { key })),
        }),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_actions::{ActionPr, ActionPrObservation, ActionRow};
    use wt_config::{ActionKind, RequireTag};
    use wt_tui::{ActionRowSeen, BoardRow, LocalMenu, Menu, PrPresentation};

    use wt_app_core::test_support::CommandFixture;

    fn commit(surface: ActionSurface, id: &str) -> Commit {
        Commit {
            surface,
            id: id.into(),
            arg: None,
            extras: String::new(),
            seen: None,
        }
    }

    /// The board key and checkout path of a fixture worktree.
    async fn row_key(fixture: &CommandFixture, slug: &str) -> (String, String) {
        let record = fixture
            .ctx
            .repository
            .inventory(&fixture.ctx.cancellation)
            .await
            .unwrap()
            .into_iter()
            .find(|record| record.target.slug() == slug)
            .unwrap();
        (
            wt_core::worktree_target_key(&record.target),
            record.target.path.clone(),
        )
    }

    fn pull_request(branch: &str, number: u64) -> wt_github::PullRequest {
        serde_json::from_value(serde_json::json!({
            "number": number, "url": "https://example.test/pr", "headRefName": branch,
            "baseRefName": "main", "mergeCommitOid": null, "title": "t", "isDraft": false,
            "state": "OPEN", "mergeable": null, "mergeStateStatus": null, "checks": "pending",
            "failedChecks": [], "review": "none", "reviewRequests": 0,
            "requestedReviewers": [], "suggestedReviewers": [], "autoMerge": null,
            "comments": [], "unresolvedThreads": 0, "unresolvedThreadsTotal": 0,
            "mergedAt": null, "closedAt": null,
        }))
        .unwrap()
    }

    fn confirmed(branch: &str, pr: Option<u64>) -> Confirmed<GithubData> {
        let mut data = GithubData::default();
        data.covered_branches.insert(branch.into());
        if let Some(number) = pr {
            data.prs.insert(branch.into(), pull_request(branch, number));
        }
        Confirmed::for_test(data)
    }

    fn stale(error: &anyhow::Error) -> bool {
        wt_app_core::host_errors::error_reply(error).stale
    }

    /// The palette lists an entry by `wt_tui::blocker` over the board's
    /// facts; dispatch refuses by `wt_actions::evaluate_requirements` over
    /// its row. Every requirement, alone and in every pair order, must read
    /// the same over every kind of row, or a palette would offer what a
    /// dispatch refuses.
    #[test]
    fn the_palettes_rule_and_dispatchs_rule_agree_on_every_requirement() {
        let tags = [
            RequireTag::Pr,
            RequireTag::PrReady,
            RequireTag::Deployed,
            RequireTag::IssueTracker,
        ];
        let mut requirement_sets: Vec<Vec<RequireTag>> = vec![Vec::new()];
        requirement_sets.extend(tags.iter().map(|tag| vec![*tag]));
        for first in tags {
            for second in tags {
                requirement_sets.push(vec![first, second]);
            }
        }
        let pr = |state: &str, is_draft: bool| {
            ActionPrObservation::Present(ActionPr {
                state: state.into(),
                is_draft,
            })
        };
        let observations = [
            ActionPrObservation::Unknown,
            ActionPrObservation::ConfirmedAbsent,
            pr("OPEN", false),
            pr("OPEN", true),
            pr("MERGED", false),
            pr("CLOSED", true),
        ];
        // (slug, stored issue id, tracker prefix)
        let issues = [
            ("plain", None, None),
            ("eng-12-fix", None, None),
            ("eng-12-fix", Some(""), None),
            ("plain", Some("ENG-7"), None),
            ("plain", Some("ENG-7"), Some("OPS")),
            ("plain", Some("GH-12"), None),
            ("plain", Some(" eng-7 "), None),
        ];
        let mut cases = 0;
        for requires in &requirement_sets {
            let definition = ActionDef {
                id: "probe".into(),
                name: "Probe".into(),
                requires: requires.clone(),
                ..Default::default()
            };
            let entry = wt_sources::inventory::action_entry(&definition);
            for observation in &observations {
                for deployed in [false, true] {
                    for (slug, issue_id, prefix) in issues {
                        let row = ActionRow {
                            slug: slug.into(),
                            issue_id: issue_id.map(str::to_owned),
                            issue_prefix: prefix.map(str::to_owned),
                            pr: observation.clone(),
                            pr_number: None,
                            deployed,
                        };
                        let facts = wt_sources::action_source::action_facts(&row);
                        let surface = ActionSurface::Row { key: slug.into() };
                        assert_eq!(
                            wt_tui::blocker(&entry, &surface, &facts)
                                .map(|blocker| blocker.reason().to_owned()),
                            wt_actions::evaluate_requirements(requires, &row),
                            "{requires:?} over {row:?}"
                        );
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 21 * 6 * 2 * 7);
        // Only the manager palette without a row adds a rule of its own.
        let definition = |id: &str| {
            wt_sources::inventory::action_entry(&ActionDef {
                id: id.into(),
                ..Default::default()
            })
        };
        let no_row = ActionSurface::Manager { key: None };
        let facts = wt_tui::ActionFacts::default();
        for (id, blocked) in [
            ("manager-ask-row", true),
            ("manager-digest", false),
            (CUSTOM_ID, false),
        ] {
            assert_eq!(
                wt_tui::blocker(&definition(id), &no_row, &facts).map(|b| b.reason()),
                blocked.then_some("no row selected"),
                "{id}"
            );
            assert_eq!(
                wt_tui::blocker(
                    &definition(id),
                    &ActionSurface::Manager {
                        key: Some("one".into())
                    },
                    &facts
                ),
                None
            );
        }
    }

    #[tokio::test]
    async fn a_commit_for_an_unavailable_slot_is_refused_as_stale() {
        let fixture = CommandFixture::new().await.unwrap();
        assert!(fixture.ctx.config.paths.wt_source.is_none());
        let surface = ActionSurface::Slot {
            target: wt_tui::SessionTarget::WtSource,
        };
        let error = check(&fixture.ctx, &commit(surface, "slot-continue"), None, None)
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "wt slot unavailable: paths.wt_source not set"
        );
        assert!(stale(&error));
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_pr_action_is_refused_until_github_confirms_the_branch() {
        let mut fixture = CommandFixture::new().await.unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config).actions = vec![ActionDef {
            id: "fix-ci".into(),
            name: "Fix CI and watch until green".into(),
            prompt: Some("Fix CI for PR {{pr}}".into()),
            requires: vec![RequireTag::Pr],
            ..Default::default()
        }];
        let (key, _) = row_key(&fixture, "one").await;
        // The restart race: the restored board still shows PR 2490 while no
        // live GitHub read exists. The board is display data; the commit is
        // checked against the confirmed read, and there is none.
        let board = Board {
            rows: vec![BoardRow {
                key: key.clone(),
                slug: "one".into(),
                branch: "feature/one".into(),
                pr: Some(PrPresentation {
                    number: Some(2490),
                    state: Some("OPEN".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let surface = ActionSurface::Row { key };
        let request = commit(surface.clone(), "fix-ci");
        let error = check(&fixture.ctx, &request, Some(&board), None)
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "Fix CI and watch until green: GitHub data is not confirmed"
        );
        assert!(stale(&error));
        // A read that covers the branch and finds no PR refuses too.
        let error = check(
            &fixture.ctx,
            &request,
            Some(&board),
            Some(&confirmed("feature/one", None)),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "Fix CI and watch until green: no PR");
        assert!(stale(&error));
        // With the PR confirmed the commit passes, with its context resolved.
        let github = confirmed("feature/one", Some(2490));
        let checked = check(&fixture.ctx, &request, Some(&board), Some(&github))
            .await
            .unwrap();
        assert_eq!(checked.definition.id, "fix-ci");
        assert_eq!(
            checked.prepared.context.row.pr_number.as_deref(),
            Some("2490")
        );
        // An action that left the catalog reopens the palette as well.
        let error = check(&fixture.ctx, &commit(surface, "retired"), None, None)
            .await
            .err()
            .unwrap();
        assert!(stale(&error), "{error:#}");
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_commit_is_refused_when_it_was_shown_another_checkout_or_pull_request() {
        let mut fixture = CommandFixture::new().await.unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config).actions = vec![ActionDef {
            id: "fix-ci".into(),
            name: "Fix CI".into(),
            prompt: Some("Fix CI for PR {{pr}}".into()),
            requires: vec![RequireTag::Pr],
            ..Default::default()
        }];
        let (key, path) = row_key(&fixture, "one").await;
        let github = confirmed("feature/one", Some(7));
        let shown = |path: &str, branch: &str, pr: Option<u64>| Commit {
            seen: Some(Seen(ActionSeen {
                row: Some(ActionRowSeen {
                    path: path.into(),
                    branch: branch.into(),
                }),
                pr,
            })),
            ..commit(ActionSurface::Row { key: key.clone() }, "fix-ci")
        };
        let path = path.as_str();
        // What the board shows for this checkout passes.
        check(
            &fixture.ctx,
            &shown(path, "feature/one", Some(7)),
            None,
            Some(&github),
        )
        .await
        .unwrap();
        for (request, message) in [
            (
                shown("/elsewhere/one", "feature/one", Some(7)),
                "one is not the checkout the palette showed",
            ),
            (
                shown(path, "feature/other", Some(7)),
                "one is not the checkout the palette showed",
            ),
            (
                shown(path, "feature/one", Some(6)),
                "Fix CI: its pull request changed since the palette opened",
            ),
            // A view that named no pull request cannot start a run that names one.
            (
                shown(path, "feature/one", None),
                "Fix CI: the palette showed no pull request",
            ),
        ] {
            let error = check(&fixture.ctx, &request, None, Some(&github))
                .await
                .err()
                .unwrap();
            assert_eq!(error.to_string(), message);
            assert!(stale(&error));
        }
        // A commit that names no view (an internal send) asserts nothing.
        check(
            &fixture.ctx,
            &commit(ActionSurface::Row { key }, "fix-ci"),
            None,
            Some(&github),
        )
        .await
        .unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_commit_needs_its_prompt_and_value_before_anything_is_read() {
        let mut fixture = CommandFixture::new().await.unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config).actions = vec![ActionDef {
            id: "deploy".into(),
            name: "Deploy".into(),
            kind: ActionKind::Shell,
            shell: Some("true {{arg}}".into()),
            arg_prompt: Some(wt_config::ActionArgPrompt {
                label: "Stage".into(),
            }),
            ..Default::default()
        }];
        let (key, _) = row_key(&fixture, "one").await;
        let surface = ActionSurface::Row { key };
        let error = check(&fixture.ctx, &commit(surface.clone(), "deploy"), None, None)
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "Deploy requires a value");
        let error = check(
            &fixture.ctx,
            &commit(surface.clone(), CUSTOM_ID),
            None,
            None,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "custom prompt is empty");
        let with_value = Commit {
            arg: Some("staging".into()),
            ..commit(surface, "deploy")
        };
        let checked = check(&fixture.ctx, &with_value, None, None).await.unwrap();
        assert_eq!(checked.prepared.context.arg.as_deref(), Some("staging"));
        fixture.close().await.unwrap();
    }

    /// The board the sources publish is enough to build every palette, and
    /// what it offers passes the host's check: the catalog, each row's facts
    /// (here a deployed stage), the recorded argument values and the slots
    /// that cannot run anything. Also the measured cost of a commit's check.
    #[tokio::test]
    async fn the_published_board_builds_the_palettes_its_host_accepts() {
        let mut fixture = CommandFixture::new().await.unwrap();
        {
            let config = std::sync::Arc::make_mut(&mut fixture.ctx.config);
            config.tmux.socket = format!(
                "wt-palette-{}",
                fixture._root.path().file_name().unwrap().to_string_lossy()
            );
            config.actions = vec![
                ActionDef {
                    id: "teardown".into(),
                    name: "Tear down stage".into(),
                    kind: ActionKind::Shell,
                    shell: Some("true".into()),
                    requires: vec![RequireTag::Deployed],
                    ..Default::default()
                },
                ActionDef {
                    id: "deploy".into(),
                    name: "Deploy".into(),
                    kind: ActionKind::Shell,
                    shell: Some("true {{arg}}".into()),
                    arg_prompt: Some(wt_config::ActionArgPrompt {
                        label: "Stage".into(),
                    }),
                    ..Default::default()
                },
            ];
        }
        let (key, path) = row_key(&fixture, "one").await;
        let sst = std::path::Path::new(&path).join(".sst");
        let stage = format!("{}one", fixture.ctx.config.stage.prefix);
        std::fs::create_dir_all(&sst).unwrap();
        std::fs::write(sst.join("stage"), &stage).unwrap();
        std::fs::write(
            sst.join("outputs.json"),
            serde_json::json!({ "stage": stage }).to_string(),
        )
        .unwrap();
        wt_actions::record_value(
            &fixture
                .ctx
                .config
                .paths
                .cache_root
                .join("action-history.json"),
            "deploy",
            "staging",
            Some("Staging"),
            1,
            &fixture.ctx.cancellation,
        )
        .await
        .unwrap();

        let scope = wt_runtime::TaskScope::new();
        let host = crate::host_service::HostService::start(
            &scope,
            fixture.ctx.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let mut updates = host.sources.board.subscribe();
        let board = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                let ready = updates.borrow_and_update().data().cloned().filter(|board| {
                    board
                        .rows
                        .iter()
                        .any(|row| row.key == key && row.action_facts.deployed)
                        && board.actions.recent_args.contains_key("deploy")
                });
                if let Some(board) = ready {
                    return board;
                }
                updates.changed().await.unwrap();
            }
        })
        .await
        .expect("the board publishes the palette facts");
        let board: &Board = &board;

        let catalog = &board.display.actions;
        assert!(
            catalog
                .entry(&ActionSurface::Row { key: key.clone() }, "teardown")
                .is_some()
        );
        assert!(
            catalog
                .entry(&ActionSurface::Manager { key: None }, "manager-digest")
                .is_some_and(|entry| entry.fleet)
        );
        assert!(
            catalog
                .entry(
                    &ActionSurface::Slot {
                        target: wt_tui::SessionTarget::Main
                    },
                    "slot-compact"
                )
                .is_some_and(|entry| entry.direct)
        );
        assert_eq!(
            board.actions.recent_args["deploy"],
            [wt_tui::ArgValue {
                value: "staging".into(),
                shown: "staging".into(),
                label: Some("Staging".into()),
            }]
        );
        assert_eq!(
            board.actions.slots_blocked.get("wt").map(String::as_str),
            Some("paths.wt_source not set")
        );
        let other = board.rows.iter().find(|row| row.slug == "two").unwrap();
        assert!(!other.action_facts.deployed);

        // The palette opens from that board alone.
        let surface = ActionSurface::Row { key: key.clone() };
        let started = std::time::Instant::now();
        let menu = UiAction::PrepareActions {
            surface: surface.clone(),
        }
        .menu(board);
        let built = started.elapsed();
        let Menu::Local(LocalMenu::Modal { modal, .. }) = menu else {
            panic!("the palette was not built from the board")
        };
        let UiModal::Picker { options, .. } = *modal else {
            panic!("expected the action list")
        };
        let detail = |id: &str| {
            options
                .iter()
                .find(|option| option.value.as_deref() == Some(id))
                .and_then(|option| option.detail.clone())
                .unwrap()
        };
        assert_eq!(detail("teardown"), "$ teardown");
        // What it offers passes the host's check, with the witness the
        // board row gives; the other row's entry is refused the same way
        // the palette lists it.
        let row = board.rows.iter().find(|row| row.key == key).unwrap();
        let request = Commit {
            seen: Some(Seen(ActionSeen {
                row: Some(ActionRowSeen {
                    path: row.path.clone(),
                    branch: row.branch.clone(),
                }),
                pr: None,
            })),
            ..commit(surface, "teardown")
        };
        let github = host.sources.github.confirmed();
        let mut samples = Vec::new();
        for _ in 0..15 {
            let started = std::time::Instant::now();
            check(&fixture.ctx, &request, Some(board), github.as_ref())
                .await
                .unwrap();
            samples.push(started.elapsed());
        }
        samples.sort();
        eprintln!(
            "palette open from the board: {built:?}; commit check: p50 {:?} max {:?}",
            samples[7], samples[14]
        );
        let refused = check(
            &fixture.ctx,
            &commit(
                ActionSurface::Row {
                    key: other.key.clone(),
                },
                "teardown",
            ),
            Some(board),
            github.as_ref(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(refused.to_string(), "Tear down stage: no stage deployed");
        assert!(stale(&refused));
        // The palette is a board menu: a host is never asked for it.
        let error = host
            .execute(UiAction::PrepareActions {
                surface: ActionSurface::Manager { key: None },
            })
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("built from the board"),
            "{error:#}"
        );
        scope
            .shutdown(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        fixture.close().await.unwrap();
    }
}
