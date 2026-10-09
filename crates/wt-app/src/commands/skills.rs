use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use std::{ffi::OsString, io::IsTerminal, time::Duration};
use wt_platform::process::CommandSpec;
use wt_skills::{MemoryStore, SyncMode, UnitState, build_reports, detect_targets, sync};

use crate::{context::AppContext, prompt};

#[derive(Debug, Clone, Args)]
pub struct SkillsArgs {
    #[command(subcommand)]
    pub command: Option<SkillsCommand>,
}
#[derive(Debug, Clone, Subcommand)]
pub enum SkillsCommand {
    /// Show native skill and instruction status.
    Status,
    /// Install or update bundled skills.
    #[command(alias = "install")]
    Sync {
        unit: Option<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        force: bool,
    },
    /// Show the bundled content for one unit.
    Diff { unit: String },
    /// Forget saved template answers and decline decisions.
    Reset {
        #[arg(long)]
        answers: bool,
        #[arg(long)]
        declines: bool,
    },
}

fn store(context: &AppContext) -> MemoryStore {
    MemoryStore::new(context.home.join(".cache/wt/skills.json"))
}
fn label(state: UnitState) -> &'static str {
    match state {
        UnitState::Fresh => "fresh",
        UnitState::Outdated => "outdated",
        UnitState::Modified => "modified",
        UnitState::Missing => "missing",
        UnitState::Blocked => "blocked",
    }
}

