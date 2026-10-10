use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::{Args, FromArgMatches};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wt_lifecycle::CreateOptions;
use wt_platform::lock::FileLock;
use wt_tui::{
    Board, ConfirmAction, PickerAction, PickerOption, RemovalRevision as UiRemovalRevision,
    UiAction, UiModal, UiReply, UrlKind,
};

use crate::{
    commands::{new::NewArgs, resolve::run_git},
    context::AppContext,
    lifecycle_ops::{self, resolve_key},
};

fn harness_name(harness: wt_core::HarnessId) -> &'static str {
    match harness {
        wt_core::HarnessId::Claude => "Claude",
        wt_core::HarnessId::Codex => "Codex",
        wt_core::HarnessId::Opencode => "OpenCode",
    }
}

fn message(text: impl Into<String>) -> UiReply {
    UiReply {
        message: text.into(),
        ..Default::default()
    }
}

fn modal(modal: UiModal) -> UiReply {
    UiReply {
        modal: Some(modal),
        ..Default::default()
    }
}

pub async fn execute(
    ctx: &AppContext,
    action: UiAction,
    board: Option<&Board>,
    github: &wt_github::GithubData,
) -> Result<UiReply> {
    match action {
        UiAction::PrepareHardRefresh
        | UiAction::HardRefresh
        | UiAction::PrepareReviewCheckout { .. }
        | UiAction::ReviewCheckout { .. }
        | UiAction::DismissReviewRequest { .. }
        | UiAction::PrepareReviewers { .. }
        | UiAction::SubmitReviewers { .. }
        | UiAction::SetPerf { .. }
        | UiAction::SetHistoryActive { .. }
        | UiAction::SetAttentionSeen { .. }
        | UiAction::Restack { .. }
        | UiAction::PrepareRestoreRemoved { .. }
        | UiAction::RestoreRemoved { .. }
        | UiAction::ToggleRemovedAutomations { .. } => {
            bail!("request must run through its host service")
        }
        UiAction::OpenLink { url } => {
            crate::editor::open_url(ctx, &url).await?;
            Ok(message("Opened link"))
        }
        UiAction::OpenPrLink { url, linear } => {
            let target = if linear {
                wt_config::PullRequestTarget::Linear
            } else {
                wt_config::PullRequestTarget::Github
            };
            crate::editor::open_url(ctx, &wt_github::pull_request_open_url(&url, target)).await?;
            Ok(message("Opened pull request"))
        }
        UiAction::OpenPrDefault { url } => {
            crate::editor::open_url(
                ctx,
                &wt_github::pull_request_open_url(&url, ctx.config.github.pr_target),
            )
            .await?;
            Ok(message("Opened pull request"))
        }
        UiAction::OpenSlotEditor { target } => {
            let path = slot_path(ctx, target)?;
            crate::editor::open(ctx, &path).await?;
            Ok(message(format!("Opened {}", path.display())))
        }
        action @ (UiAction::PrepareSessions { .. }
        | UiAction::PrepareStopTerminal { .. }
        | UiAction::StopTerminal { .. }
        | UiAction::StopSession { .. }
        | UiAction::KillSession { .. }
        | UiAction::PrepareHarnesses { .. }) => crate::session_ui::execute(ctx, action).await,
        UiAction::SelectSession { .. }
        | UiAction::EnterHarness { .. }
        | UiAction::PerfInvestigate { .. } => {
            bail!("session attachment is owned by the controller")
        }
        UiAction::CancelAutomations => {
            bail!("queued automations must be cancelled through the host service")
        }
        UiAction::ToggleAutomations { key } => {
            let members = board
                .map(|board| {
                    board
                        .rows
                        .iter()
                        .map(|row| {
                            wt_core::ChainMember::new(
                                &row.key,
                                row.branch.clone(),
                                row.base_branch.clone(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let stacks = wt_core::build_stack_index(&members, &ctx.config.branch.base);
            let stack = key
                .as_ref()
                .and_then(|key| members.iter().find(|member| member.slug == *key))
                .and_then(|member| stacks.by_branch.get(&member.branch))
                .map(|entry| &stacks.layouts[entry.layout_index])
                .map(|stack| {
                    (
                        stack.stack_id.clone(),
                        stack
                            .nodes
                            .iter()
                            .map(|node| node.slug.clone())
                            .collect::<Vec<_>>(),
                    )
                });
            let paused = ctx
                .database
                .call(move |store| {
                    if let Some((id, members)) = stack {
                        return Ok(store.toggle_stack_automations_paused(&id, &members)?);
                    }
                    match key {
                        Some(slug) => Ok(store.toggle_slug_automations_paused(&slug)?),
                        None => Ok(store.toggle_global_automations_paused()?),
                    }
                })
                .await?;
            Ok(message(if paused {
                "Automations paused"
            } else {
                "Automations resumed"
            }))
        }
        UiAction::OnHost { .. } => bail!("host routing must be resolved before local dispatch"),
        UiAction::PrepareCreate { initial } => Ok(modal(UiModal::Text {
            action: wt_tui::TextAction::Create,
            prompt: "New worktree name: ".into(),
            initial,
            allow_empty: false,
        })),
        UiAction::KillAction { action_key, run_id } => {
            let killed = crate::actions::service(ctx)?
                .kill_run(&action_key, &run_id, &ctx.cancellation)
                .await?;
            Ok(message(if killed {
                "Action stopped"
            } else {
                "That action is no longer running"
            }))
        }
        UiAction::PrepareActions { .. }
        | UiAction::PrepareGithub { .. }
        | UiAction::GithubMarkReady { .. }
        | UiAction::GithubSetAutoMerge { .. }
        | UiAction::GithubShip { .. }
        | UiAction::GithubFailedChecks { .. }
        | UiAction::GenerateTitle { .. }
        | UiAction::PrepareAction { .. }
        | UiAction::RunAction { .. } => {
            bail!("action palette requests must be routed through their controller")
        }
        UiAction::PrepareSection { key } => crate::section_actions::prepare(ctx, key, board).await,
        UiAction::Reorder { key, section, down } => {
            crate::section_actions::reorder(ctx, key, section, down, board).await
        }
        UiAction::MoveSection { key, section } => {
            crate::section_actions::move_row(ctx, key, section, board).await
        }
        UiAction::RenameSection { old, new } => crate::section_actions::rename(ctx, old, new).await,
        UiAction::FoldSection { key, folded } => {
            ctx.database
                .call(move |store| {
                    store.set_section_folded(&key, folded)?;
                    Ok(())
                })
                .await?;
            Ok(message(if folded {
                "Section folded"
            } else {
                "Section expanded"
            }))
        }
        UiAction::CyclePrimary => {
            let app = crate::harness::AppHarness::new(ctx);
            let config = ctx.config.clone();
            let next = tokio::task::spawn_blocking(move || {
                let choices: Vec<_> = wt_core::HarnessId::ALL
                    .into_iter()
                    .filter(|id| !config.harness.hidden.contains(id))
                    .collect();
                let current = app.primary();
                let next = match choices.iter().position(|id| *id == current) {
                    Some(index) => choices[(index + 1) % choices.len()],
                    None => *choices
                        .first()
                        .context("all harnesses are hidden in configuration")?,
                };
                crate::harness::persist_primary(&config.paths.cache_root, next)?;
                Ok::<_, anyhow::Error>(next)
            })
            .await??;
            Ok(UiReply {
                primary_harness: Some(harness_name(next).into()),
                ..message(format!("Primary harness: {}", harness_name(next)))
            })
        }
        UiAction::SetTitle { key, title } => {
            let row = resolve_key(ctx, &key).await?;
            let slug = row.target.slug().to_owned();
            ctx.database
                .call(move |store| {
                    store.set_slug_manual_title(&slug, &title, None)?;
                    Ok(())
                })
                .await?;
            Ok(message("Title saved"))
        }
        UiAction::Copy { value, label } => {
            #[cfg(target_os = "macos")]
            let mut command = wt_platform::process::CommandSpec::new("pbcopy");
            #[cfg(not(target_os = "macos"))]
            let mut command = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                wt_platform::process::CommandSpec::new("wl-copy")
            } else {
                wt_platform::process::CommandSpec::new("xclip").args(["-selection", "clipboard"])
            };
            command.input = Some(value.into_bytes());
            command.timeout = std::time::Duration::from_secs(3);
            command.preserve_children_on_success = true;
            let name = command.program.clone();
            ctx.processes
                .run(command, &ctx.cancellation)
                .await?
                .checked(name)?;
            Ok(message(format!("Copied {label}")))
        }
        UiAction::Create { input } => create(ctx, &input).await,
        UiAction::OpenEditor { key } => {
            let row = resolve_key(ctx, &key).await?;
            crate::editor::open(ctx, Path::new(&row.target.path)).await?;
            Ok(message(format!("Opened {}", row.target.slug())))
        }
        UiAction::PrepareRemove { key } => {
            let row = resolve_key(ctx, &key).await?;
            let plans = lifecycle_ops::plan_cached(ctx, vec![row], github).await?;
            let plan = plans
                .rows
                .into_iter()
                .next()
                .context("worktree disappeared")?;
            let force = !plan.hazards.is_empty();
            let mut lines = vec![format!(
                "{} ({})",
                plan.row.target.slug(),
                plan.row.target.branch
            )];
            if force {
                lines.push(format!(
                    "Will discard or abandon: {}",
                    plan.hazards.join(", ")
                ));
            }
            if plan.destroy_stage {
                lines.push(format!("Destroy deployed stage {}", plan.row.target.stage));
            }
            if let Some(warning) = plans.warning {
                lines.push(warning);
            }
            Ok(modal(UiModal::Confirm {
                action: ConfirmAction::Remove {
                    key,
                    force,
                    revision: ui_revision(&plan.revision),
                },
                title: if force {
                    "Force remove worktree?"
                } else {
                    "Remove worktree?"
                }
                .into(),
                lines,
                cancel_key: Some('d'),
            }))
        }
        UiAction::Remove {
            key,
            force,
            revision,
        } => {
            let row = resolve_key(ctx, &key).await?;
            let plans = lifecycle_ops::plan_cached(ctx, vec![row], github).await?;
            let mut plan = plans
                .rows
                .into_iter()
                .next()
                .context("worktree disappeared")?;
            let current_revision = ui_revision(&plan.revision);
            if revision != current_revision {
                let next_force = !plan.hazards.is_empty();
                let mut lines = vec![
                    format!("{} ({})", plan.row.target.slug(), plan.row.target.branch),
                    "Checkout or hazards changed since the previous confirmation.".into(),
                ];
                if next_force {
                    lines.push(format!(
                        "Will discard or abandon: {}",
                        plan.hazards.join(", ")
                    ));
                }
                if plan.destroy_stage {
                    lines.push(format!("Destroy deployed stage {}", plan.row.target.stage));
                }
                return Ok(modal(UiModal::Confirm {
                    action: ConfirmAction::Remove {
                        key,
                        force: next_force,
                        revision: current_revision,
                    },
                    title: if next_force {
                        "Review changed removal hazards"
                    } else {
                        "Review changed checkout"
                    }
                    .into(),
                    lines,
                    cancel_key: Some('d'),
                }));
            }
            if let Some(visible) =
                board.and_then(|board| board.rows.iter().find(|row| row.key == key))
                && visible.title != visible.slug
                && !visible.title.trim().is_empty()
            {
                plan.removed_snapshot
                    .extra
                    .insert("title".into(), serde_json::json!(visible.title));
            }
            crate::commands::_destroy::start_remove(
                ctx,
                &plan.row,
                crate::commands::_destroy::DestroyOptions {
                    force,
                    delete_branch: true,
                    landed: plan.landed,
                    destroy_stage: plan.destroy_stage,
                    expected_revision: Some(plan.revision.clone()),
                    removed_snapshot: Some(plan.removed_snapshot.clone()),
                },
            )
            .await?;
            Ok(message(format!(
                "Removal queued for {}",
                plan.row.target.slug()
            )))
        }
        UiAction::PrepareCleanup => {
            let rows = ctx.repository.inventory(&ctx.cancellation).await?;
            let plans = lifecycle_ops::plan_cached(ctx, rows, github).await?;
            let mut revisions = Vec::new();
            let mut lines = Vec::new();
            for plan in plans.rows {
                if !plan.landed {
                    continue;
                }
                if plan.hazards.is_empty() {
                    revisions.push(ui_revision(&plan.revision));
                    lines.push(format!("Remove {}", plan.row.target.slug()));
                } else {
                    lines.push(format!(
                        "Keep {}: {}",
                        plan.row.target.slug(),
                        plan.hazards.join(", ")
                    ));
                }
            }
            if let Some(warning) = plans.warning {
                lines.push(warning);
            }
            if revisions.is_empty() {
                return Ok(message(format!(
                    "Nothing safe to clean. {}",
                    lines.join("; ")
                )));
            }
            Ok(modal(UiModal::Confirm {
                action: ConfirmAction::Cleanup { revisions },
                title: "Clean landed worktrees?".into(),
                lines,
                cancel_key: Some('c'),
            }))
        }
        UiAction::Cleanup { revisions } => Ok(message(
            lifecycle_ops::cleanup_cached(ctx, &revisions, github).await?,
        )),
        UiAction::ToggleArchive { key } => {
            if wt_core::is_remote_worktree_ledger_key(&key) {
                board
                    .and_then(|board| board.rows.iter().find(|row| row.key == key))
                    .context("remote worktree metadata is still loading")?;
            } else {
                let row = resolve_key(ctx, &key).await?;
                let slug = row.target.slug().to_owned();
                if FileLock::try_acquire(&ctx.config.paths.lock_dir, &slug, "archive probe")
                    .await?
                    .is_none()
                {
                    let op = board
                        .and_then(|board| board.rows.iter().find(|row| row.key == key))
                        .and_then(|row| row.busy.as_ref())
                        .map_or("busy".to_owned(), |busy| busy.label.clone());
                    bail!("{slug} is {op}; can't change archive state");
                }
            }
            let slug = key;
            let archived = ctx
                .database
                .call(move |store| {
                    let archived = !store.read_archived_keys()?.contains(&slug);
                    store.set_archived(&slug, archived)?;
                    if archived {
                        store.set_section_folded(crate::board_layout::ARCHIVED, true)?;
                    } else {
                        // Restored rows return to the Inbox, as in TS.
                        store.move_worktrees_to_section(std::slice::from_ref(&slug), None)?;
                    }
                    Ok(archived)
                })
                .await?;
            Ok(message(if archived { "Archived" } else { "Restored" }))
        }
        UiAction::PrepareStatus { key } => prepare_status(ctx, key).await,
        UiAction::SetStatus {
            key,
            state,
            note,
            verify_after_merge,
        } => set_status(ctx, &key, state, note, verify_after_merge).await,
        UiAction::PrepareBase { key } => {
            let row = resolve_key(ctx, &key).await?;
            let slug = row.target.slug().to_owned();
            let state = ctx
                .database
                .call(move |store| Ok(store.read_wt_state()?))
                .await?;
            let current = state["slugs"][slug]["baseBranch"]
                .as_str()
                .map(str::to_owned);
            let refs = run_git(
                ctx,
                &ctx.config.paths.main_clone,
                [
                    "for-each-ref",
                    "--format=%(refname:short)",
                    "refs/heads",
                    "refs/remotes/origin",
                ],
            )
            .await?
            .checked("git")?
            .stdout_text();
            let mut branches = refs
                .lines()
                .map(str::trim)
                .filter(|branch| {
                    !branch.is_empty()
                        && *branch != "origin/HEAD"
                        && *branch != row.target.branch.as_str()
                })
                .map(str::to_owned)
                .collect::<std::collections::BTreeSet<_>>();
            if let Some(recorded) = &current
                && recorded != &ctx.config.branch.base
                && recorded != &row.target.branch
            {
                branches.insert(recorded.clone());
            }
            let mut options = vec![PickerOption {
                value: None,
                label: format!("{} (default)", ctx.config.branch.base),
                chord: None,
                note: None,
                verify_after_merge: None,
                detail: None,
            }];
            options.extend(branches.into_iter().map(|branch| PickerOption {
                label: if current.as_deref() == Some(branch.as_str()) {
                    format!("{branch} (current)")
                } else {
                    branch.clone()
                },
                value: Some(branch),
                chord: None,
                note: None,
                verify_after_merge: None,
                detail: None,
            }));
            let selected = current
                .as_ref()
                .and_then(|current| {
                    options
                        .iter()
                        .position(|option| option.value.as_ref() == Some(current))
                })
                .unwrap_or(0);
            Ok(modal(UiModal::Picker {
                action: PickerAction::Base { key },
                title: "Record fork base".into(),
                options,
                selected,
            }))
        }
        UiAction::SetBase { key, base } => set_base(ctx, &key, base).await,
        UiAction::SetIssueOverride { key, issue_id } => {
            let row = resolve_key(ctx, &key).await?;
            let value = issue_id.unwrap_or_default().trim().to_ascii_uppercase();
            if !value.is_empty()
                && !regex::Regex::new(r"^[A-Z][A-Z0-9]*-\d+$")
                    .expect("constant issue regex")
                    .is_match(&value)
            {
                bail!("Issue ID must look like ENG-123, or be empty for no issue");
            }
            let slug = row.target.slug().to_owned();
            ctx.database
                .call(move |store| {
                    store.set_slug_issue_id(&slug, Some(&value))?;
                    Ok(())
                })
                .await?;
            Ok(message("Issue updated"))
        }
        UiAction::OpenUrl { key, kind } => {
            if !wt_core::is_remote_worktree_ledger_key(&key) {
                resolve_key(ctx, &key).await?;
            }
            let row = board
                .and_then(|board| board.rows.iter().find(|row| row.key == key))
                .context("worktree metadata is still loading")?;
            let url = match kind {
                UrlKind::PullRequest => row.pr_url.as_deref(),
                UrlKind::Issue => row.issue_url.as_deref().or(row.github_issue_url.as_deref()),
                UrlKind::PrimaryIssue => row.issue_url.as_deref(),
                UrlKind::StageOrDev => row.stage_url.as_deref().or(row.dev_url.as_deref()),
            }
            .context("no URL is available for this worktree")?;
            let url = if kind == UrlKind::PullRequest {
                wt_github::pull_request_open_url(url, ctx.config.github.pr_target)
            } else {
                url.to_owned()
            };
            crate::editor::open_url(ctx, &url).await?;
            Ok(message("Opened URL"))
        }
        UiAction::Session { .. } => unreachable!("session handoff is owned by the controller"),
    }
}

/// The checkout a special slot works in.
pub(crate) fn slot_path(
    ctx: &AppContext,
    target: wt_tui::SessionTarget,
) -> Result<std::path::PathBuf> {
    use wt_tui::SessionTarget;
    match target {
        SessionTarget::Main | SessionTarget::Manager => Ok(ctx.config.paths.main_clone.clone()),
        SessionTarget::WtSource => ctx
            .config
            .paths
            .wt_source
            .clone()
            .filter(|path| path.is_dir())
            .context("wt source checkout is unavailable"),
        SessionTarget::Dotfiles => Some(ctx.config.paths.dotfiles.clone())
            .filter(|path| path.is_dir())
            .context("dotfiles checkout is unavailable"),
        SessionTarget::Harness | SessionTarget::Shell | SessionTarget::Diff => {
            bail!("worktree targets are not slots")
        }
    }
}

async fn create(ctx: &AppContext, input: &str) -> Result<UiReply> {
    let words = shlex::split(input).context("unterminated quote in new-worktree input")?;
    let matches = NewArgs::augment_args(clap::Command::new("new"))
        .try_get_matches_from(std::iter::once("new".to_owned()).chain(words))?;
    let args = NewArgs::from_arg_matches(&matches)?;
    let branch = crate::commands::new::parse_branch(ctx, &args.positionals.join(" "), &args, false)
        .await
        .map_err(anyhow::Error::msg)?;
    let existing = ctx
        .repository
        .inventory(&ctx.cancellation)
        .await?
        .into_iter()
        .find(|row| !row.is_main && row.target.branch == branch);
    let target = if let Some(row) = existing {
        row.target
    } else {
        lifecycle_ops::service(ctx)?
            .create(
                &branch,
                CreateOptions {
                    base: args.base,
                    fetch_origin: true,
                    run_install: !args.no_install,
                },
                &ctx.cancellation,
            )
            .await?
            .target
    };
    if let Some(issue) = args.gh {
        let slug = target.slug().to_owned();
        ctx.database
            .call(move |store| {
                store.set_slug_github_issue(&slug, Some(issue))?;
                Ok(())
            })
            .await?;
    }
    if args.open && !args.no_open {
        crate::editor::open(ctx, Path::new(&target.path)).await?;
    }
    let inventory = ctx.repository.inventory(&ctx.cancellation).await?;
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let mut placement = Board {
        rows: inventory
            .into_iter()
            .filter(|row| !row.is_main)
            .map(|row| wt_tui::BoardRow {
                key: wt_core::worktree_target_key(&row.target),
                slug: row.target.slug().to_owned(),
                branch: row.target.branch,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    crate::board_layout::prepare(
        &mut placement,
        &state,
        &ctx.config.branch.base,
        ctx.config.ui.sort,
    );
    if let Some(section) =
        crate::board_layout::section_for(&placement, &wt_core::worktree_target_key(&target))
    {
        ctx.database
            .call(move |store| {
                store.set_section_folded(&section, false)?;
                Ok(())
            })
            .await?;
    }
    Ok(UiReply {
        message: format!("Created {}", target.slug()),
        select_when_visible: Some(wt_core::worktree_target_key(&target)),
        ..Default::default()
    })
}

const STATUS_CHORDS: [(&str, char); 8] = [
    ("todo", 't'),
    ("working", 'w'),
    ("review", 'r'),
    ("needs-testing", 'n'),
    ("needs-human", 'h'),
    ("ready", 'y'),
    ("verified", 'v'),
    ("dropped", 'd'),
];

async fn prepare_status(ctx: &AppContext, key: String) -> Result<UiReply> {
    let row = resolve_key(ctx, &key).await?;
    let slug = row.target.slug().to_owned();
    let previous = ctx
        .database
        .call(move |store| Ok(store.read_slug_state(&slug)?))
        .await?;
    let work = previous.as_ref().and_then(|entry| entry.get("work"));
    let note = work
        .and_then(|work| work["note"].as_str())
        .map(str::to_owned);
    let verify = work
        .and_then(|work| work["verifyAfterMerge"].as_str())
        .map(str::to_owned);
    // TS order: the plain states, `a` beside `y`, then clear. The current
    // claim is marked and starts highlighted; a ready claim with an owed
    // check is the `a` row.
    let current_state = work.and_then(|work| work["state"].as_str());
    let current = match current_state {
        Some("ready") if verify.is_some() => Some('a'),
        Some(state) => STATUS_CHORDS
            .iter()
            .find(|(name, _)| *name == state)
            .map(|(_, chord)| *chord),
        None => Some('x'),
    };
    let mut options = Vec::new();
    for (state, chord) in STATUS_CHORDS {
        options.push(PickerOption {
            value: Some(state.into()),
            label: state.into(),
            chord: Some(chord),
            note: note.clone(),
            verify_after_merge: None,
            detail: None,
        });
        if chord == 'y' {
            options.push(PickerOption {
                value: Some("ready".into()),
                label: "ready + verify after merge".into(),
                chord: Some('a'),
                note: None,
                verify_after_merge: Some(verify.clone().unwrap_or_default()),
                detail: None,
            });
        }
    }
    options.push(PickerOption {
        value: None,
        label: "clear status".into(),
        chord: Some('x'),
        note: None,
        verify_after_merge: None,
        detail: None,
    });
    let selected = options
        .iter()
        .position(|option| option.chord == current)
        .unwrap_or(0);
    if current_state.is_some() {
        options[selected].label.push_str(" (current)");
    }
    Ok(modal(UiModal::Picker {
        action: PickerAction::Status { key },
        title: "Work status".into(),
        options,
        selected,
    }))
}

async fn set_status(
    ctx: &AppContext,
    key: &str,
    state: Option<String>,
    note: Option<String>,
    verify: Option<String>,
) -> Result<UiReply> {
    let row = resolve_key(ctx, key).await?;
    let slug = row.target.slug().to_owned();
    let state = state
        .map(|state| wt_core::resolve_work_state(&state).context("unknown work status"))
        .transpose()?;
    let head = run_git(
        ctx,
        Path::new(&row.target.path),
        ["rev-parse", "--verify", "HEAD"],
    )
    .await?
    .checked("git")?
    .stdout_text()
    .trim()
    .to_owned();
    let at = OffsetDateTime::now_utc().format(&Rfc3339)?;
    ctx.database
        .call(move |store| {
            let previous = store
                .read_slug_state(&slug)?
                .and_then(|entry| entry.get("work").cloned())
                .and_then(|work| serde_json::from_value::<wt_store::WorkStatusRecord>(work).ok());
            let record = state.map(|state| {
                let terminal = matches!(
                    state,
                    wt_core::WorkState::Verified | wt_core::WorkState::Dropped
                );
                wt_store::WorkStatusRecord {
                    state: state.as_str().into(),
                    at,
                    note: note
                        .map(|text| wt_core::sanitize_work_note(&text))
                        .filter(|text| !text.is_empty()),
                    risk: if state == wt_core::WorkState::Ready {
                        previous.as_ref().and_then(|work| work.risk.clone())
                    } else {
                        None
                    },
                    sha: Some(head),
                    by: Some("human".into()),
                    blocked_on: None,
                    verify_after_merge: if terminal {
                        None
                    } else {
                        match verify {
                            Some(text) => (!text.trim().is_empty())
                                .then(|| wt_core::sanitize_work_note(&text)),
                            None => previous.and_then(|work| work.verify_after_merge),
                        }
                    },
                    extra: Default::default(),
                }
            });
            store.set_slug_work_status(&slug, record.as_ref())?;
            Ok(())
        })
        .await?;
    Ok(message("Work status updated"))
}

async fn set_base(ctx: &AppContext, key: &str, base: Option<String>) -> Result<UiReply> {
    crate::fork_base::set(ctx, key, base).await?;
    Ok(message("Fork base updated"))
}

fn ui_revision(revision: &wt_lifecycle::RemovalRevision) -> UiRemovalRevision {
    UiRemovalRevision {
        key: revision.key.clone(),
        path: revision.path.clone(),
        branch: revision.branch.clone(),
        head: revision.head.clone(),
        digest: revision.digest.clone(),
        hazards: revision.hazards.clone(),
        published_base: revision.published_base.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn human_status_preserves_verification_until_explicitly_cleared() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let row = crate::commands::resolve::resolve_named_worktree(&fixture.ctx, "one")
            .await
            .unwrap();
        let key = wt_core::worktree_target_key(&row.target);
        set_status(
            &fixture.ctx,
            &key,
            Some("ready".into()),
            Some("checked locally".into()),
            Some("STEPS: check deployed version".into()),
        )
        .await
        .unwrap();
        set_status(&fixture.ctx, &key, Some("working".into()), None, None)
            .await
            .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(
            state["slugs"]["one"]["work"]["verifyAfterMerge"],
            "STEPS: check deployed version"
        );
        set_status(
            &fixture.ctx,
            &key,
            Some("ready".into()),
            None,
            Some(String::new()),
        )
        .await
        .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert!(
            state["slugs"]["one"]["work"]
                .get("verifyAfterMerge")
                .is_none()
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn fork_picker_rejects_cycles_and_keeps_a_real_merge_base() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let rows = fixture
            .ctx
            .repository
            .inventory(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let key = |slug: &str| {
            wt_core::worktree_target_key(
                &rows
                    .iter()
                    .find(|row| row.target.slug() == slug)
                    .unwrap()
                    .target,
            )
        };
        set_base(&fixture.ctx, &key("one"), Some("feature/two".into()))
            .await
            .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(state["slugs"]["one"]["baseBranch"], "feature/two");
        assert_eq!(state["slugs"]["one"]["baseSha"].as_str().unwrap().len(), 40);
        assert!(
            set_base(&fixture.ctx, &key("two"), Some("feature/one".into()))
                .await
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
        assert!(
            set_base(&fixture.ctx, &key("one"), Some("missing".into()))
                .await
                .is_err()
        );
        let after = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(
            after["slugs"]["one"]["baseSha"],
            state["slugs"]["one"]["baseSha"]
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn clearing_issue_from_ui_persists_explicit_none_and_stale_keys_refuse_writes() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let row = crate::commands::resolve::resolve_named_worktree(&fixture.ctx, "one")
            .await
            .unwrap();
        execute(
            &fixture.ctx,
            UiAction::SetIssueOverride {
                key: wt_core::worktree_target_key(&row.target),
                issue_id: None,
            },
            None,
            &wt_github::GithubData::default(),
        )
        .await
        .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(state["slugs"]["one"]["issueId"], "");
        assert!(
            execute(
                &fixture.ctx,
                UiAction::SetTitle {
                    key: "missing".into(),
                    title: "new".into()
                },
                None,
                &wt_github::GithubData::default()
            )
            .await
            .is_err()
        );
        fixture.close().await.unwrap();
    }
}
