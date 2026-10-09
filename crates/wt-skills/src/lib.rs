//! Compile-time bundled agent skills and managed instruction distribution.
//!
//! No runtime read depends on the wt source checkout. All mutable paths,
//! environment values, and process services are explicit inputs.

mod apply;
mod bundle;
mod memory;
mod report;
mod targets;
mod template;

pub use apply::{SyncMode, SyncSummary, sync};
pub use bundle::{TemplateVar, Unit, UnitKind, find_unit, unit_key, unit_source, units};
pub use memory::{MemoryError, MemoryStore, SkillsMemory};
pub use report::{
    TargetRef, UnitReport, UnitState, build_reports, decline_key, load_reports,
    report_is_actionable,
};
pub use targets::{
    AgentToolId, InstructionsTarget, RulesyncInfo, SkillsTarget, TargetOptions, Targets,
    detect_targets, harness_can_resolve_skill, harness_skill_dirs, no_tools_hint, target_key,
};
pub use template::{
    content_hash, count_instructions_blocks, extract_instructions_block, normalize_body,
    render_template, splice_instructions_block, split_stamp, stamp_content, strip_rulesync_keys,
};

use anyhow::{Result, bail};
use wt_core::HarnessId;

/// Return a harness-native invocation for the already-provisioned start skill.
/// The daemon/controller uses this instead of resolving source files at runtime.
pub fn start_skill_invocation(harness: HarnessId) -> Result<String> {
    match harness {
        HarnessId::Claude => Ok("/start".into()),
        HarnessId::Codex | HarnessId::Opencode => Ok("$start".into()),
    }
}

/// Render the bundled start skill for contexts where the harness cannot load
/// installed skill files. The caller decides whether a full body or native
/// invocation is appropriate for the selected harness.
pub fn render_start_skill(answers: &std::collections::BTreeMap<String, String>) -> Result<String> {
    let unit =
        find_unit("start").ok_or_else(|| anyhow::anyhow!("bundled start skill is missing"))?;
    Ok(template::normalize_body(&template::render_template(
        unit.source,
        unit.vars,
        answers,
    )))
}

/// Fail clearly when the embedded bundle omitted an expected instruction.
pub fn bundled_instructions() -> Result<&'static str> {
    let unit = find_unit("instructions")
        .ok_or_else(|| anyhow::anyhow!("bundled instructions are missing"))?;
    if unit.kind != UnitKind::Instructions {
        bail!("invalid instructions bundle")
    }
    Ok(unit.source)
}
