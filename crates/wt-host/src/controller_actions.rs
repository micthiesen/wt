use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::{Args, FromArgMatches};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio_util::sync::CancellationToken;
use wt_core::Domain;
use wt_lifecycle::CreateOptions;
use wt_platform::lock::FileLock;
use wt_runtime::{Confirmed, Provenance, SourceState};
use wt_tui::{
    Board, ConfirmAction, RemovalRevision as UiRemovalRevision, UiAction, UiModal, UiReply, UrlKind,
};

use wt_app_core::context::AppContext;
use wt_ops::{
    commands::new::NewArgs,
    commands::resolve::run_git,
    lifecycle_ops::{self, resolve_key},
};

fn harness_name(harness: wt_core::HarnessId) -> &'static str {
    match harness {
        wt_core::HarnessId::Claude => "Claude",
        wt_core::HarnessId::Codex => "Codex",
        wt_core::HarnessId::Opencode => "OpenCode",
        wt_core::HarnessId::Pi => "Pi",
    }
}

/// Forget a failed removal record whose checkout is gone. This writes only
/// the `removals` record: no files, branches or history change. Runs in the
/// key's removal lane, so no removal of this key starts concurrently here; the
/// store write rechecks the record and the path under its own transaction.
async fn dismiss_failed_removal(
    ctx: &AppContext,
    key: String,
    seen: wt_tui::FailedRemovalSeen,
) -> Result<UiReply> {
    // Inventory is authoritative for whether Git still knows the checkout. A
    // listed key or path means a retry, not a dismissal, is the right review.
    let inventory = ctx.repository.inventory(&ctx.cancellation).await?;
    if inventory.iter().any(|row| {
        !row.is_main
            && (wt_core::worktree_target_key(&row.target) == key || row.target.path == seen.path)
    }) {
        return Err(wt_app_core::host_errors::stale(
            "the checkout is in Git inventory again; review a retry instead",
        ));
    }
    let wt_tui::FailedRemovalSeen {
        job_id,
        path,
        branch,
    } = seen;
    let slug = key.clone();
    let outcome = ctx
        .database
        .call(move |store| {
            let checkout = std::path::PathBuf::from(&path);
            Ok(
                store.clear_failed_removal(&key, &job_id, &path, &branch, || {
                    checkout_absent(&checkout)
                })?,
            )
        })
        .await?;
    match outcome {
        wt_store::ClearFailedRemoval::Cleared => {
            Ok(message(format!("Dismissed failed removal of {slug}")))
        }
        wt_store::ClearFailedRemoval::Changed => Err(wt_app_core::host_errors::stale(
            "the removal record changed since the dismissal menu was shown",
        )),
        wt_store::ClearFailedRemoval::CheckoutPresent => Err(wt_app_core::host_errors::stale(
            "the checkout path exists again; review a retry instead",
        )),
    }
}

