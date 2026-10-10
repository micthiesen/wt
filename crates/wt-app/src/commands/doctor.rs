use anyhow::Result;
use clap::Args;
use serde::Serialize;
use std::path::Path;
use wt_github::{GithubClient, GithubOptions};
use wt_harness::stale_harness_shims;
use wt_platform::process::CommandSpec;

use crate::{
    commands::{
        diagnostics::{Check, CheckStatus, display_status, worst},
        resolve::worktree_at_cwd,
    },
    context::AppContext,
};

#[derive(Debug, Clone, Args, Default)]
pub struct DoctorArgs {
    pub target: Option<String>,
    #[arg(short, long)]
    pub all: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct Report {
    slug: String,
    path: String,
    status: CheckStatus,
    checks: Vec<Check>,
}

pub async fn run(ctx: &AppContext, args: &DoctorArgs) -> Result<i32> {
    if args.all && args.target.is_some() {
        eprintln!("a target cannot be combined with --all");
        return Ok(2);
    }
    let records = ctx.repository.inventory(&ctx.cancellation).await?;
    let selected = if args.all {
        records.into_iter().filter(|r| !r.is_main).collect()
    } else if let Some(target) = &args.target {
        vec![crate::commands::resolve::resolve_from_inventory(
            &records, target, true,
        )?]
    } else if let Some(cwd) = worktree_at_cwd(&records, &ctx.cwd) {
        vec![cwd.clone()]
    } else {
        records.into_iter().filter(|r| !r.is_main).collect()
    };
    let branches = selected
        .iter()
        .filter(|record| !record.target.branch.is_empty())
        .map(|record| record.target.branch.clone())
        .collect::<Vec<_>>();
    let github = GithubClient::new(
        ctx.processes.clone(),
        ctx.config.paths.main_clone.clone(),
        GithubOptions::from_config(&ctx.config, true),
    );
    let (prs, pr_error) = match github.fetch_worktrees(&branches, &ctx.cancellation).await {
        Ok(data) => (Some(data.prs), None),
        Err(error) => (None, Some(error.to_string())),
    };
    if !args.json {
        print_banners(ctx).await?;
    }
    let mut reports = Vec::new();
    for row in selected {
        let path = std::path::PathBuf::from(&row.target.path);
        let mut checks = Vec::new();
        let status = ctx.repository.status(&row, &ctx.cancellation).await;
        match status {
            Ok(status) if !status.dirty => {
                checks.push(check("working tree", CheckStatus::Ok, "clean".into()))
            }
            Ok(status) => checks.push(check(
                "working tree",
                CheckStatus::Warn,
                format!(
                    "{} tracked change(s), {} untracked file(s)",
                    status.tracked_changes, status.untracked_files
                ),
            )),
            Err(error) => checks.push(check("working tree", CheckStatus::Err, error.to_string())),
        }
        let trunk = format!("origin/{}", ctx.config.branch.base);
        let compare = ctx
            .processes
            .run(
                CommandSpec::new("git")
                    .args([
                        "rev-list",
                        "--left-right",
                        "--count",
                        trunk.as_str(),
                        "HEAD",
                    ])
                    .cwd(&path),
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
                let behind = nums.first().copied().unwrap_or(0);
                let ahead = nums.get(1).copied().unwrap_or(0);
                let status = if behind == 0 {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Warn
                };
                checks.push(check(
                    "sync",
                    status,
                    if behind == 0 && ahead == 0 {
                        "up to date".into()
                    } else {
                        format!("{ahead} ahead, {behind} behind {trunk}")
                    },
                ));
            }
            Ok(_) => checks.push(check(
                "sync",
                CheckStatus::Warn,
                format!("cannot compare with {trunk}"),
            )),
            Err(error) => checks.push(check(
                "sync",
                CheckStatus::Warn,
                format!("cannot compare with {trunk}: {error}"),
            )),
        }
        if path.join("package.json").exists() {
            let installed = path.join("node_modules").is_dir();
            checks.push(check(
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
                checks.push(check(
                    "pnpm tree",
                    CheckStatus::Info,
                    "native doctor checks install presence; it does not scan pnpm's isolated virtual store for unmanaged top-level packages".into(),
                ));
            }
        }
        match crate::commands::diagnostics::operation_lock(ctx, row.target.slug()).await {
            Ok(Some(lock)) => {
                let label = lock.op.as_deref().unwrap_or("operation");
                let phase = lock.phase.as_deref().filter(|phase| !phase.is_empty());
                let pid = lock.pid.map_or_else(|| "?".into(), |pid| pid.to_string());
                let message = match phase {
                    Some(phase) => format!("{label}: {phase} (pid {pid})"),
                    None => format!("{label} (pid {pid})"),
                };
                checks.push(check("operation lock", CheckStatus::Warn, message));
            }
            Ok(None) => checks.push(check("operation lock", CheckStatus::Ok, "none".into())),
            Err(error) => checks.push(check(
                "operation lock",
                CheckStatus::Info,
                format!("unable to inspect lock: {error}"),
            )),
        }
        if ctx.config.sst.is_some() {
            checks.extend(check_stage(ctx, &path, row.target.slug()));
        }
        if !row.target.branch.is_empty() {
            let expected = ctx
                .database
                .call({
                    let slug = row.target.slug().to_owned();
                    move |store| {
                        let state = store.read_slug_state(&slug)?;
                        Ok(state
                            .as_ref()
                            .and_then(|record| record.get("baseBranch"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned))
                    }
                })
                .await?
                .unwrap_or_else(|| ctx.config.branch.base.clone());
            let key = format!("branch.{}.gh-merge-base", row.target.branch);
            let got = ctx
                .processes
                .run(
                    CommandSpec::new("git")
                        .args(["config", key.as_str()])
                        .cwd(&path),
                    &ctx.cancellation,
                )
                .await;
            let actual = got
                .ok()
                .filter(|out| out.status.success())
                .map(|out| out.stdout_text().trim().to_owned())
                .filter(|value| !value.is_empty());
            checks.push(check(
                "gh merge base",
                if actual.as_deref() == Some(&expected) {
                    CheckStatus::Ok
                } else {
                    CheckStatus::Warn
                },
                actual
                    .map(|actual| format!("set to {actual}, expected {expected}"))
                    .unwrap_or_else(|| {
                        format!(
                            "unset; bare `gh pr create` targets the repo default; fix: git -C {} config {} {}",
                            path.display(), key, expected
                        )
                    }),
            ));
            checks.push(check_merged(ctx, &path, &trunk).await);
            match prs.as_ref().and_then(|prs| prs.get(&row.target.branch)) {
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
        for report in &reports {
            println!("{}  {}", report.slug, display_status(report.status));
            for check in &report.checks {
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
    }
    Ok(
        if reports.iter().any(|r| matches!(r.status, CheckStatus::Err)) {
            1
        } else {
            0
        },
    )
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
                println!(
                    "warning `wt` at {} resolves to {}, expected {}",
                    candidate.display(),
                    resolved.display(),
                    expected.display()
                );
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
        Ok(out) if out.status.success() => {
            let ahead = out.stdout_text().trim().parse::<u64>().unwrap_or(0);
            if ahead == 0 {
                return check("merged", CheckStatus::Ok, "no commits beyond trunk".into());
            }
            let ancestor = ctx
                .processes
                .run(
                    CommandSpec::new("git")
                        .args(["merge-base", "--is-ancestor", "HEAD", trunk])
                        .cwd(worktree),
                    &ctx.cancellation,
                )
                .await;
            match ancestor {
                Ok(out) if out.status.success() => {
                    check("merged", CheckStatus::Info, format!("contained in {trunk}"))
                }
                Ok(out) if out.status.code() == Some(1) => check(
                    "merged",
                    CheckStatus::Ok,
                    format!("not contained in {trunk}"),
                ),
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
    use crate::commands::test_support::CommandFixture;
    use sha2::{Digest, Sha256};

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
