use anyhow::Result;
use clap::Args;
use futures_util::{StreamExt, stream};
use serde::Serialize;
use std::path::Path;
use wt_harness::stale_harness_shims;
use wt_platform::process::CommandSpec;

use wt_app_core::context::AppContext;
use wt_ops::commands::{
    diagnostics::{Check, CheckStatus, display_status, worst},
    resolve::worktree_at_cwd,
};

#[derive(Debug, Clone, Args, Default)]
pub struct DoctorArgs {
    pub target: Option<String>,
    #[arg(short, long)]
    pub all: bool,
    #[arg(long)]
    pub json: bool,
    /// Remove what the daily maintenance pass would, now.
    #[arg(long, conflicts_with_all = ["target", "all", "json"])]
    pub prune: bool,
}

#[derive(Serialize)]
struct Report {
    slug: String,
    path: String,
    status: CheckStatus,
    checks: Vec<Check>,
}

pub async fn run(ctx: &AppContext, args: &DoctorArgs) -> Result<i32> {
    if args.prune {
        let Some(gone) = wt_ops::maintenance::run(
            ctx,
            install_paths(ctx).as_ref(),
            wt_ops::maintenance::Scope::ASKED,
        )
        .await
        else {
            println!("another maintenance pass is running for this repository; nothing done");
            return Ok(0);
        };
        if gone.is_empty() {
            println!("nothing to remove");
        } else {
            println!("removed {}", gone.counts().join(", "));
        }
        if !gone.unknown.is_empty() {
            println!("not judged (could not read): {}", gone.unknown.join(", "));
        }
        for check in &gone.redated_checks {
            println!(
                "{}: {} check dated from history, due {:.1} h earlier than recorded",
                check.slug,
                check.stage.as_str(),
                (check.from_ms - check.to_ms) as f64 / 3_600_000.0
            );
        }
        return Ok(0);
    }
    if args.all && args.target.is_some() {
        eprintln!("a target cannot be combined with --all");
        return Ok(2);
    }
    let records = ctx.repository.inventory(&ctx.cancellation).await?;
    let selected = select_rows(records, args, &ctx.cwd)?;
    let branches = selected
        .iter()
        .filter(|record| !record.target.branch.is_empty())
        .map(|record| record.target.branch.clone())
        .collect::<Vec<_>>();
    // The PR read overlaps the local ones. `--json` has nowhere to say that
    // its data is a held snapshot, and its `PR / CI` status sets the exit
    // code, so it always reads GitHub itself, as `wt ls --json` reads Git.
    let (github, local) = tokio::join!(
        async {
            if args.json {
                (live_pull_requests(ctx, &branches).await, None)
            } else {
                super::fleet::pull_requests(ctx, &branches).await
            }
        },
        local_reads(ctx, &selected, !args.json),
    );
    let (prs, pr_error, pr_held) = match github {
        (Ok(data), snapshot) => (
            Some(data.prs),
            None,
            snapshot.and_then(|snapshot| snapshot.held),
        ),
        (Err(error), _) => (None, Some(error.to_string()), None),
    };
    let trunk = format!("origin/{}", ctx.config.branch.base);
    let mut reports = Vec::new();
    for (row, local) in selected.iter().zip(local?) {
        let pr = prs.as_ref().and_then(|prs| prs.get(&row.target.branch));
        let mut checks = vec![local.working_tree];
        checks.push(sync_check(
            &local.sync,
            &trunk,
            merged_at(pr, row.head_sha.as_deref()),
        ));
        checks.extend(local.checkout);
        if !row.target.branch.is_empty() {
            checks.extend(local.branch);
            match pr {
                Some(pr) => {
                    let status =
                        if !pr.failed_checks.is_empty() || pr.checks == wt_github::PrChecks::Fail {
                            CheckStatus::Err
                        } else if pr.checks == wt_github::PrChecks::Pending {
                            CheckStatus::Warn
                        } else {
                            CheckStatus::Ok
                        };
                    let state = if pr.is_draft && pr.state == "OPEN" {
                        "open draft"
                    } else {
                        pr.state.as_str()
                    };
                    let checks_msg = match pr.checks {
                        wt_github::PrChecks::Fail => {
                            format!("{} CI check(s) failing", pr.failed_checks.len())
                        }
                        wt_github::PrChecks::Pending => "CI pending".into(),
                        _ => "CI clear".into(),
                    };
                    checks.push(check(
                        "PR / CI",
                        status,
                        format!("#{} {state}; {checks_msg}", pr.number),
                    ));
                }
                None if pr_error.is_some() => checks.push(check(
                    "PR / CI",
                    CheckStatus::Warn,
                    format!(
                        "GitHub unavailable: {}",
                        pr_error.as_deref().unwrap_or_default()
                    ),
                )),
                None => checks.push(check(
                    "PR / CI",
                    CheckStatus::Info,
                    "no pull request found".into(),
                )),
            }
        }
        reports.push(Report {
            slug: row.target.slug().to_owned(),
            path: row.target.path.clone(),
            status: worst(&checks),
            checks,
        });
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        let mut pnpm_rows = 0;
        for report in &reports {
            println!("{}  {}", report.slug, display_status(report.status));
            for check in &report.checks {
                // The same sentence for every row: said once, below.
                if check.name == PNPM_TREE {
                    pnpm_rows += 1;
                    continue;
                }
                println!(
                    "  {:<16} {:<5} {}",
                    check.name,
                    display_status(check.status),
                    check.message
                );
            }
        }
        if reports.is_empty() {
            println!("no worktrees found");
        }
        if pnpm_rows > 0 {
            println!("info {PNPM_TREE} ({pnpm_rows} worktree(s)): {PNPM_TREE_NOTE}");
        }
        if let Some(held) = pr_held {
            eprintln!("{held}");
        }
    }
    Ok(
        if reports.iter().any(|r| matches!(r.status, CheckStatus::Err)) {
            1
        } else {
            0
        },
    )
}