/// Only a definite "not found" proves absence. A dangling symlink still
/// occupies the path, and an unreadable parent proves nothing.
fn checkout_absent(path: &Path) -> bool {
    matches!(
        std::fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
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
    github: Option<&Confirmed<wt_github::GithubData>>,
) -> Result<UiReply> {
    execute_with_shutdown(ctx, action, board, github, None).await
}

pub async fn execute_with_shutdown(
    ctx: &AppContext,
    action: UiAction,
    board: Option<&Board>,
    github: Option<&Confirmed<wt_github::GithubData>>,
    shutdown: Option<&CancellationToken>,
) -> Result<UiReply> {
    match action {
        // Built by the TUI from its board (`UiAction::menu`); the host
        // service refuses them before dispatch.
        UiAction::PrepareStatus { .. }
        | UiAction::PrepareBase { .. }
        | UiAction::PrepareSection { .. }
        | UiAction::PrepareDeleteSection { .. }
        | UiAction::PrepareHardRefresh
        | UiAction::PrepareReviewCheckout { .. }
        | UiAction::PrepareRestoreRemoved { .. }
        | UiAction::PrepareHarnesses { .. } => {
            bail!("this menu is built from the board and never asks the host")
        }
        // Answered by the host service, from the board's own entries when they
        // exist and from its discovery snapshot otherwise.
        UiAction::PrepareSessions { .. } => {
            bail!("session pickers are answered by the host service")
        }
        // Queued by the host service before dispatch; it never produces a reply.
        UiAction::DiscoverSessions { .. } => {
            bail!("session discovery is queued by the host service")
        }
        UiAction::HardRefresh
        | UiAction::ReviewCheckout { .. }
        | UiAction::DismissReviewRequest { .. }
        | UiAction::PrepareReviewers { .. }
        | UiAction::SubmitReviewers { .. }
        | UiAction::SetPerf { .. }
        | UiAction::SetHistoryActive { .. }
        | UiAction::SetAttentionSeen { .. }
        | UiAction::Restack { .. }
        | UiAction::RestoreRemoved { .. }
        | UiAction::ToggleRemovedAutomations { .. } => {
            bail!("request must run through its host service")
        }
        UiAction::OpenLink { url } => {
            wt_app_core::editor::open_url(ctx, &url).await?;
            Ok(message("Opened link"))
        }
        UiAction::OpenPrLink { url, linear } => {
            let target = if linear {
                wt_config::PullRequestTarget::Linear
            } else {
                wt_config::PullRequestTarget::Github
            };
            wt_app_core::editor::open_url(ctx, &wt_github::pull_request_open_url(&url, target))
                .await?;
            Ok(message("Opened pull request"))
        }
        UiAction::OpenPrDefault { url } => {
            wt_app_core::editor::open_url(
                ctx,
                &wt_github::pull_request_open_url(&url, ctx.config.github.pr_target),
            )
            .await?;
            Ok(message("Opened pull request"))
        }
        UiAction::OpenSlotEditor { target } => {
            let path = slot_path(ctx, target)?;
            wt_app_core::editor::open(ctx, &path).await?;
            Ok(message(format!("Opened {}", path.display())))
        }
        action @ (UiAction::PrepareStopTerminal { .. }
        | UiAction::StopTerminal { .. }
        | UiAction::StopSession { .. }
        | UiAction::KillSession { .. }) => crate::session_ui::execute(ctx, action).await,
        UiAction::SelectSession { .. }
        | UiAction::EnterHarness { .. }
        | UiAction::PerfInvestigate { .. } => {
            bail!("session attachment is owned by the controller")
        }
        UiAction::CancelAutomations => {
            bail!("queued automations must be cancelled through the host service")
        }
        UiAction::ToggleAutomations { key, seen } => {
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
                    // The witness is the flag the board shows: the row's own
                    // or the global one. The store compares it and writes in
                    // one transaction.
                    let seen = seen.map(|wt_tui::Seen(seen)| seen);
                    let outcome = match (stack, &key) {
                        (Some((id, members)), Some(slug)) => store
                            .toggle_stack_automations_paused_seen(
                                &id,
                                &members,
                                seen.map(|seen| (slug.as_str(), seen)),
                            )?,
                        (_, Some(slug)) => store.toggle_slug_automations_paused_seen(slug, seen)?,
                        (_, None) => store.toggle_global_automations_paused_seen(seen)?,
                    };
                    match outcome {
                        wt_store::SeenToggle::Set(paused) => Ok(paused),
                        wt_store::SeenToggle::Stale { current } => {
                            Err(wt_app_core::host_errors::stale(if current {
                                "automations are now paused"
                            } else {
                                "automations are now running"
                            }))
                        }
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
        UiAction::ManagedSignIn { .. } => bail!("sign-in belongs to a managed remote host"),
        // Ctrl+N builds its host picker and name prompt from the board. This
        // arm answers only a host the user already chose, with the same prompt.
        UiAction::PrepareCreate { initial, placement } => {
            Ok(modal(wt_tui::new_worktree_prompt(&initial, placement)))
        }
        UiAction::KillAction { action_key, run_id } => {
            let killed = wt_app_core::actions::service(ctx)?
                .kill_run(&action_key, &run_id, &ctx.cancellation)
                .await?;
            Ok(message(if killed {
                "Action stopped"
            } else {
                "That action is no longer running"
            }))
        }
        UiAction::PrepareActions { .. }
        | UiAction::WarmPalette { .. }
        | UiAction::PrepareGithub { .. }
        | UiAction::GithubMarkReady { .. }
        | UiAction::GithubSetAutoMerge { .. }
        | UiAction::GithubShip { .. }
        | UiAction::GithubFailedChecks { .. }
        | UiAction::PrepareAction { .. }
        | UiAction::RunAction { .. } => {
            bail!("action palette requests must be routed through their controller")
        }
        UiAction::Reorder { key, section, down } => {
            crate::section_actions::reorder(ctx, key, section, down, board).await
        }
        UiAction::MoveSection { key, section, seen } => {
            crate::section_actions::move_row(ctx, key, section, seen, board).await
        }
        UiAction::RenameSection { old, new } => crate::section_actions::rename(ctx, old, new).await,
        UiAction::DeleteSection { section, seen } => {
            crate::section_actions::delete(ctx, section, seen, board).await
        }
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
            let app = wt_app_core::harness::AppHarness::new(ctx);
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
                wt_app_core::harness::persist_primary(&config.paths.cache_root, next)?;
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
                    // The TUI rename is the user's, and final.
                    store.set_slug_title(&slug, &title, wt_store::TitleSource::User, None)?;
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
        UiAction::Create { input, .. } => {
            create_resolved(ctx, resolve_creation(ctx, &input).await?).await
        }
        UiAction::ResolveCreation { input } => Ok(UiReply {
            creation_plan: Some(resolve_creation(ctx, &input).await?),
            ..Default::default()
        }),
        UiAction::CreateResolved { plan } => create_resolved(ctx, plan).await,
        UiAction::OpenEditor { key } => {
            let row = resolve_key(ctx, &key).await?;
            wt_app_core::editor::open(ctx, Path::new(&row.target.path)).await?;
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
                lines.extend(removal_hazard_lines(
                    &plan.hazards,
                    dirty_file_count(board, &key),
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
                    revision: Some(ui_revision(&plan.revision)),
                    seen: None,
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
        UiAction::DismissFailedRemoval { key, seen } => {
            dismiss_failed_removal(ctx, key, seen.0).await
        }
        UiAction::Remove {
            key,
            force,
            revision,
            seen,
        } => {
            // A board menu witnesses only its checkout identity, and asserts no
            // revision. Anything without either witness asserts nothing.
            if revision.is_none() && seen.is_none() {
                bail!("removal must name the checkout it was reviewed against");
            }
            let row = resolve_key(ctx, &key).await?;
            let plans = lifecycle_ops::plan_cached(ctx, vec![row], github).await?;
            let warning = plans.warning.clone();
            let plan = plans
                .rows
                .into_iter()
                .next()
                .context("worktree disappeared")?;
            let current_revision = ui_revision(&plan.revision);
            if let Some(wt_tui::Seen(seen)) = seen.as_deref()
                && (seen.path != plan.revision.path
                    || seen.branch != plan.revision.branch
                    || seen.head != plan.revision.head)
            {
                return Err(wt_app_core::host_errors::stale(
                    "checkout changed since the removal menu was shown",
                ));
            }
            let changed = revision
                .as_ref()
                .is_some_and(|revision| *revision != current_revision);
            // The board authorizes no hazards. Any hazard, a deployed
            // stage, or a warning goes back to the user before anything runs.
            let review = !plan.hazards.is_empty() || plan.destroy_stage || warning.is_some();
            if changed || (revision.is_none() && (force || review)) {
                let next_force = !plan.hazards.is_empty();
                let mut lines = vec![
                    format!("{} ({})", plan.row.target.slug(), plan.row.target.branch),
                    if changed {
                        "Checkout or hazards changed since the previous confirmation."
                    } else {
                        "Current removal checks require confirmation."
                    }
                    .into(),
                ];
                if next_force {
                    lines.extend(removal_hazard_lines(
                        &plan.hazards,
                        dirty_file_count(board, &key),
                    ));
                }
                if plan.destroy_stage {
                    lines.push(format!("Destroy deployed stage {}", plan.row.target.stage));
                }
                if let Some(warning) = warning {
                    lines.push(warning);
                }
                return Ok(modal(UiModal::Confirm {
                    action: ConfirmAction::Remove {
                        key,
                        force: next_force,
                        revision: Some(current_revision),
                        seen: None,
                    },
                    title: if changed {
                        "Review changed removal checks"
                    } else {
                        "Review removal hazards"
                    }
                    .into(),
                    lines,
                    cancel_key: Some('d'),
                }));
            }
            let job = wt_ops::commands::_destroy::start_remove(
                ctx,
                &plan.row,
                wt_ops::commands::_destroy::DestroyOptions {
                    force,
                    delete_branch: true,
                    landed: plan.landed,
                    destroy_stage: plan.destroy_stage,
                    expected_revision: Some(plan.revision.clone()),
                    removed_snapshot: Some(plan.removed_snapshot.clone()),
                },
            )
            .await?;
            match wt_ops::commands::_destroy::wait_for_completion_or_shutdown(ctx, &job, shutdown)
                .await?
            {
                wt_ops::commands::_destroy::RemovalWait::Completed => {
                    Ok(message(format!("Removed {}", plan.row.target.slug())))
                }
                wt_ops::commands::_destroy::RemovalWait::ContinuesInBackground => Ok(UiReply {
                    message: format!(
                        "Removal job {job} continues in the background; use `wt logs` to follow it"
                    ),
                    outcome: Some(wt_tui::ReplyOutcome::Accepted(wt_runtime::Stamp::now())),
                    ..Default::default()
                }),
            }
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
        UiAction::CleanupTargets { targets } => Ok(message(
            lifecycle_ops::cleanup_targets_cached(ctx, &targets, github).await?,
        )),
        UiAction::Cleanup { revisions } => Ok(message(
            lifecycle_ops::cleanup_cached(ctx, &revisions, github).await?,
        )),
        UiAction::ToggleArchive { key, seen } => {
            if wt_core::is_remote_worktree_ledger_key(&key) {
                let row = board
                    .and_then(|board| board.rows.iter().find(|row| row.key == key))
                    .context("remote worktree metadata is still loading")?;
                require_live_host(board, row.host.as_deref())?;
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
                    // Flip only the state the board showed; a changed marker
                    // reopens the decision rather than undoing it. Restored
                    // rows return to the Inbox, as in TS.
                    let seen = seen.map(|wt_tui::Seen(seen)| seen);
                    match store.toggle_archived_seen(
                        &slug,
                        seen,
                        wt_sources::board_layout::ARCHIVED,
                    )? {
                        wt_store::SeenToggle::Set(archived) => Ok(archived),
                        wt_store::SeenToggle::Stale { current } => {
                            Err(wt_app_core::host_errors::stale(if current {
                                "row is already archived"
                            } else {
                                "row is no longer archived"
                            }))
                        }
                    }
                })
                .await?;
            Ok(message(if archived { "Archived" } else { "Restored" }))
        }
        UiAction::SetStatus {
            key,
            state,
            note,
            verify_after_merge,
            seen,
        } => set_status(ctx, &key, state, note, verify_after_merge, seen).await,
        UiAction::SetBase { key, base, seen } => set_base(ctx, &key, base, seen).await,
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
                UrlKind::Guide => row.guide.as_ref().map(|guide| guide.url.as_str()),
            }
            .context("no URL is available for this worktree")?;
            let url = if kind == UrlKind::PullRequest {
                wt_github::pull_request_open_url(url, ctx.config.github.pr_target)
            } else {
                url.to_owned()
            };
            wt_app_core::editor::open_url(ctx, &url).await?;
            Ok(message("Opened URL"))
        }
        UiAction::Session { .. } => unreachable!("session handoff is owned by the controller"),
    }
}

/// A remote row is only known to exist while its host's snapshot is live. A
/// restored, failed or still-connecting host shows last known rows, which
/// cannot justify a durable write keyed by them.
fn require_live_host(board: Option<&Board>, host: Option<&str>) -> Result<()> {
    let host = host.context("remote worktree has no host")?;
    let domains = board
        .and_then(|board| board.host_domains.get(host))
        .filter(|domains| !domains.is_empty())
        .with_context(|| format!("{host} is still connecting; try again once it is live"))?;
    // The connection must be live and current, and the host's own worktree
    // inventory must be live there too (not its restored cache).
    let connected = domains.get(&Domain::Host).is_some_and(|meta| {
        meta.provenance() == Provenance::Live && !matches!(meta.state(), SourceState::Failed(_))
    });
    let inventory = domains
        .get(&Domain::Git)
        .is_some_and(|meta| meta.provenance() == Provenance::Live);
    if !connected || !inventory {
        bail!("{host} is showing cached data; try again once it is live");
    }
    Ok(())
}

/// The checkout a special slot works in.
pub fn slot_path(ctx: &AppContext, target: wt_tui::SessionTarget) -> Result<std::path::PathBuf> {
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

async fn resolve_creation(ctx: &AppContext, input: &str) -> Result<wt_tui::CreatePlan> {
    let words = shlex::split(input).context("unterminated quote in new-worktree input")?;
    let matches = NewArgs::augment_args(clap::Command::new("new"))
        .try_get_matches_from(std::iter::once("new".to_owned()).chain(words))?;
    let args = NewArgs::from_arg_matches(&matches)?;
    let wt_ops::commands::new::Parsed { branch, title } =
        wt_ops::commands::new::parse_branch(ctx, &args.positionals.join(" "), &args, false)
            .await
            .map_err(anyhow::Error::msg)?;
    let existing = ctx
        .repository
        .inventory(&ctx.cancellation)
        .await?
        .into_iter()
        .find(|row| !row.is_main && row.target.branch == branch);
    Ok(wt_tui::CreatePlan {
        already_exists: existing.is_some(),
        key: existing
            .as_ref()
            .map(|row| wt_core::worktree_target_key(&row.target))
            .unwrap_or(lifecycle_ops::service(ctx)?.creation_key(&branch)),
        branch,
        base: args.base,
        install: !args.no_install && ctx.config.backend.kind == wt_config::BackendKind::GitWorktree,
        open: args.open && !args.no_open,
        github_issue: args.gh,
        title,
    })
}

async fn create_resolved(ctx: &AppContext, plan: wt_tui::CreatePlan) -> Result<UiReply> {
    let branch = plan.branch;
    let existing = ctx
        .repository
        .inventory(&ctx.cancellation)
        .await?
        .into_iter()
        .find(|row| !row.is_main && row.target.branch == branch);
    let expected = existing
        .as_ref()
        .map(|row| wt_core::worktree_target_key(&row.target))
        .unwrap_or(lifecycle_ops::service(ctx)?.creation_key(&branch));
    anyhow::ensure!(
        expected == plan.key,
        "Creation identity changed; submit the request again"
    );
    let reused = existing.is_some();
    let install = existing.is_none() && plan.install;
    let target = if let Some(row) = existing {
        row.target
    } else {
        let lookup = wt_ops::commands::new::IssueTitleLookup::start(ctx, &plan.title);
        let target = lifecycle_ops::service(ctx)?
            .create(
                &branch,
                CreateOptions {
                    base: plan.base,
                    fetch_origin: true,
                    run_install: false,
                },
                &ctx.cancellation,
            )
            .await?
            .target;
        let issue_title = lookup.finish().await;
        // Creating from the TUI is the user's own request.
        wt_ops::commands::new::record_title(
            ctx,
            target.slug(),
            plan.title,
            wt_store::TitleSource::User,
            issue_title,
        )
        .await;
        target
    };
    let mut setup_warning = None;
    if install && let Err(error) = start_install(ctx, &target).await {
        setup_warning = Some(format!("; dependency setup failed to start: {error:#}"));
    }
    if let Some(issue) = plan.github_issue {
        let slug = target.slug().to_owned();
        ctx.database
            .call(move |store| {
                store.set_slug_github_issue(&slug, Some(issue))?;
                Ok(())
            })
            .await?;
    }
    if plan.open {
        wt_app_core::editor::open(ctx, Path::new(&target.path)).await?;
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
    wt_sources::board_layout::prepare(
        &mut placement,
        &state,
        &ctx.config.branch.base,
        ctx.config.ui.sort,
    );
    if let Some(section) =
        wt_sources::board_layout::section_for(&placement, &wt_core::worktree_target_key(&target))
    {
        ctx.database
            .call(move |store| {
                store.set_section_folded(&section, false)?;
                Ok(())
            })
            .await?;
    }
    Ok(UiReply {
        message: format!(
            "Created {}{}",
            target.slug(),
            setup_warning.unwrap_or_default()
        ),
        creation_reused: reused,
        select_when_visible: Some(wt_core::worktree_target_key(&target)),
        ..Default::default()
    })
}

async fn start_install(ctx: &AppContext, target: &wt_core::WorktreeTarget) -> Result<()> {
    let config = wt_lifecycle::ServiceConfig::from_config(&ctx.config);
    let policy = wt_platform::install::InstallPolicy {
        command: config.install_command,
        shell: config.shell,
    };
    let Some(plan) = policy.resolve(Path::new(&target.path), false).await? else {
        return Ok(());
    };
    let mut command = vec![plan.spec.program.to_string_lossy().into_owned()];
    command.extend(
        plan.spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned()),
    );
    let mut config_selectors = std::collections::BTreeMap::new();
    if let Some(path) = &ctx.config.repository_config {
        config_selectors.insert("WT_REPO_CONFIG".into(), path.to_string_lossy().into_owned());
    }
    wt_app_core::actions::service(ctx)?
        .start(
            wt_actions::ActionRequest {
                action_key: wt_core::worktree_target_key(target),
                slug: target.slug().to_owned(),
                worktree_ref: Some(wt_core::WorktreeRef::Local {
                    slug: target.slug().to_owned(),
                }),
                action_id: "builtin:create-install".into(),
                action_name: "Install dependencies".into(),
                arg_history: None,
                prompt: plan.label,
                kind: wt_actions::ActionRunKind::Shell,
                command,
                cwd: Path::new(&target.path).to_path_buf(),
                affects: vec![wt_config::EffectTag::Git],
                issue_status: None,
                external: false,
                auto_fire_keys: Vec::new(),
                config_selectors,
            },
            &ctx.cancellation,
        )
        .await?;
    Ok(())
}

async fn set_status(
    ctx: &AppContext,
    key: &str,
    state: Option<String>,
    note: Option<String>,
    verify: Option<String>,
    seen: Option<wt_tui::Seen<Option<wt_tui::StatusClaim>>>,
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
            // Another writer (often the worktree's agent) may have claimed a
            // status since the menu opened. Never overwrite a claim unseen.
            // The note and owed check are carried into the new claim, so a
            // change to either is as unseen as a change of state.
            if let Some(wt_tui::Seen(seen)) = &seen {
                let current = previous.as_ref().map(|work| wt_tui::StatusClaim {
                    state: work.state.clone(),
                    note: work.note.clone(),
                    verify_after_merge: work.verify_after_merge.clone(),
                });
                if current != *seen {
                    return Err(wt_app_core::host_errors::stale(format!(
                        "status is now {}",
                        current.map_or("unset".into(), |claim| claim.state)
                    )));
                }
            }
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

async fn set_base(
    ctx: &AppContext,
    key: &str,
    base: Option<String>,
    seen: Option<wt_tui::Seen<Option<String>>>,
) -> Result<UiReply> {
    wt_ops::fork_base::set(ctx, key, base, seen.map(|seen| seen.0)).await?;
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

/// Changed and untracked files on the row, from the prepared board, so the
/// confirm can count what would be lost as TS did.
fn dirty_file_count(board: Option<&Board>, key: &str) -> Option<u32> {
    let row = board?.rows.iter().find(|row| row.key == key)?;
    let count = row.git.tracked_changes? + row.git.untracked_files.unwrap_or(0);
    (count > 0).then_some(count)
}

/// Force-remove confirm lines in the TS style: work that disappears reads
/// "<what> will be lost."; any other hazard is its own sentence.
fn removal_hazard_lines(hazards: &[String], dirty_files: Option<u32>) -> Vec<String> {
    // The lifecycle hazard does not count files; the board's facts do.
    let hazards = hazards
        .iter()
        .map(|hazard| match dirty_files {
            Some(count) if hazard == wt_lifecycle::UNCOMMITTED_HAZARD => {
                format!(
                    "{count} uncommitted change{}",
                    if count == 1 { "" } else { "s" }
                )
            }
            _ => hazard.clone(),
        })
        .collect::<Vec<_>>();
    let (lost, other): (Vec<_>, Vec<_>) = hazards.iter().partition(|hazard| {
        wt_lifecycle::is_lost_work_hazard(hazard) || hazard.contains(" uncommitted change")
    });
    let sentence = |text: &str| {
        let mut chars = text.chars();
        chars.next().map_or_else(String::new, |first| {
            format!("{}{}.", first.to_uppercase(), chars.as_str())
        })
    };
    let mut lines = Vec::new();
    if !lost.is_empty() {
        let joined = lost
            .iter()
            .map(|hazard| hazard.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(sentence(&format!("{joined} will be lost")));
    }
    lines.extend(other.into_iter().map(|hazard| sentence(hazard)));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_hazards_read_as_sentences() {
        assert_eq!(
            removal_hazard_lines(
                &[
                    "uncommitted changes".into(),
                    "1 unpushed commit".into(),
                    "post-merge verification still owed".into(),
                ],
                None,
            ),
            [
                "Uncommitted changes, 1 unpushed commit will be lost.",
                "Post-merge verification still owed.",
            ]
        );
        assert_eq!(
            removal_hazard_lines(&["3 unpushed commits".into()], None),
            ["3 unpushed commits will be lost."]
        );
        assert_eq!(
            removal_hazard_lines(&["uncommitted changes".into()], Some(1)),
            ["1 uncommitted change will be lost."]
        );
    }

    #[tokio::test]
    async fn human_status_preserves_verification_until_explicitly_cleared() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let row = wt_ops::commands::resolve::resolve_named_worktree(&fixture.ctx, "one")
            .await
            .unwrap();
        let key = wt_core::worktree_target_key(&row.target);
        set_status(
            &fixture.ctx,
            &key,
            Some("ready".into()),
            Some("checked locally".into()),
            Some("STEPS: check deployed version".into()),
            None,
        )
        .await
        .unwrap();
        // A menu that showed an older claim is refused without writing.
        let error = set_status(
            &fixture.ctx,
            &key,
            Some("working".into()),
            None,
            None,
            Some(wt_tui::Seen(Some(wt_tui::StatusClaim {
                state: "review".into(),
                note: None,
                verify_after_merge: None,
            }))),
        )
        .await
        .unwrap_err();
        assert!(wt_app_core::host_errors::error_reply(&error).stale);
        // Same state, but the owed check changed since: still unseen.
        let error = set_status(
            &fixture.ctx,
            &key,
            Some("working".into()),
            None,
            None,
            Some(wt_tui::Seen(Some(wt_tui::StatusClaim {
                state: "ready".into(),
                note: Some("checked locally".into()),
                verify_after_merge: Some("STEPS: older steps".into()),
            }))),
        )
        .await
        .unwrap_err();
        assert!(wt_app_core::host_errors::error_reply(&error).stale);
        set_status(
            &fixture.ctx,
            &key,
            Some("working".into()),
            None,
            None,
            Some(wt_tui::Seen(Some(wt_tui::StatusClaim {
                state: "ready".into(),
                note: Some("checked locally".into()),
                verify_after_merge: Some("STEPS: check deployed version".into()),
            }))),
        )
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
            None,
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
    async fn board_removal_is_witnessed_and_hazards_return_for_review() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let row = wt_ops::commands::resolve::resolve_named_worktree(&fixture.ctx, "one")
            .await
            .unwrap();
        let key = wt_core::worktree_target_key(&row.target);
        let path = row.target.path.clone();
        let head = run_git(
            &fixture.ctx,
            Path::new(&path),
            ["rev-parse", "--verify", "HEAD"],
        )
        .await
        .unwrap()
        .checked("git")
        .unwrap()
        .stdout_text()
        .trim()
        .to_owned();
        let seen = |head: &str| {
            Some(Box::new(wt_tui::Seen(wt_tui::RemovalSeen {
                path: path.clone(),
                branch: "feature/one".into(),
                head: head.into(),
            })))
        };
        let remove = |revision, seen| UiAction::Remove {
            key: key.clone(),
            force: false,
            revision,
            seen,
        };

        // A commit with neither a reviewed revision nor a board witness is refused.
        let error = execute(&fixture.ctx, remove(None, None), None, None)
            .await
            .unwrap_err();
        assert!(!wt_app_core::host_errors::error_reply(&error).stale);
        assert!(Path::new(&path).exists());

        // The head moved after the board showed the row: the menu reopens.
        let error = execute(&fixture.ctx, remove(None, seen("moved")), None, None)
            .await
            .unwrap_err();
        assert!(wt_app_core::host_errors::error_reply(&error).stale);
        assert!(Path::new(&path).exists());

        // Uncommitted work appeared after the clean board: nothing is removed,
        // and the exact hazards come back as a confirmation.
        std::fs::write(Path::new(&path).join("scratch.txt"), "work\n").unwrap();
        let reply = execute(&fixture.ctx, remove(None, seen(&head)), None, None)
            .await
            .unwrap();
        let Some(UiModal::Confirm {
            action:
                ConfirmAction::Remove {
                    force,
                    revision,
                    seen,
                    ..
                },
            lines,
            ..
        }) = reply.modal
        else {
            panic!("hazards must be reviewed before removal");
        };
        assert!(force);
        assert!(seen.is_none());
        assert_eq!(
            revision.expect("review carries the reviewed revision").head,
            head
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Uncommitted changes")),
            "{lines:?}"
        );
        assert!(Path::new(&path).exists());
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn failed_removal_dismissal_requires_its_witness_and_an_absent_checkout() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let listed = wt_ops::commands::resolve::resolve_named_worktree(&fixture.ctx, "one")
            .await
            .unwrap();
        let listed_key = wt_core::worktree_target_key(&listed.target);
        let listed_path = listed.target.path.clone();
        let gone = fixture._root.path().join("gone");
        let gone_path = gone.to_string_lossy().into_owned();
        let ctx = &fixture.ctx;
        let record = |key: String, path: String, branch: &'static str| async move {
            ctx.database
                .call(move |store| {
                    store.begin_removal(&key, "1-a", &path, branch)?;
                    store.finish_removal(&key, "1-a", Some("permission denied"))?;
                    Ok(())
                })
                .await
                .unwrap();
        };
        record("gone".into(), gone_path.clone(), "feature/gone").await;
        record(listed_key.clone(), listed_path.clone(), "feature/one").await;
        let dismiss =
            |key: &str, job: &str, path: &str, branch: &str| UiAction::DismissFailedRemoval {
                key: key.into(),
                seen: wt_tui::Seen(wt_tui::FailedRemovalSeen {
                    job_id: job.into(),
                    path: path.into(),
                    branch: branch.into(),
                }),
            };
        let removals = || async {
            fixture
                .ctx
                .database
                .call(|store| Ok(store.read_wt_state()?["removals"].clone()))
                .await
                .unwrap()
        };
        let refused = |action: UiAction| async {
            let error = execute(&fixture.ctx, action, None, None).await.unwrap_err();
            assert!(
                wt_app_core::host_errors::error_reply(&error).stale,
                "{error:#}"
            );
        };

        // Another job than the one the menu showed writes nothing.
        refused(dismiss("gone", "0-z", &gone_path, "feature/gone")).await;
        // A checkout path that exists again keeps the record for a retry.
        std::fs::create_dir(&gone).unwrap();
        refused(dismiss("gone", "1-a", &gone_path, "feature/gone")).await;
        std::fs::remove_dir(&gone).unwrap();
        // A checkout Git still lists is reviewed through normal removal.
        refused(dismiss(&listed_key, "1-a", &listed_path, "feature/one")).await;
        let state = removals().await;
        assert_eq!(state["gone"]["error"], "permission denied");
        assert_eq!(state[listed_key.as_str()]["error"], "permission denied");

        execute(
            &fixture.ctx,
            dismiss("gone", "1-a", &gone_path, "feature/gone"),
            None,
            None,
        )
        .await
        .unwrap();
        let state = removals().await;
        assert!(state["gone"].is_null());
        assert!(!state[listed_key.as_str()].is_null());
        assert!(Path::new(&listed_path).exists());
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn fork_picker_rejects_cycles_and_keeps_a_real_merge_base() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
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
        set_base(&fixture.ctx, &key("one"), Some("feature/two".into()), None)
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
            set_base(&fixture.ctx, &key("two"), Some("feature/one".into()), None)
                .await
                .unwrap_err()
                .to_string()
                .contains("not an eligible fork base")
        );
        assert!(
            set_base(&fixture.ctx, &key("one"), Some("missing".into()), None)
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
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let row = wt_ops::commands::resolve::resolve_named_worktree(&fixture.ctx, "one")
            .await
            .unwrap();
        execute(
            &fixture.ctx,
            UiAction::SetIssueOverride {
                key: wt_core::worktree_target_key(&row.target),
                issue_id: None,
            },
            None,
            None,
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
                None
            )
            .await
            .is_err()
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn toggles_refuse_a_state_the_user_did_not_see() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let row = wt_ops::commands::resolve::resolve_named_worktree(&fixture.ctx, "one")
            .await
            .unwrap();
        let key = wt_core::worktree_target_key(&row.target);
        let archive = |seen| UiAction::ToggleArchive {
            key: key.clone(),
            seen: Some(wt_tui::Seen(seen)),
        };
        let archived = |ctx: &AppContext| {
            let key = key.clone();
            let database = ctx.database.clone();
            async move {
                database
                    .call(move |store| Ok(store.read_archived_keys()?.contains(&key)))
                    .await
                    .unwrap()
            }
        };
        execute(&fixture.ctx, archive(false), None, None)
            .await
            .unwrap();
        assert!(archived(&fixture.ctx).await);
        // A second board that still showed it unarchived must not undo it.
        let error = execute(&fixture.ctx, archive(false), None, None)
            .await
            .unwrap_err();
        assert!(wt_app_core::host_errors::error_reply(&error).stale);
        assert!(archived(&fixture.ctx).await);
        assert!(
            fixture
                .ctx
                .database
                .call(|store| Ok(store.is_section_folded(wt_sources::board_layout::ARCHIVED)?))
                .await
                .unwrap(),
            "archiving folds the archive section in the same write"
        );
        execute(&fixture.ctx, archive(true), None, None)
            .await
            .unwrap();
        assert!(!archived(&fixture.ctx).await);

        let paused = |ctx: &AppContext| {
            let database = ctx.database.clone();
            async move {
                database
                    .call(|store| {
                        let state = store.read_wt_state()?;
                        Ok((
                            state["slugs"]["one"]["automationsPaused"].as_bool() == Some(true),
                            state["automationsPaused"].as_bool() == Some(true),
                        ))
                    })
                    .await
                    .unwrap()
            }
        };
        for key in [Some(key.clone()), None] {
            let toggle = |seen| UiAction::ToggleAutomations {
                key: key.clone(),
                seen: Some(wt_tui::Seen(seen)),
            };
            let read = |state: (bool, bool)| if key.is_some() { state.0 } else { state.1 };
            execute(&fixture.ctx, toggle(false), None, None)
                .await
                .unwrap();
            assert!(read(paused(&fixture.ctx).await));
            let error = execute(&fixture.ctx, toggle(false), None, None)
                .await
                .unwrap_err();
            assert!(wt_app_core::host_errors::error_reply(&error).stale);
            assert!(read(paused(&fixture.ctx).await), "a stale toggle resumed");
            execute(&fixture.ctx, toggle(true), None, None)
                .await
                .unwrap();
            assert!(!read(paused(&fixture.ctx).await));
        }
        fixture.close().await.unwrap();
    }

    #[test]
    fn remote_archive_requires_the_hosts_live_data() {
        use wt_runtime::{SourceSnapshot, merge_domains};
        let live = |domain| {
            SourceSnapshot::test_live(Some(()), SourceState::Ready, domain)
                .domains()
                .clone()
        };
        let mut board = Board::default();
        assert!(require_live_host(Some(&board), Some("devbox")).is_err());
        // Restored host cache: last known rows, never authority for a write.
        let mut restored = SourceSnapshot::test_restored((), Domain::Host, 1)
            .domains()
            .clone();
        merge_domains(&mut restored, &live(Domain::Git));
        board.host_domains.insert("devbox".into(), restored);
        assert!(require_live_host(Some(&board), Some("devbox")).is_err());
        // Connected, but the host itself still shows its restored inventory.
        let mut connected = live(Domain::Host);
        merge_domains(
            &mut connected,
            SourceSnapshot::test_restored((), Domain::Git, 1).domains(),
        );
        board.host_domains.insert("devbox".into(), connected);
        assert!(require_live_host(Some(&board), Some("devbox")).is_err());
        let mut current = live(Domain::Host);
        merge_domains(&mut current, &live(Domain::Git));
        board.host_domains.insert("devbox".into(), current);
        require_live_host(Some(&board), Some("devbox")).unwrap();
        assert!(require_live_host(Some(&board), None).is_err());
    }
}
