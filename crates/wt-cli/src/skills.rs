//! Native access to the compile-time bundled skills.

use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::IsTerminal,
    path::PathBuf,
    time::Duration,
};
use wt_core::HarnessId;
use wt_platform::process::CommandSpec;

use crate::context::AppContext;

fn target_options(context: &AppContext) -> wt_skills::TargetOptions {
    wt_skills::TargetOptions {
        home: context.home.clone(),
        codex_home: std::env::var_os("CODEX_HOME").map(PathBuf::from),
        pi_dir: std::env::var_os("PI_CODING_AGENT_DIR").map(PathBuf::from),
    }
}

/// Build the native prompt for an already installed start skill. Callers can
/// use this in worker starts without depending on the repository checkout.
pub async fn start_prompt(context: &AppContext, harness: HarnessId) -> Result<String> {
    let options = target_options(context);
    let can_resolve = tokio::task::spawn_blocking(move || {
        wt_skills::harness_can_resolve_skill(harness, "start", &options)
    })
    .await
    .context("join skill lookup")?;
    if !can_resolve {
        bail!(
            "the `{}` harness cannot resolve the bundled `start` skill; run `wt skills sync start --yes` on this host and retry",
            harness.as_str()
        );
    }
    wt_skills::start_skill_invocation(harness).context("select native start-skill invocation")
}

/// Offer installed skill updates before the TUI takes control of the terminal.
/// Startup is silent on non-interactive runs and when every managed copy is
/// already fresh. Modified local copies always require a separate explicit yes.
pub async fn startup_check(context: &AppContext) -> Result<()> {
    if std::env::var_os("WT_SKILLS").is_some_and(|value| value == "off")
        || !context.config.skills.startup_check
        || !std::io::stdin().is_terminal()
        || !std::io::stderr().is_terminal()
    {
        return Ok(());
    }
    let options = target_options(context);
    let memory = wt_skills::MemoryStore::new(context.home.join(".cache/wt/skills.json"));
    let (targets, mut mem, reports) = tokio::task::spawn_blocking({
        let memory = memory.clone();
        move || {
            let targets = wt_skills::detect_targets(&options);
            let mem = memory.load()?;
            let reports = wt_skills::build_reports(&targets, &mem);
            Ok::<_, wt_skills::MemoryError>((targets, mem, reports))
        }
    })
    .await
    .context("join startup skills scan")??;
    if targets.harnesses.is_empty() {
        return Ok(());
    }
    let actionable = reports
        .iter()
        .filter(|r| wt_skills::report_is_actionable(r))
        .collect::<Vec<_>>();
    if actionable.is_empty() {
        return Ok(());
    }
    eprintln!(
        "wt: {} agent-skill update(s) available",
        actionable
            .iter()
            .map(|r| r.unit.name)
            .collect::<BTreeSet<_>>()
            .len()
    );
    let mut by_unit: BTreeMap<&str, Vec<&wt_skills::UnitReport>> = BTreeMap::new();
    for report in actionable {
        by_unit.entry(report.unit.name).or_default().push(report)
    }
    let mut accepted = BTreeSet::new();
    for (name, items) in by_unit {
        let modified = items
            .iter()
            .any(|r| r.state == wt_skills::UnitState::Modified);
        let message = if modified {
            format!("~ {name}: local copy differs. Overwrite with the wt-managed version? [y/N] ")
        } else {
            format!("Install/update {name}? [Y/n] ")
        };
        let yes = crate::prompt::confirm(&message, !modified, &context.cancellation).await?;
        if yes {
            accepted.insert(name.to_owned());
        } else {
            memory.update(|state| {
                for r in &items {
                    state.declined.insert(
                        wt_skills::decline_key(r.unit, &r.target),
                        r.canonical_hash.clone(),
                    );
                }
            })?;
        }
    }
    if accepted.is_empty() {
        return Ok(());
    }
    let mut changed = false;
    for report in &reports {
        if accepted.contains(report.unit.name) {
            for var in report.unit.vars {
                if !mem.answers.contains_key(var.key) {
                    let answer = crate::prompt::read_line(
                        &format!("{}: ", var.prompt),
                        &context.cancellation,
                    )
                    .await?
                    .unwrap_or_default();
                    mem.answers.insert(var.key.into(), answer.trim().into());
                    changed = true;
                }
            }
        }
    }
    if changed {
        memory.save(&mem)?;
    }
    let mut fresh_reports = tokio::task::spawn_blocking({
        let targets = targets.clone();
        let mem = mem.clone();
        move || wt_skills::build_reports(&targets, &mem)
    })
    .await
    .context("join startup skills refresh")?;
    fresh_reports.retain(|r| {
        accepted.contains(r.unit.name)
            && matches!(
                r.state,
                wt_skills::UnitState::Missing
                    | wt_skills::UnitState::Outdated
                    | wt_skills::UnitState::Modified
            )
    });
    let roots = fresh_reports
        .iter()
        .filter_map(|r| match &r.target {
            wt_skills::TargetRef::Skills(wt_skills::SkillsTarget::Rulesync {
                rulesync, ..
            })
            | wt_skills::TargetRef::Instructions(wt_skills::InstructionsTarget::Rulesync {
                rulesync,
                ..
            }) => Some(rulesync.clone()),
            _ => None,
        })
        .fold(BTreeMap::new(), |mut m, r| {
            m.insert(r.root.clone(), r);
            m
        })
        .into_values()
        .collect::<Vec<_>>();
    let mut approved_roots = BTreeSet::new();
    for root in roots {
        if crate::prompt::confirm(
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
            approved_roots.insert(root.root);
        }
    }
    fresh_reports.retain(|r| match &r.target {
        wt_skills::TargetRef::Skills(wt_skills::SkillsTarget::Rulesync { rulesync, .. })
        | wt_skills::TargetRef::Instructions(wt_skills::InstructionsTarget::Rulesync {
            rulesync,
            ..
        }) => approved_roots.contains(&rulesync.root),
        _ => true,
    });
    let applied = tokio::task::spawn_blocking(move || {
        wt_skills::sync(&fresh_reports, wt_skills::SyncMode::Force)
    })
    .await
    .context("join startup skills apply")??;
    if applied.installed + applied.updated > 0 {
        eprintln!(
            "wt: installed {}, updated {} agent-skill unit(s)",
            applied.installed, applied.updated
        );
    }
    for root in approved_roots {
        if let Some(info) = targets
            .skills
            .iter()
            .find_map(|t| match t {
                wt_skills::SkillsTarget::Rulesync { rulesync, .. } if rulesync.root == root => {
                    Some(rulesync.clone())
                }
                _ => None,
            })
            .or_else(|| {
                targets.instructions.iter().find_map(|t| match t {
                    wt_skills::InstructionsTarget::Rulesync { rulesync, .. }
                        if rulesync.root == root =>
                    {
                        Some(rulesync.clone())
                    }
                    _ => None,
                })
            })
        {
            let Some((program, args)) = info.regen.split_first() else {
                continue;
            };
            let mut spec = CommandSpec::new(program).args(args).cwd(root);
            spec.timeout = Duration::from_secs(180);
            let output = context
                .processes
                .run(spec, &context.cancellation)
                .await
                .context("run rulesync generator")?;
            if !output.status.success() {
                eprintln!("rulesync generation failed: {}", output.stderr_text());
            }
        }
    }
    Ok(())
}

pub(crate) fn target_options_for(context: &AppContext) -> wt_skills::TargetOptions {
    target_options(context)
}