/// What only the checkout can say of a row: everything but its pull request.
struct LocalChecks {
    working_tree: Check,
    sync: Sync,
    /// Dependencies, the operation lock, the stage pin and automation runs.
    checkout: Vec<Check>,
    /// The checks a row with a branch gets: `gh merge base` and `merged`.
    branch: Vec<Check>,
}

/// Rows read at once. Each is a handful of Git calls, and the process runner
/// bounds the subprocesses themselves.
const ROW_READS: usize = 8;

const PNPM_TREE: &str = "pnpm tree";
const PNPM_TREE_NOTE: &str = "native doctor checks install presence; it does not scan pnpm's isolated virtual store for unmanaged top-level packages";

/// The banners, then every row's local checks, in the rows' order.
async fn local_reads(
    ctx: &AppContext,
    rows: &[wt_vcs::WorktreeRecord],
    banners: bool,
) -> Result<Vec<LocalChecks>> {
    // The budget probe is several GitHub calls and the leftovers a few
    // directory scans; neither waits for, or holds up, the rows.
    let extras = async {
        if !banners {
            return None;
        }
        let install = install_paths(ctx);
        Some(tokio::join!(
            super::events::token_budget(ctx),
            leftover_lines(ctx, install.as_ref()),
        ))
    };
    let (extras, rows) = tokio::join!(extras, row_reads(ctx, rows, banners));
    if let Some((budget, leftovers)) = extras {
        for (label, text) in super::events::budget_lines(&budget, super::events::now_ms()) {
            println!("github {label:<10} {text}");
        }
        for line in leftovers {
            println!("{line}");
        }
    }
    rows
}

async fn row_reads(
    ctx: &AppContext,
    rows: &[wt_vcs::WorktreeRecord],
    banners: bool,
) -> Result<Vec<LocalChecks>> {
    if banners {
        print_banners(ctx).await?;
    }
    // A failed read says nothing either way, and is not this command's to
    // report: the engine logs it.
    let unknown_runs = wt_app_core::automation_engine::unknown_outcomes(
        &ctx.config.paths.cache_root.join("automations.json"),
    )
    .await
    .unwrap_or_default();
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let (state, unknown_runs) = (&state, &unknown_runs);
    Ok(stream::iter(rows)
        .map(|row| async move {
            let automations = unknown_runs
                .iter()
                .filter(|(slug, _)| slug == row.target.slug())
                .map(|(_, message)| check("automation", CheckStatus::Warn, message.clone()));
            let mut local = local_checks(ctx, row, state).await;
            local.checkout.extend(automations);
            local
        })
        .buffered(ROW_READS)
        .collect()
        .await)
}

