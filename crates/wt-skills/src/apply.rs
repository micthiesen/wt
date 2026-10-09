use crate::{
    InstructionsTarget, SkillsTarget, TargetRef, UnitReport, UnitState, splice_instructions_block,
    stamp_content,
};
use std::{fs, path::Path};
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    Safe,
    Force,
}
#[derive(Clone, Debug, Default)]
pub struct SyncSummary {
    pub installed: usize,
    pub updated: usize,
    pub skipped_modified: usize,
    pub blocked: usize,
}
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("sync skills: {0}")]
    Io(#[from] std::io::Error),
    #[error("refusing to replace modified skill {0}")]
    Modified(String),
    #[error("target is blocked: {0}")]
    Blocked(String),
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("target has no parent"))?;
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("wt-{}.tmp", std::process::id()));
    {
        use std::io::Write;
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Ok(d) = fs::File::open(parent) {
        let _ = d.sync_all();
    }
    Ok(())
}
fn write_skill(report: &UnitReport) -> Result<(), SyncError> {
    let parent = report
        .path
        .parent()
        .ok_or_else(|| std::io::Error::other("skill path has no parent"))?;
    fs::create_dir_all(parent)?;
    // Build the replacement beside the destination, then rename. Existing user
    // content remains available until the complete new file is durable.
    atomic_write(&report.path, stamp_content(&report.expected).as_bytes())?;
    Ok(())
}
fn write_instructions(report: &UnitReport) -> Result<(), SyncError> {
    if matches!(&report.target,TargetRef::Instructions(InstructionsTarget::Rulesync{rulesync,..}) if rulesync.root_rule_file.is_none())
    {
        return Err(SyncError::Blocked("missing rulesync root rule".into()));
    }
    let current = match fs::read_to_string(&report.path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let next = splice_instructions_block(&current, &report.expected);
    atomic_write(&report.path, next.as_bytes())?;
    Ok(())
}
pub fn sync(reports: &[UnitReport], mode: SyncMode) -> Result<SyncSummary, SyncError> {
    let mut summary = SyncSummary::default();
    for r in reports {
        if r.state == UnitState::Blocked {
            summary.blocked += 1;
            continue;
        }
        if !matches!(
            r.state,
            UnitState::Missing | UnitState::Outdated | UnitState::Modified
        ) {
            continue;
        }
        if r.declined {
            continue;
        }
        if r.state == UnitState::Modified && mode == SyncMode::Safe {
            summary.skipped_modified += 1;
            continue;
        }
        match &r.target {
            TargetRef::Skills(SkillsTarget::Native { .. } | SkillsTarget::Rulesync { .. }) => {
                write_skill(r)?
            }
            TargetRef::Instructions(_) => write_instructions(r)?,
        }
        if r.state == UnitState::Missing {
            summary.installed += 1
        } else {
            summary.updated += 1
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SkillsMemory, Targets, build_reports};
    #[test]
    fn safe_sync_preserves_existing_personal_skill() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("skills");
        let p = root.join("wt/SKILL.md");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "personal").unwrap();
        let targets = Targets {
            skills: vec![SkillsTarget::Native {
                harnesses: vec![],
                dir: root,
            }],
            ..Default::default()
        };
        let reports = build_reports(&targets, &SkillsMemory::default());
        let summary = sync(&reports, SyncMode::Safe).unwrap();
        assert_eq!(summary.skipped_modified, 1);
        assert_eq!(fs::read_to_string(p).unwrap(), "personal");
    }
}
