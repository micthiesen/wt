use crate::{
    InstructionsTarget, MemoryStore, SkillsMemory, SkillsTarget, Targets, Unit, UnitKind,
    content_hash, count_instructions_blocks, extract_instructions_block, normalize_body,
    render_template, split_stamp, strip_rulesync_keys, target_key, unit_key, units,
};
use std::{fs, io::ErrorKind, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitState {
    Fresh,
    Outdated,
    Modified,
    Missing,
    Blocked,
}
#[derive(Clone, Debug)]
pub struct UnitReport {
    pub unit: &'static Unit,
    pub target: TargetRef,
    pub state: UnitState,
    pub canonical_hash: String,
    pub path: PathBuf,
    pub expected: String,
    pub detail: Option<String>,
    pub declined: bool,
}
#[derive(Clone, Debug)]
pub enum TargetRef {
    Skills(SkillsTarget),
    Instructions(InstructionsTarget),
}
impl TargetRef {
    pub fn key(&self) -> String {
        match self {
            Self::Skills(t) => target_key(t),
            Self::Instructions(t) => target_key(t),
        }
    }
}
pub fn decline_key(unit: &Unit, target: &TargetRef) -> String {
    format!("{}::{}", unit_key(unit), target.key())
}
pub fn report_is_actionable(r: &UnitReport) -> bool {
    !r.declined
        && matches!(
            r.state,
            UnitState::Missing | UnitState::Outdated | UnitState::Modified
        )
}
fn render(unit: &Unit, memory: &SkillsMemory) -> String {
    normalize_body(&render_template(unit.source, unit.vars, &memory.answers))
}
fn skill_path(unit: &Unit, target: &SkillsTarget) -> PathBuf {
    match target {
        SkillsTarget::Native { dir, .. } => dir,
        SkillsTarget::Rulesync { rulesync, .. } => &rulesync.skills_dir,
    }
    .join(unit.name)
    .join("SKILL.md")
}
fn check_declined(mem: &SkillsMemory, unit: &Unit, key: &str, hash: &str) -> bool {
    mem.declined.get(key).is_some_and(|h| h == hash)
        || mem.declined.get(&unit_key(unit)).is_some_and(|h| h == hash)
}
pub fn build_reports(targets: &Targets, memory: &SkillsMemory) -> Vec<UnitReport> {
    let mut out = vec![];
    for unit in units() {
        let expected = render(unit, memory);
        let hash = content_hash(&expected);
        if unit.kind == UnitKind::Skill {
            for target in &targets.skills {
                let target = TargetRef::Skills(target.clone());
                let path = skill_path(
                    unit,
                    match &target {
                        TargetRef::Skills(t) => t,
                        _ => unreachable!(),
                    },
                );
                let expected = if matches!(target, TargetRef::Skills(SkillsTarget::Native { .. })) {
                    normalize_body(&strip_rulesync_keys(&expected))
                } else {
                    expected.clone()
                };
                let key = decline_key(unit, &target);
                let declined = check_declined(memory, unit, &key, &hash);
                let installed = fs::read_to_string(&path);
                let (state, detail) = match installed {
                    Err(error) if error.kind() == ErrorKind::NotFound => {
                        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                            (
                                UnitState::Blocked,
                                Some("skill file is a dangling symlink".into()),
                            )
                        } else {
                            (UnitState::Missing, None)
                        }
                    }
                    Err(error) => (
                        UnitState::Blocked,
                        Some(format!("cannot read installed skill: {error}")),
                    ),
                    Ok(s) => {
                        let (body, stamp) = split_stamp(&s);
                        if body == expected {
                            (UnitState::Fresh, None)
                        } else if stamp.as_deref() == Some(content_hash(&body).as_str()) {
                            (UnitState::Outdated, None)
                        } else {
                            (
                                UnitState::Modified,
                                Some(
                                    if stamp.is_none() {
                                        "existing copy was not installed by wt"
                                    } else {
                                        "edited after install"
                                    }
                                    .into(),
                                ),
                            )
                        }
                    }
                };
                out.push(UnitReport {
                    unit,
                    target,
                    state,
                    canonical_hash: hash.clone(),
                    path,
                    expected,
                    detail,
                    declined,
                });
            }
        } else {
            for target in &targets.instructions {
                let target = TargetRef::Instructions(target.clone());
                let (path, blocked) = match &target {
                    TargetRef::Instructions(InstructionsTarget::Native { file, .. }) => {
                        (file.clone(), None)
                    }
                    TargetRef::Instructions(InstructionsTarget::Rulesync { rulesync, .. }) => {
                        match &rulesync.root_rule_file {
                            Some(p) => (p.clone(), None),
                            None => (
                                rulesync.root.join(".rulesync/rules"),
                                Some("no root: true rulesync rule file found".to_owned()),
                            ),
                        }
                    }
                    _ => unreachable!(),
                };
                let expected = expected.trim_end_matches('\n').to_owned();
                let current = fs::read_to_string(&path);
                let (state, detail) = if let Some(d) = blocked {
                    (UnitState::Blocked, Some(d))
                } else {
                    match current {
                        Err(error) if error.kind() == ErrorKind::NotFound => {
                            if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink())
                            {
                                (
                                    UnitState::Blocked,
                                    Some(
                                        "instructions file is a dangling symlink; fix it first"
                                            .into(),
                                    ),
                                )
                            } else {
                                (UnitState::Missing, None)
                            }
                        }
                        Err(error) => (
                            UnitState::Blocked,
                            Some(format!("cannot read instructions file: {error}")),
                        ),
                        Ok(text) if count_instructions_blocks(&text) > 1 => (
                            UnitState::Blocked,
                            Some("multiple managed blocks found; remove duplicates by hand".into()),
                        ),
                        Ok(text) => match extract_instructions_block(&text) {
                            None => (UnitState::Missing, None),
                            Some(b) if b.body == expected => (UnitState::Fresh, None),
                            Some(b) if b.hash == content_hash(&b.body) => {
                                (UnitState::Outdated, None)
                            }
                            Some(_) => (
                                UnitState::Modified,
                                Some("instructions block edited after install".into()),
                            ),
                        },
                    }
                };
                let key = decline_key(unit, &target);
                let declined = check_declined(memory, unit, &key, &hash);
                out.push(UnitReport {
                    unit,
                    target,
                    state,
                    canonical_hash: hash.clone(),
                    path,
                    expected,
                    detail,
                    declined,
                });
            }
        }
    }
    out
}
pub fn load_reports(
    targets: &Targets,
    store: &MemoryStore,
) -> Result<Vec<UnitReport>, crate::MemoryError> {
    Ok(build_reports(targets, &store.load()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modified_copy_is_never_implicit_update() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skill");
        fs::create_dir_all(dir.join("start")).unwrap();
        fs::write(dir.join("start/SKILL.md"), "personal text").unwrap();
        let targets = Targets {
            skills: vec![SkillsTarget::Native {
                harnesses: vec![],
                dir: dir.clone(),
            }],
            ..Default::default()
        };
        let reports = build_reports(&targets, &SkillsMemory::default());
        let report = reports.iter().find(|r| r.unit.name == "start").unwrap();
        assert_eq!(report.state, UnitState::Modified);
        assert!(report_is_actionable(report));
    }

    #[test]
    fn dangling_instructions_link_is_blocked_instead_of_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("AGENTS.md");
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("missing"), &file).unwrap();
        let targets = Targets {
            instructions: vec![InstructionsTarget::Native {
                harnesses: vec![],
                file,
            }],
            ..Default::default()
        };
        let reports = build_reports(&targets, &SkillsMemory::default());
        assert_eq!(reports[0].state, UnitState::Blocked);
        assert!(!report_is_actionable(&reports[0]));
    }

    #[test]
    fn intact_old_stamp_is_safe_to_update() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("skills");
        let path = dir.join("start/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, crate::stamp_content("older bundle")).unwrap();
        let targets = Targets {
            skills: vec![SkillsTarget::Native {
                harnesses: vec![],
                dir,
            }],
            ..Default::default()
        };
        let report = build_reports(&targets, &SkillsMemory::default())
            .into_iter()
            .find(|r| r.unit.name == "start")
            .unwrap();
        assert_eq!(report.state, UnitState::Outdated);
        assert!(report_is_actionable(&report));
    }
}