async fn local_checks(
    ctx: &AppContext,
    row: &wt_vcs::WorktreeRecord,
    state: &serde_json::Value,
) -> LocalChecks {
    let path = std::path::PathBuf::from(&row.target.path);
    let slug = row.target.slug();
    let trunk = format!("origin/{}", ctx.config.branch.base);
    let has_branch = !row.target.branch.is_empty();
    let key = format!("branch.{}.gh-merge-base", row.target.branch);
    let (status, sync, lock, merge_base, merged) = tokio::join!(
        ctx.repository.status(row, &ctx.cancellation),
        read_sync(ctx, &path, &trunk),
        lock_check(ctx, slug),
        async {
            if !has_branch {
                return None;
            }
            ctx.processes
                .run(
                    CommandSpec::new("git")
                        .args(["config", key.as_str()])
                        .cwd(&path),
                    &ctx.cancellation,
                )
                .await
                .ok()
                .filter(|out| out.status.success())
                .map(|out| out.stdout_text().trim().to_owned())
                .filter(|value| !value.is_empty())
        },
        async {
            if has_branch {
                Some(check_merged(ctx, &path, &trunk).await)
            } else {
                None
            }
        },
    );
    let working_tree = match status {
        Ok(status) if !status.dirty => check("working tree", CheckStatus::Ok, "clean".into()),
        Ok(status) => check(
            "working tree",
            CheckStatus::Warn,
            format!(
                "{} tracked change(s), {} untracked file(s)",
                status.tracked_changes, status.untracked_files
            ),
        ),
        Err(error) => check("working tree", CheckStatus::Err, error.to_string()),
    };
    let mut checkout = Vec::new();
    if path.join("package.json").exists() {
        let installed = path.join("node_modules").is_dir();
        checkout.push(check(
            "dependencies",
            if installed {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            },
            if installed {
                "node_modules present".into()
            } else {
                "node_modules missing".into()
            },
        ));
        if path.join("pnpm-lock.yaml").exists() && installed {
            checkout.push(check(PNPM_TREE, CheckStatus::Info, PNPM_TREE_NOTE.into()));
        }
    }
    checkout.push(lock);
    if ctx.config.sst.is_some() {
        checkout.extend(check_stage(ctx, &path, slug));
    }
    let mut branch = Vec::new();
    if has_branch {
        let expected = state["slugs"][slug]["baseBranch"]
            .as_str()
            .unwrap_or(&ctx.config.branch.base);
        branch.push(check(
            "gh merge base",
            if merge_base.as_deref() == Some(expected) {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            },
            merge_base
                .map(|actual| format!("set to {actual}, expected {expected}"))
                .unwrap_or_else(|| {
                    format!(
                        "unset; bare `gh pr create` targets the repo default; fix: git -C {} config {} {}",
                        path.display(), key, expected
                    )
                }),
        ));
        branch.extend(merged);
    }
    LocalChecks {
        working_tree,
        sync,
        checkout,
        branch,
    }
}

/// A held lock is an operation running now, which is not a fault: it is
/// reported with whoever holds it. The lock file itself names nobody.
async fn lock_check(ctx: &AppContext, slug: &str) -> Check {
    match wt_ops::commands::diagnostics::operation_lock(ctx, slug).await {
        Ok(Some(lock)) => {
            let path = ctx.config.paths.lock_dir.join(format!("{slug}.lock"));
            let holder = match lock.pid {
                Some(pid) => Some(pid),
                None => lock_holder(ctx, &path).await,
            };
            let holder = match holder {
                Some(pid) => match process_command(ctx, pid).await {
                    Some(command) => format!("held by pid {pid} ({command})"),
                    None => format!("held by pid {pid}"),
                },
                None => "held by a running operation whose process could not be found".into(),
            };
            let phase = lock.phase.as_deref().filter(|phase| !phase.is_empty());
            let message = match (lock.op.as_deref(), phase) {
                (Some(op), Some(phase)) => format!("{op}: {phase}, {holder}"),
                (Some(label), None) | (None, Some(label)) => format!("{label}, {holder}"),
                (None, None) => holder,
            };
            check("operation lock", CheckStatus::Info, message)
        }
        Ok(None) => check("operation lock", CheckStatus::Ok, "none".into()),
        Err(error) => check(
            "operation lock",
            CheckStatus::Info,
            format!("unable to inspect lock: {error}"),
        ),
    }
}

/// The process that has the lock file open, other than this one. `None`
/// when `lsof` is missing or names nobody.
async fn lock_holder(ctx: &AppContext, path: &Path) -> Option<u32> {
    let output = ctx
        .processes
        .run(
            CommandSpec::new("lsof").args([
                std::ffi::OsStr::new("-Fp"),
                std::ffi::OsStr::new("--"),
                path.as_os_str(),
            ]),
            &ctx.cancellation,
        )
        .await
        .ok()?;
    first_other_pid(&output.stdout_text(), std::process::id())
}

/// The first process of `lsof -Fp` output that is not `own`.
fn first_other_pid(lsof: &str, own: u32) -> Option<u32> {
    lsof.lines()
        .filter_map(|line| line.strip_prefix('p')?.parse::<u32>().ok())
        .find(|pid| *pid != own)
}

async fn process_command(ctx: &AppContext, pid: u32) -> Option<String> {
    let output = ctx
        .processes
        .run(
            CommandSpec::new("ps").args(["-o", "command=", "-p", &pid.to_string()]),
            &ctx.cancellation,
        )
        .await
        .ok()
        .filter(|output| output.status.success())?;
    let command = output.stdout_text().trim().to_owned();
    (!command.is_empty()).then(|| command.chars().take(120).collect())
}