pub async fn run(context: &AppContext, args: &SkillsArgs) -> Result<i32> {
    let command = args.command.clone().unwrap_or(SkillsCommand::Status);
    let memory = store(context);
    match command {
        SkillsCommand::Status => {
            let targets = detect_targets(&super::super::skills::target_options_for(context));
            if targets.harnesses.is_empty() {
                println!("{}", wt_skills::no_tools_hint());
                return Ok(0);
            }
            let reports = build_reports(&targets, &memory.load()?);
            for r in reports {
                println!(
                    "{}  {}  {}{}",
                    label(r.state),
                    r.unit.name,
                    r.path.display(),
                    if r.declined { " (declined)" } else { "" }
                );
                if let Some(detail) = r.detail {
                    println!("  {detail}")
                }
            }
            Ok(0)
        }
        SkillsCommand::Sync { unit, yes, force } => {
            let options = super::super::skills::target_options_for(context);
            let targets = detect_targets(&options);
            if targets.harnesses.is_empty() {
                println!("{} — nothing to install into", wt_skills::no_tools_hint());
                return Ok(0);
            }
            let interactive = std::io::stdin().is_terminal() && !yes;
            let mut mem = memory.load()?;
            let initial = build_reports(&targets, &mem);
            if interactive {
                let pending = initial
                    .iter()
                    .filter(|r| {
                        unit.as_ref().is_some_and(|name| name == r.unit.name)
                            || wt_skills::report_is_actionable(r)
                    })
                    .map(|r| r.unit.name)
                    .collect::<std::collections::BTreeSet<_>>();
                let vars = wt_skills::units()
                    .iter()
                    .filter(|u| pending.contains(u.name))
                    .flat_map(|u| u.vars.iter())
                    .filter(|v| !mem.answers.contains_key(v.key))
                    .collect::<Vec<_>>();
                let mut changed = false;
                for var in vars {
                    let answer =
                        prompt::read_line(&format!("{}: ", var.prompt), &context.cancellation)
                            .await?;
                    mem.answers.insert(
                        var.key.to_owned(),
                        answer.unwrap_or_default().trim().to_owned(),
                    );
                    changed = true;
                }
                if changed {
                    memory.save(&mem)?;
                }
            }
            let before = build_reports(&targets, &mem);
            let selected = before
                .into_iter()
                .filter(|r| {
                    unit.as_ref().is_none_or(|name| {
                        name == r.unit.name || *name == wt_skills::unit_key(r.unit)
                    })
                })
                .collect::<Vec<_>>();
            if let Some(name) = &unit
                && wt_skills::find_unit(name).is_none()
                && !selected
                    .iter()
                    .any(|r| wt_skills::unit_key(r.unit) == *name)
            {
                bail!("unknown skill unit `{name}`")
            }
            if !interactive && !yes && !force {
                let pending = selected
                    .iter()
                    .filter(|r| wt_skills::report_is_actionable(r))
                    .collect::<Vec<_>>();
                if pending.is_empty() {
                    println!("agent skills and instructions are up to date");
                    return Ok(0);
                }
                println!(
                    "{} pending unit(s):",
                    pending
                        .iter()
                        .map(|r| r.unit.name)
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                );
                for report in pending {
                    println!("  {} — {:?}", report.unit.name, report.state);
                }
                println!(
                    "re-run interactively, or pass --yes (add --force to replace modified copies)"
                );
                return Ok(1);
            }
            let mut approved = Vec::new();
            let mut declined = Vec::new();
            for report in selected {
                if !matches!(
                    report.state,
                    UnitState::Missing | UnitState::Outdated | UnitState::Modified
                ) {
                    continue;
                }
                if report.declined && unit.is_none() {
                    continue;
                }
                let apply = if report.state == UnitState::Modified {
                    if force && !interactive {
                        true
                    } else if !interactive {
                        false
                    } else {
                        prompt::confirm(
                            &format!(
                                "Replace modified {} at {}? [y/N] ",
                                report.unit.name,
                                report.path.display()
                            ),
                            false,
                            &context.cancellation,
                        )
                        .await?
                    }
                } else {
                    yes || force
                        || (interactive
                            && prompt::confirm(
                                &format!(
                                    "Install/update {} at {}? [y/N] ",
                                    report.unit.name,
                                    report.path.display()
                                ),
                                false,
                                &context.cancellation,
                            )
                            .await?)
                };
                if apply {
                    approved.push(report)
                } else if interactive {
                    declined.push((
                        wt_skills::decline_key(report.unit, &report.target),
                        report.canonical_hash,
                    ));
                }
            }
            if !declined.is_empty() {
                memory.update(|m| {
                    for (key, hash) in declined {
                        m.declined.insert(key, hash);
                    }
                })?;
            }
            if interactive {
                let roots = approved
                    .iter()
                    .filter_map(|r| match &r.target {
                        wt_skills::TargetRef::Skills(wt_skills::SkillsTarget::Rulesync {
                            rulesync,
                            ..
                        })
                        | wt_skills::TargetRef::Instructions(
                            wt_skills::InstructionsTarget::Rulesync { rulesync, .. },
                        ) => Some(rulesync.clone()),
                        _ => None,
                    })
                    .fold(std::collections::BTreeMap::new(), |mut m, r| {
                        m.insert(r.root.clone(), r);
                        m
                    })
                    .into_values()
                    .collect::<Vec<_>>();
                let mut skipped = std::collections::BTreeSet::new();
                for root in roots {
                    if !prompt::confirm(
                        &format!(
                            "Applying will run `{}` in {}. Continue? [Y/n] ",
                            root.regen.join(" "),
                            root.root.display()
                        ),
                        true,
                        &context.cancellation,
                    )
                    .await?
                    {
                        skipped.insert(root.root);
                    }
                }
                approved.retain(|r| !match &r.target {
                    wt_skills::TargetRef::Skills(wt_skills::SkillsTarget::Rulesync {
                        rulesync,
                        ..
                    })
                    | wt_skills::TargetRef::Instructions(
                        wt_skills::InstructionsTarget::Rulesync { rulesync, .. },
                    ) => skipped.contains(&rulesync.root),
                    _ => false,
                });
            }
            let mode = SyncMode::Force;
            let summary = sync(&approved, mode)?;
            // Regenerate each touched rulesync pipeline after its source files
            // are durable. Failures are reported without undoing those sources.
            let roots = approved
                .iter()
                .filter_map(|r| match &r.target {
                    wt_skills::TargetRef::Skills(wt_skills::SkillsTarget::Rulesync {
                        rulesync,
                        ..
                    })
                    | wt_skills::TargetRef::Instructions(
                        wt_skills::InstructionsTarget::Rulesync { rulesync, .. },
                    ) => Some(rulesync.clone()),
                    _ => None,
                })
                .fold(std::collections::BTreeMap::new(), |mut m, r| {
                    m.insert(r.root.clone(), r);
                    m
                })
                .into_values()
                .collect::<Vec<_>>();
            for root in roots {
                let Some((program, args)) = root.regen.split_first() else {
                    continue;
                };
                let mut spec = CommandSpec::new(OsString::from(program))
                    .args(args.iter().map(OsString::from))
                    .cwd(root.root);
                spec.timeout = Duration::from_secs(180);
                let output = context.processes.run(spec, &context.cancellation).await;
                match output {
                    Ok(out) if out.status.success() => {}
                    Ok(out) => eprintln!("rulesync generation failed: {}", out.stderr_text()),
                    Err(e) => eprintln!("rulesync generation failed: {e}"),
                }
            }
            println!(
                "installed {}, updated {}, skipped modified {}, blocked {}",
                summary.installed, summary.updated, summary.skipped_modified, summary.blocked
            );
            Ok(0)
        }
        SkillsCommand::Diff { unit } => {
            let _unit = wt_skills::find_unit(&unit)
                .with_context(|| format!("unknown skill unit `{unit}`"))?;
            let targets = detect_targets(&super::super::skills::target_options_for(context));
            let reports = build_reports(&targets, &memory.load()?)
                .into_iter()
                .filter(|r| r.unit.name == unit)
                .collect::<Vec<_>>();
            for report in reports {
                println!("--- {} ({})", report.path.display(), label(report.state));
                let old = match &report.target {
                    wt_skills::TargetRef::Skills(_) => std::fs::read_to_string(&report.path)
                        .ok()
                        .map(|text| wt_skills::split_stamp(&text).0),
                    wt_skills::TargetRef::Instructions(_) => {
                        std::fs::read_to_string(&report.path).ok().and_then(|text| {
                            wt_skills::extract_instructions_block(&text).map(|b| b.body)
                        })
                    }
                };
                if let Some(old) = old {
                    for line in old.lines() {
                        println!("-{line}");
                    }
                }
                for line in report.expected.lines() {
                    println!("+{line}");
                }
            }
            Ok(0)
        }
        SkillsCommand::Reset { answers, declines } => {
            let mut mem = memory.load()?;
            if !answers && !declines || answers {
                mem.answers.clear();
            }
            if !answers && !declines || declines {
                mem.declined.clear();
            }
            memory.save(&mem)?;
            println!("cleared saved skills answers and declines");
            Ok(0)
        }
    }
}
