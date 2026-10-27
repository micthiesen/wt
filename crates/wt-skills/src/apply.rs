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

/// The file a write should replace. A symlinked final component names a
/// durable source elsewhere (dotfiles link skills per file); renaming onto
/// the link itself would swap it for a plain copy and orphan that source.
/// Dangling links are reported as blocked before any write.
fn write_target(path: &Path) -> Result<std::path::PathBuf, std::io::Error> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(path),
        _ => Ok(path.to_path_buf()),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let path = &write_target(path)?;
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
/// State of a CLAUDE.md wt no longer writes its block into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetiredInstructions {
    /// No file, or no wt block in it.
    Clean,
    /// wt's block is still there; sync removes it.
    Pending,
    /// Markers are damaged or repeated; left for a human.
    Blocked(String),
}

pub fn inspect_retired(path: &Path) -> RetiredInstructions {
    match fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => RetiredInstructions::Clean,
        Err(error) => RetiredInstructions::Blocked(format!("cannot read: {error}")),
        Ok(text) if crate::count_instructions_blocks(&text) == 0 => RetiredInstructions::Clean,
        Ok(text) if crate::remove_instructions_block(&text).is_some() => {
            RetiredInstructions::Pending
        }
        Ok(_) => RetiredInstructions::Blocked(
            "wt block markers are damaged or repeated; remove them by hand".into(),
        ),
    }
}

/// Remove wt's block from a retired instructions file, leaving everything
/// else. A plain file left empty is deleted; a symlinked one is written
/// through and kept. Returns whether anything changed.
pub fn retire_instructions(path: &Path) -> Result<bool, SyncError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if crate::count_instructions_blocks(&text) == 0 {
        return Ok(false);
    }
    let rest = crate::remove_instructions_block(&text).ok_or_else(|| {
        SyncError::Blocked(format!(
            "{}: wt block markers are damaged or repeated",
            path.display()
        ))
    })?;
    let linked = fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink());
    if rest.trim().is_empty() && !linked {
        fs::remove_file(path)?;
    } else {
        atomic_write(path, rest.as_bytes())?;
    }
    Ok(true)
}

pub fn sync(reports: &[UnitReport], mode: SyncMode) -> Result<SyncSummary, SyncError> {
    let mut summary = SyncSummary::default();
    // Two targets can link to one source file; write it once.
    let mut written = std::collections::BTreeSet::new();
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
        if !written.insert(write_target(&r.path).unwrap_or_else(|_| r.path.clone())) {
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
    use std::path::Path;
    #[test]
    fn retiring_removes_only_the_block_and_an_emptied_file() {
        let d = tempfile::tempdir().unwrap();
        let only = d.path().join("CLAUDE.md");
        fs::write(&only, crate::splice_instructions_block("", "RULES")).unwrap();
        assert_eq!(inspect_retired(&only), RetiredInstructions::Pending);
        assert!(retire_instructions(&only).unwrap());
        assert!(!only.exists());
        assert_eq!(inspect_retired(&only), RetiredInstructions::Clean);
        assert!(!retire_instructions(&only).unwrap());

        let shared = d.path().join("shared.md");
        fs::write(&shared, crate::splice_instructions_block("mine\n", "RULES")).unwrap();
        assert!(retire_instructions(&shared).unwrap());
        assert_eq!(fs::read_to_string(&shared).unwrap(), "mine\n");

        let damaged = d.path().join("damaged.md");
        let text = crate::splice_instructions_block("", "RULES")
            .replace("<!-- wt:instructions:end -->", "");
        fs::write(&damaged, &text).unwrap();
        assert!(matches!(
            inspect_retired(&damaged),
            RetiredInstructions::Blocked(_)
        ));
        assert!(retire_instructions(&damaged).is_err());
        assert_eq!(fs::read_to_string(&damaged).unwrap(), text);
    }

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

    /// Dotfiles link skills per file: the skill directory is real, its
    /// SKILL.md links into a durable source that must receive the update.
    #[cfg(unix)]
    #[test]
    fn sync_writes_through_a_per_file_skill_symlink() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("dotfiles/wt/SKILL.md");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, stamp_content("older bundle")).unwrap();
        let root = d.path().join("skills");
        let link = root.join("wt/SKILL.md");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&source, &link).unwrap();
        let targets = Targets {
            skills: vec![SkillsTarget::Native {
                harnesses: vec![],
                dir: root,
            }],
            ..Default::default()
        };
        let reports = build_reports(&targets, &SkillsMemory::default());
        let report = reports.iter().find(|r| r.unit.name == "wt").unwrap();
        assert_eq!(report.state, UnitState::Outdated);
        sync(&reports, SyncMode::Safe).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&link).unwrap(), source);
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            stamp_content(&report.expected)
        );
        let after = build_reports(&targets, &SkillsMemory::default());
        let report = after.iter().find(|r| r.unit.name == "wt").unwrap();
        assert_eq!(report.state, UnitState::Fresh);
    }

    #[cfg(unix)]
    #[test]
    fn sync_writes_through_an_instructions_symlink() {
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("dotfiles/AGENTS.md");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "user text\n").unwrap();
        // A relative link, as dotfiles managers usually create.
        let home = d.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let link = home.join("AGENTS.md");
        std::os::unix::fs::symlink("../dotfiles/AGENTS.md", &link).unwrap();
        let targets = Targets {
            instructions: vec![InstructionsTarget::Native {
                harnesses: vec![],
                file: link.clone(),
            }],
            ..Default::default()
        };
        let reports = build_reports(&targets, &SkillsMemory::default());
        assert_eq!(reports[0].state, UnitState::Missing);
        sync(&reports, SyncMode::Safe).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new("../dotfiles/AGENTS.md")
        );
        let text = fs::read_to_string(&source).unwrap();
        assert!(text.starts_with("user text\n") && text.contains("wt:instructions:begin"));
        // No temp file left beside either path.
        for dir in [&home, &source.parent().unwrap().to_path_buf()] {
            assert!(
                fs::read_dir(dir).unwrap().all(|e| !e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp"))
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn dangling_skill_symlink_stays_blocked_and_untouched() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("skills");
        let link = root.join("wt/SKILL.md");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        let missing = d.path().join("missing/SKILL.md");
        std::os::unix::fs::symlink(&missing, &link).unwrap();
        let targets = Targets {
            skills: vec![SkillsTarget::Native {
                harnesses: vec![],
                dir: root,
            }],
            ..Default::default()
        };
        let reports = build_reports(&targets, &SkillsMemory::default());
        let report = reports.iter().find(|r| r.unit.name == "wt").unwrap();
        assert_eq!(report.state, UnitState::Blocked);
        sync(&reports, SyncMode::Force).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), missing);
        assert!(!missing.exists());
    }
}