/// Whether the pull request merged at the commit the checkout is on. A
/// branch that gained commits after its merge has work still to land.
fn merged_at(pr: Option<&wt_github::PullRequest>, head: Option<&str>) -> bool {
    pr.is_some_and(|pr| {
        pr.state == "MERGED" && head.is_some() && pr.head_ref_oid.as_deref() == head
    })
}

async fn live_pull_requests(
    ctx: &AppContext,
    branches: &[String],
) -> Result<wt_github::GithubData, wt_github::GithubError> {
    wt_github::GithubClient::new(
        ctx.processes.clone(),
        ctx.config.paths.main_clone.clone(),
        wt_github::GithubOptions::from_config(&ctx.config, true),
    )
    .fetch_worktrees(branches, &ctx.cancellation)
    .await
}

/// How far a checkout is ahead of and behind its trunk, or why not known.
enum Sync {
    Counts { ahead: u64, behind: u64 },
    Unknown(String),
}

async fn read_sync(ctx: &AppContext, path: &Path, trunk: &str) -> Sync {
    // The symmetric difference counts each side's own commits. Two separate
    // revisions would count everything reachable from either.
    let range = format!("{trunk}...HEAD");
    let compare = ctx
        .processes
        .run(
            CommandSpec::new("git")
                .args(["rev-list", "--left-right", "--count", range.as_str()])
                .cwd(path),
            &ctx.cancellation,
        )
        .await;
    match compare {
        Ok(output) if output.status.success() => {
            let nums = output
                .stdout_text()
                .split_whitespace()
                .filter_map(|n| n.parse::<u64>().ok())
                .collect::<Vec<_>>();
            Sync::Counts {
                behind: nums.first().copied().unwrap_or(0),
                ahead: nums.get(1).copied().unwrap_or(0),
            }
        }
        Ok(_) => Sync::Unknown(format!("cannot compare with {trunk}")),
        Err(error) => Sync::Unknown(format!("cannot compare with {trunk}: {error}")),
    }
}

/// Being behind the trunk matters while the work is still to merge. Once its
/// pull request has merged there is nothing left to bring up to date.
fn sync_check(sync: &Sync, trunk: &str, merged: bool) -> Check {
    // `merged` is [`merged_at`]: the checkout's own HEAD is what merged.
    match sync {
        Sync::Counts {
            ahead: 0,
            behind: 0,
        } => check("sync", CheckStatus::Ok, "up to date".into()),
        Sync::Counts { ahead, behind } if merged && *behind > 0 => check(
            "sync",
            CheckStatus::Info,
            format!("{ahead} ahead, {behind} behind {trunk}; merged, so nothing to update"),
        ),
        Sync::Counts { ahead, behind } => check(
            "sync",
            if *behind == 0 {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            },
            format!("{ahead} ahead, {behind} behind {trunk}"),
        ),
        Sync::Unknown(why) => check("sync", CheckStatus::Warn, why.clone()),
    }
}

async fn print_banners(ctx: &AppContext) -> Result<()> {
    let main = &ctx.config.paths.main_clone;
    let main_branch = ctx
        .processes
        .run(
            CommandSpec::new("git")
                .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
                .cwd(main),
            &ctx.cancellation,
        )
        .await?;
    if !main_branch.status.success() {
        println!(
            "warning main clone is detached; should be on {}",
            ctx.config.branch.base
        );
    } else if main_branch.stdout_text().trim() != ctx.config.branch.base {
        println!(
            "warning main clone is on {:?}; should be on {}. Move that work into a worktree (`wt new {}`) and `git -C {} checkout {}`.",
            main_branch.stdout_text().trim(),
            ctx.config.branch.base,
            main_branch.stdout_text().trim(),
            main.display(),
            ctx.config.branch.base
        );
    }
    for warning in &ctx.config.warnings {
        println!("warning config: {warning}");
    }
    let messaging = claude_shim_check(
        &ctx.config.paths.cache_root,
        std::env::var("WT_INSPECT").is_ok_and(|value| value.eq_ignore_ascii_case("off")),
    );
    if !matches!(messaging.status, CheckStatus::Ok) {
        println!(
            "{} {}: {}",
            display_status(messaging.status),
            messaging.name,
            messaging.message
        );
    }
    if let Some(expected) = expected_wt_path(ctx) {
        match find_path_wt() {
            Some((candidate, resolved))
                if resolved != expected.canonicalize().unwrap_or(expected.clone()) =>
            {
                println!("{}", path_wt_line(&candidate, &resolved, &expected));
            }
            Some(_) => {}
            None => println!(
                "warning `wt` is not on PATH; scripts cannot call it. Fix: ln -s {} {}/wt",
                expected.display(),
                ctx.home.join(".local/bin").display()
            ),
        }
    }
    Ok(())
}

