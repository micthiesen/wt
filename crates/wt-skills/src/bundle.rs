#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitKind {
    Skill,
    Instructions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TemplateVar {
    pub key: &'static str,
    pub prompt: &'static str,
    pub fallback: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct Unit {
    pub kind: UnitKind,
    pub name: &'static str,
    pub summary: &'static str,
    pub source: &'static str,
    pub vars: &'static [TemplateVar],
}

const NO_VARS: &[TemplateVar] = &[];
const PROJECT_NOTES: &[TemplateVar] = &[TemplateVar {
    key: "project_notes",
    prompt: "Project-specific conventions for agents starting work (design-review flow, testing tools/skills, tracker quirks). One line or a short sentence; empty for none",
    fallback: "(none configured — if project conventions are unclear, ask the human rather than guessing)",
}];

pub static UNITS: &[Unit] = &[
    Unit {
        kind: UnitKind::Instructions,
        name: "instructions",
        summary: "always-on agent rules",
        source: include_str!("../../../skills/instructions.md"),
        vars: NO_VARS,
    },
    Unit {
        kind: UnitKind::Skill,
        name: "wt",
        summary: "wt orientation: subcommands, conventions, stacked-PR model",
        source: include_str!("../../../skills/wt/SKILL.md"),
        vars: NO_VARS,
    },
    Unit {
        kind: UnitKind::Skill,
        name: "restack",
        summary: "conflict-resolution playbook for `wt restack`",
        source: include_str!("../../../skills/restack/SKILL.md"),
        vars: NO_VARS,
    },
    Unit {
        kind: UnitKind::Skill,
        name: "manager",
        summary: "playbook for the singleton manager session",
        source: include_str!("../../../skills/manager/SKILL.md"),
        vars: NO_VARS,
    },
    Unit {
        kind: UnitKind::Skill,
        name: "shepherd",
        summary: "drive the whole fleet toward mergeable, stopping short of merge",
        source: include_str!("../../../skills/shepherd/SKILL.md"),
        vars: NO_VARS,
    },
    Unit {
        kind: UnitKind::Skill,
        name: "babysit",
        summary: "drive this worktree's branch through review",
        source: include_str!("../../../skills/babysit/SKILL.md"),
        vars: NO_VARS,
    },
    Unit {
        kind: UnitKind::Skill,
        name: "start",
        summary: "kick off work inside a prepared worktree",
        source: include_str!("../../../skills/start/SKILL.md"),
        vars: PROJECT_NOTES,
    },
    Unit {
        kind: UnitKind::Skill,
        name: "handoff",
        summary: "create a follow-up worktree and start its agent",
        source: include_str!("../../../skills/handoff/SKILL.md"),
        vars: NO_VARS,
    },
    Unit {
        kind: UnitKind::Skill,
        name: "triage",
        summary: "turn a task batch into prioritized worktrees",
        source: include_str!("../../../skills/triage/SKILL.md"),
        vars: PROJECT_NOTES,
    },
];

pub fn units() -> &'static [Unit] {
    UNITS
}
pub fn find_unit(name: &str) -> Option<&'static Unit> {
    UNITS.iter().find(|unit| unit.name == name)
}
pub fn unit_source(name: &str) -> Option<&'static str> {
    find_unit(name).map(|unit| unit.source)
}
pub fn unit_key(unit: &Unit) -> String {
    match unit.kind {
        UnitKind::Skill => format!("skill:{}", unit.name),
        UnitKind::Instructions => "instructions".into(),
    }
}