/// The install this process belongs to, when there is one on disk.
fn install_paths(ctx: &AppContext) -> Option<wt_update::InstallPaths> {
    let root = std::env::var_os(wt_launcher::INSTALL_ROOT_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ctx.home.join(".local/share/wt"));
    wt_update::InstallPaths::new(root)
        .ok()
        .filter(|paths| paths.state_file().is_file())
}

fn size(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    if bytes >= 1024 * MB {
        format!("{:.1} GB", bytes as f64 / (1024 * MB) as f64)
    } else if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// What the maintenance pass would remove today, and the files from before
/// the native rewrite that it never touches.
async fn leftover_lines(
    ctx: &AppContext,
    install: Option<&wt_update::InstallPaths>,
) -> Vec<String> {
    // What `--prune` would remove: the daily pass and the slug records.
    let found = wt_ops::maintenance::survey(ctx, install, wt_ops::maintenance::Scope::ASKED).await;
    let cache_root = ctx.config.paths.cache_root.clone();
    let legacy_install = install.cloned();
    let versions = found.versions.clone();
    let (version_bytes, (legacy, legacy_bytes)) = tokio::task::spawn_blocking(move || {
        let roots: Vec<&Path> = std::iter::once(cache_root.as_path())
            .chain(cache_root.parent())
            .collect();
        (
            versions
                .iter()
                .map(|path| wt_ops::maintenance::tree_bytes(path))
                .sum::<u64>(),
            wt_ops::maintenance::legacy_files(&roots, legacy_install.as_ref()),
        )
    })
    .await
    .unwrap_or_default();
    let mut lines = Vec::new();
    if !found.is_empty() {
        let mut counts = found.counts();
        if version_bytes > 0 {
            counts[0] = format!("{} ({})", counts[0], size(version_bytes));
        }
        let maintenance = ctx.config.maintenance;
        let daily = match (maintenance.enabled, maintenance.state_records) {
            (false, _) => "; the daily pass is off (`[maintenance] enabled`)",
            (true, true) => ", which a running wt also does once a day",
            (true, false) if found.slug_records.is_empty() => {
                ", which a running wt also does once a day"
            }
            (true, false) => ", which a running wt also does once a day, the slug records excepted",
        };
        lines.push(format!(
            "info leftovers: {} can go; `wt doctor --prune` removes them{daily}",
            counts.join(", ")
        ));
    }
    if !found.unknown.is_empty() {
        lines.push(format!(
            "info leftovers not judged (could not read): {}",
            found.unknown.join(", ")
        ));
    }
    if legacy > 0 {
        lines.push(format!(
            "info {legacy} file(s) from before the native rewrite ({}): old logs, state backups and the migration archive; wt keeps them",
            size(legacy_bytes)
        ));
    }
    lines
}

fn claude_shim_check(cache_root: &Path, inspector_disabled: bool) -> Check {
    let stale = stale_harness_shims(cache_root);
    if stale.is_empty() {
        return check(
            "Claude messaging",
            CheckStatus::Ok,
            "no stale harness shims".into(),
        );
    }
    let shim_dir = cache_root.join("shims");
    if inspector_disabled {
        return check(
            "Claude messaging",
            CheckStatus::Info,
            format!(
                "WT_INSPECT=off intentionally disables prompt injection; stale {} shim(s) remain in {} and should be removed before re-enabling it",
                stale.join(", "),
                shim_dir.display()
            ),
        );
    }
    check(
        "Claude messaging",
        CheckStatus::Warn,
        format!(
            "stale {} harness shim(s) in {}; these can interfere with managed harness launches and may explain missing Claude inspector sockets. Remove them, start a new wt-managed session, then run `wt claude selftest`",
            stale.join(", "),
            shim_dir.display()
        ),
    )
}

fn expected_wt_path(ctx: &AppContext) -> Option<std::path::PathBuf> {
    let root = std::env::var_os(wt_launcher::INSTALL_ROOT_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ctx.home.join(".local/share/wt"));
    let candidate = root.join("bin/wt");
    candidate.is_file().then_some(candidate)
}

/// What to say of a `wt` on PATH that is not the installed launcher. One
/// inside the install's own `versions` directory is the build a session was
/// started with, pinned there by design (docs/updates.md): not a fault.
fn path_wt_line(candidate: &Path, resolved: &Path, expected: &Path) -> String {
    let versions = expected
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("versions"));
    let pinned = versions
        .and_then(|versions| resolved.strip_prefix(versions).ok())
        .and_then(|inside| inside.components().next())
        .map(|version| version.as_os_str().to_string_lossy().into_owned());
    match pinned {
        Some(version) => format!(
            "info `wt` on PATH is the build this session started with ({version}), pinned by design; {} is the installed one",
            expected.display()
        ),
        None => format!(
            "warning `wt` at {} resolves to {}, expected {}",
            candidate.display(),
            resolved.display(),
            expected.display()
        ),
    }
}

fn find_path_wt() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    for directory in std::env::split_paths(&std::env::var_os("PATH")?) {
        let candidate = directory.join("wt");
        if !candidate.is_file() {
            continue;
        }
        let resolved = candidate.canonicalize().ok()?;
        return Some((candidate, resolved));
    }
    None
}

fn check(name: &str, status: CheckStatus, message: String) -> Check {
    Check {
        name: name.into(),
        status,
        message,
    }
}

fn check_stage(ctx: &AppContext, worktree: &Path, slug: &str) -> Vec<Check> {
    let path = worktree.join(".sst/stage");
    let actual = match std::fs::read_to_string(&path) {
        Ok(value) => value.trim().to_owned(),
        Err(_) => {
            return vec![
                check(
                    "sst stage",
                    CheckStatus::Warn,
                    "no readable .sst/stage pin".into(),
                ),
                check("sst deploy", CheckStatus::Info, "not deployed".into()),
            ];
        }
    };
    let pattern = regex::RegexBuilder::new(&ctx.config.branch.id_pattern)
        .case_insensitive(true)
        .build()
        .ok();
    let expected = wt_core::stage_name(slug, &ctx.config.stage.prefix, pattern.as_ref());
    let stage_check = check(
        "sst stage",
        if actual == expected {
            CheckStatus::Ok
        } else {
            CheckStatus::Warn
        },
        if actual == expected {
            format!("pinned to {actual}")
        } else {
            format!("pinned to {actual}; expected {expected}")
        },
    );
    let safe = !ctx.config.stage.prefix.is_empty() && actual.starts_with(&ctx.config.stage.prefix);
    let outputs = worktree.join(".sst/outputs.json");
    let deployed =
        safe && std::fs::read_to_string(outputs).is_ok_and(|contents| contents.contains(&actual));
    vec![
        stage_check,
        check(
            "sst deploy",
            CheckStatus::Info,
            if deployed {
                "deployed (outputs reference pinned stage)".into()
            } else {
                "not deployed".into()
            },
        ),
    ]
}

/// An explicit target, else the linked worktree containing cwd, else every
/// linked worktree. The main clone is never a diagnosis subject.
fn select_rows(
    records: Vec<wt_vcs::WorktreeRecord>,
    args: &DoctorArgs,
    cwd: &Path,
) -> Result<Vec<wt_vcs::WorktreeRecord>> {
    if args.all {
        return Ok(records.into_iter().filter(|r| !r.is_main).collect());
    }
    if let Some(target) = &args.target {
        return Ok(vec![wt_ops::commands::resolve::resolve_from_inventory(
            &records, target, true,
        )?]);
    }
    if let Some(row) = worktree_at_cwd(&records, cwd).filter(|row| !row.is_main) {
        return Ok(vec![row.clone()]);
    }
    Ok(records.into_iter().filter(|r| !r.is_main).collect())
}

async fn check_merged(ctx: &AppContext, worktree: &Path, trunk: &str) -> Check {
    let range = format!("{trunk}..HEAD");
    let count = ctx
        .processes
        .run(
            CommandSpec::new("git")
                .args(["rev-list", "--count", range.as_str()])
                .cwd(worktree),
            &ctx.cancellation,
        )
        .await;
    match count {
        // A branch with commits beyond trunk can never be an ancestor of it,
        // so the count alone decides; squash landing is the PR check's job.
        Ok(out) if out.status.success() => match out.stdout_text().trim().parse::<u64>() {
            Ok(0) => check("merged", CheckStatus::Ok, "no commits beyond trunk".into()),
            Ok(_) => check(
                "merged",
                CheckStatus::Ok,
                format!("not contained in {trunk}"),
            ),
            Err(_) => check(
                "merged",
                CheckStatus::Warn,
                format!("cannot compare with {trunk}: unreadable commit count"),
            ),
        },
        Ok(out) => check(
            "merged",
            CheckStatus::Warn,
            format!("cannot compare with {trunk}: {}", out.stderr_text().trim()),
        ),
        Err(error) => check(
            "merged",
            CheckStatus::Warn,
            format!("cannot compare with {trunk}: {error}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use wt_app_core::test_support::CommandFixture;

    #[tokio::test]
    async fn leftovers_and_legacy_files_are_each_reported_once() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        assert!(leftover_lines(ctx, None).await.is_empty());
        let week = std::time::Duration::from_secs(8 * 24 * 60 * 60);
        let dev = ctx.config.paths.cache_root.join("dev");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::create_dir_all(&ctx.config.paths.lock_dir).unwrap();
        let old = [
            dev.join("gone.state"),
            dev.join("also-gone.state"),
            dev.join("one.state"),
            ctx.config.paths.lock_dir.join("gone.lock"),
        ];
        for path in &old {
            std::fs::File::create(path)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - week)
                .unwrap();
        }
        std::fs::write(
            ctx.config.paths.cache_root.join("state.json.bak-v3"),
            vec![b'x'; 2048],
        )
        .unwrap();
        let lines = leftover_lines(ctx, None).await;
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].starts_with("info leftovers: 2 dev-server files can go;"),
            "{lines:?}"
        );
        assert!(lines[1].starts_with("info 1 file(s) from before the native rewrite (2 KB)"));
        assert!(old.iter().all(|path| path.exists()));
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn sst_health_reports_owned_pins_and_outputs_as_deployed() {
        let fixture = CommandFixture::new().await.unwrap();
        let worktree = fixture.ctx.cwd.as_path();
        let slug = "one";
        let digest = Sha256::digest(slug.as_bytes());
        let stage = format!("stage-{}", &format!("{digest:x}")[..10]);
        std::fs::create_dir_all(worktree.join(".sst")).unwrap();
        std::fs::write(worktree.join(".sst/stage"), &stage).unwrap();
        std::fs::write(
            worktree.join(".sst/outputs.json"),
            format!("{{\"url\":\"https://{stage}.example.test\"}}"),
        )
        .unwrap();
        let checks = check_stage(&fixture.ctx, worktree, slug);
        assert!(matches!(checks[0].status, CheckStatus::Ok));
        assert!(checks[1].message.starts_with("deployed"));
        std::fs::write(worktree.join(".sst/outputs.json"), "{}").unwrap();
        assert_eq!(
            check_stage(&fixture.ctx, worktree, slug)[1].message,
            "not deployed"
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn sync_counts_each_side_of_the_fork_not_all_of_history() {
        let fixture = CommandFixture::new().await.unwrap();
        let worktree = fixture.ctx.cwd.clone();
        let main = fixture.ctx.config.paths.main_clone.clone();
        let git = |cwd: &Path, args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
        };
        // History both sides share, then one commit only the trunk has and
        // two only the worktree has.
        for message in ["shared one", "shared two"] {
            git(&main, &["commit", "--allow-empty", "-m", message]);
        }
        git(&worktree, &["reset", "--hard", "main"]);
        git(&main, &["update-ref", "refs/remotes/origin/main", "main"]);
        let synced = sync_check(
            &read_sync(&fixture.ctx, &worktree, "origin/main").await,
            "origin/main",
            false,
        );
        assert_eq!(synced.message, "up to date");
        assert!(matches!(synced.status, CheckStatus::Ok));

        git(&main, &["commit", "--allow-empty", "-m", "trunk only"]);
        git(&main, &["update-ref", "refs/remotes/origin/main", "main"]);
        for message in ["mine one", "mine two"] {
            git(&worktree, &["commit", "--allow-empty", "-m", message]);
        }
        let diverged = sync_check(
            &read_sync(&fixture.ctx, &worktree, "origin/main").await,
            "origin/main",
            false,
        );
        assert_eq!(diverged.message, "2 ahead, 1 behind origin/main");
        assert!(matches!(diverged.status, CheckStatus::Warn));
        // Once the pull request has merged, being behind is not a warning.
        let merged = sync_check(
            &read_sync(&fixture.ctx, &worktree, "origin/main").await,
            "origin/main",
            true,
        );
        assert!(matches!(merged.status, CheckStatus::Info));
        assert!(
            merged
                .message
                .starts_with("2 ahead, 1 behind origin/main; merged")
        );

        let unknown = sync_check(
            &read_sync(&fixture.ctx, &worktree, "origin/absent").await,
            "origin/absent",
            false,
        );
        assert_eq!(unknown.message, "cannot compare with origin/absent");
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn main_clone_cwd_selects_every_linked_worktree_not_the_main_clone() {
        let fixture = CommandFixture::new().await.unwrap();
        let records = fixture
            .ctx
            .repository
            .inventory(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let main = fixture.ctx.config.paths.main_clone.clone();
        let selected = select_rows(records.clone(), &DoctorArgs::default(), &main).unwrap();
        let mut slugs = selected
            .iter()
            .map(|row| row.target.slug().to_owned())
            .collect::<Vec<_>>();
        slugs.sort();
        assert_eq!(slugs, ["one", "two"]);
        let selected = select_rows(records, &DoctorArgs::default(), &fixture.ctx.cwd).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].target.slug(), "one");
        fixture.close().await.unwrap();
    }

    #[test]
    fn behind_stops_mattering_only_when_this_head_is_what_merged() {
        let pr = |state: &str, head: &str| -> wt_github::PullRequest {
            serde_json::from_value(serde_json::json!({
                "number": 42, "url": "https://example.invalid/pull/42",
                "headRefName": "feature/one", "headRefOid": head,
                "baseRefName": "main", "mergeCommitOid": "f".repeat(40),
                "title": "Feature", "isDraft": false, "state": state,
                "checks": "pass", "failedChecks": [], "review": "none",
                "reviewRequests": 0, "requestedReviewers": [], "suggestedReviewers": [],
                "comments": [], "unresolvedThreads": 0, "unresolvedThreadsTotal": 0
            }))
            .unwrap()
        };
        assert!(merged_at(Some(&pr("MERGED", "abc")), Some("abc")));
        // The branch was reused: commits after the merge still have to land.
        assert!(!merged_at(Some(&pr("MERGED", "abc")), Some("def")));
        assert!(!merged_at(Some(&pr("OPEN", "abc")), Some("abc")));
        assert!(!merged_at(Some(&pr("MERGED", "abc")), None));
        assert!(!merged_at(None, Some("abc")));
    }

    #[test]
    fn a_session_pinned_wt_is_not_reported_as_a_broken_path() {
        let expected = Path::new("/home/me/.local/share/wt/bin/wt");
        let pinned = Path::new("/home/me/.local/share/wt/versions/v0.1.76-abc-darwin/bin/wt");
        let line = path_wt_line(pinned, pinned, expected);
        assert!(line.starts_with(
            "info `wt` on PATH is the build this session started with (v0.1.76-abc-darwin), pinned"
        ));
        assert!(line.contains("/home/me/.local/share/wt/bin/wt"));
        let stray = Path::new("/usr/local/bin/wt");
        assert!(
            path_wt_line(stray, stray, expected).starts_with("warning `wt` at /usr/local/bin/wt")
        );
    }

    #[tokio::test]
    async fn a_held_lock_names_its_holder_and_does_not_warn() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        assert_eq!(lock_check(ctx, "one").await.message, "none");
        let lock_dir = ctx.config.paths.lock_dir.clone();
        // Another process holds the lock, as a running `wt new` does.
        std::fs::create_dir_all(&lock_dir).unwrap();
        let mut holder = std::process::Command::new("perl")
            .args([
                "-e",
                "use Fcntl qw(:flock); open(my $f, '>>', $ARGV[0]) or die; flock($f, LOCK_EX) or die; $| = 1; print \"held\\n\"; sleep 60;",
            ])
            .arg(lock_dir.join("one.lock"))
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(holder.stdout.take().unwrap()),
            &mut ready,
        )
        .unwrap();
        let held = lock_check(ctx, "one").await;
        holder.kill().unwrap();
        holder.wait().unwrap();
        assert!(matches!(held.status, CheckStatus::Info));
        // Naming the holder takes `lsof`, which not every machine has.
        let named = format!("held by pid {} (perl", holder.id());
        assert!(
            held.message.starts_with(&named)
                || held.message.starts_with("held by a running operation"),
            "{}",
            held.message
        );
        #[cfg(target_os = "macos")]
        assert!(held.message.starts_with(&named), "{}", held.message);
        assert_eq!(first_other_pid("p12\np34\n", 12), Some(34));
        assert_eq!(first_other_pid("", 12), None);
        fixture.close().await.unwrap();
    }

    #[test]
    fn claude_shim_check_distinguishes_clean_stale_and_intentionally_disabled() {
        let cache = tempfile::tempdir().unwrap();
        let clean = claude_shim_check(cache.path(), false);
        assert!(matches!(clean.status, CheckStatus::Ok));

        let shim_dir = cache.path().join("shims");
        std::fs::create_dir_all(&shim_dir).unwrap();
        std::fs::write(shim_dir.join("claude"), "stale harness shim").unwrap();
        let stale = claude_shim_check(cache.path(), false);
        assert!(matches!(stale.status, CheckStatus::Warn));
        assert!(stale.message.contains(&shim_dir.display().to_string()));
        assert!(stale.message.contains("wt claude selftest"));

        let disabled = claude_shim_check(cache.path(), true);
        assert!(matches!(disabled.status, CheckStatus::Info));
        assert!(disabled.message.contains("WT_INSPECT=off"));
    }
}
