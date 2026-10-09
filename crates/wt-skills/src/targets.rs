use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use wt_core::HarnessId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentToolId {
    Claude,
    Codex,
    Opencode,
    Pi,
}
impl AgentToolId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Pi => "pi",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesyncInfo {
    pub root: PathBuf,
    pub skills_dir: PathBuf,
    pub root_rule_file: Option<PathBuf>,
    pub regen: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillsTarget {
    Native {
        harnesses: Vec<AgentToolId>,
        dir: PathBuf,
    },
    Rulesync {
        harnesses: Vec<AgentToolId>,
        rulesync: RulesyncInfo,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionsTarget {
    Native {
        harnesses: Vec<AgentToolId>,
        file: PathBuf,
    },
    Rulesync {
        harnesses: Vec<AgentToolId>,
        rulesync: RulesyncInfo,
    },
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Targets {
    pub skills: Vec<SkillsTarget>,
    pub instructions: Vec<InstructionsTarget>,
    pub harnesses: Vec<AgentToolId>,
}
#[derive(Clone, Debug, Default)]
pub struct TargetOptions {
    pub home: PathBuf,
    pub codex_home: Option<PathBuf>,
    pub pi_dir: Option<PathBuf>,
}

pub fn target_key(target: &impl TargetPath) -> String {
    target.rulesync().map_or_else(
        || format!("native:{}", target.path().display()),
        |r| format!("rulesync:{}", r.root.display()),
    )
}
pub trait TargetPath {
    fn path(&self) -> &Path;
    fn rulesync(&self) -> Option<&RulesyncInfo>;
}
impl TargetPath for SkillsTarget {
    fn path(&self) -> &Path {
        match self {
            Self::Native { dir, .. } => dir,
            Self::Rulesync { rulesync, .. } => &rulesync.skills_dir,
        }
    }
    fn rulesync(&self) -> Option<&RulesyncInfo> {
        if let Self::Rulesync { rulesync, .. } = self {
            Some(rulesync)
        } else {
            None
        }
    }
}
impl TargetPath for InstructionsTarget {
    fn path(&self) -> &Path {
        match self {
            Self::Native { file, .. } => file,
            Self::Rulesync { rulesync, .. } => {
                rulesync.root_rule_file.as_deref().unwrap_or(&rulesync.root)
            }
        }
    }
    fn rulesync(&self) -> Option<&RulesyncInfo> {
        if let Self::Rulesync { rulesync, .. } = self {
            Some(rulesync)
        } else {
            None
        }
    }
}

fn configured(path: &Path) -> bool {
    fs::read_dir(path)
        .ok()
        .and_then(|mut it| it.next())
        .is_some_and(|entry| entry.is_ok())
}
fn resolve(path: &Path) -> PathBuf {
    if let Ok(p) = fs::canonicalize(path) {
        return p;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(p) = fs::canonicalize(parent)
    {
        return p.join(name);
    }
    path.to_path_buf()
}
fn rulesync_root(path: &Path) -> Option<PathBuf> {
    let mut d = path.to_path_buf();
    for _ in 0..12 {
        if d.join(".rulesync").is_dir() {
            return Some(d);
        }
        if !d.pop() {
            break;
        }
    }
    None
}
fn root_rule(root: &Path) -> Option<PathBuf> {
    let mut entries = fs::read_dir(root.join(".rulesync/rules"))
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect::<Vec<_>>();
    entries.sort();
    entries
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .find(|p| {
            fs::read_to_string(p).ok().is_some_and(|s| {
                s.split("---")
                    .nth(1)
                    .is_some_and(|fm| fm.lines().any(|l| l.trim() == "root: true"))
            })
        })
}
fn rs_info(root: PathBuf) -> RulesyncInfo {
    let script = root.join("scripts/rulesync.sh");
    let regen = if script.is_file() {
        vec!["bash".into(), script.to_string_lossy().into_owned()]
    } else {
        vec![
            "npx".into(),
            "-y".into(),
            "rulesync".into(),
            "generate".into(),
        ]
    };
    RulesyncInfo {
        skills_dir: root.join(".rulesync/skills"),
        root_rule_file: root_rule(&root),
        root,
        regen,
    }
}
fn dedupe_skills(candidates: Vec<(AgentToolId, PathBuf)>) -> Vec<SkillsTarget> {
    let mut map: BTreeMap<String, SkillsTarget> = BTreeMap::new();
    for (h, p) in candidates {
        let root = rulesync_root(&p);
        let key = root.as_ref().map_or_else(
            || format!("native:{}", p.display()),
            |r| format!("rulesync:{}", r.display()),
        );
        let entry = map.entry(key).or_insert_with(|| {
            root.map_or_else(
                || SkillsTarget::Native {
                    harnesses: vec![],
                    dir: p.clone(),
                },
                |r| SkillsTarget::Rulesync {
                    harnesses: vec![],
                    rulesync: rs_info(r),
                },
            )
        });
        let hs = match entry {
            SkillsTarget::Native { harnesses, .. } | SkillsTarget::Rulesync { harnesses, .. } => {
                harnesses
            }
        };
        if !hs.contains(&h) {
            hs.push(h)
        }
    }
    map.into_values().collect()
}
fn dedupe_ins(candidates: Vec<(AgentToolId, PathBuf)>) -> Vec<InstructionsTarget> {
    let mut map: BTreeMap<String, InstructionsTarget> = BTreeMap::new();
    for (h, p) in candidates {
        let root = rulesync_root(&p);
        let key = root.as_ref().map_or_else(
            || format!("native:{}", p.display()),
            |r| format!("rulesync:{}", r.display()),
        );
        let entry = map.entry(key).or_insert_with(|| {
            root.map_or_else(
                || InstructionsTarget::Native {
                    harnesses: vec![],
                    file: p.clone(),
                },
                |r| InstructionsTarget::Rulesync {
                    harnesses: vec![],
                    rulesync: rs_info(r),
                },
            )
        });
        let hs = match entry {
            InstructionsTarget::Native { harnesses, .. }
            | InstructionsTarget::Rulesync { harnesses, .. } => harnesses,
        };
        if !hs.contains(&h) {
            hs.push(h)
        }
    }
    map.into_values().collect()
}
pub fn detect_targets(o: &TargetOptions) -> Targets {
    let home = &o.home;
    let codex = o.codex_home.clone().unwrap_or_else(|| home.join(".codex"));
    let pi = o.pi_dir.clone().or_else(|| {
        [home.join(".pi"), home.join(".config/pi")]
            .into_iter()
            .find(|p| configured(p))
    });
    let mut hs = vec![];
    if configured(&home.join(".claude")) {
        hs.push(AgentToolId::Claude)
    }
    if configured(&codex) || configured(&home.join(".agents")) {
        hs.push(AgentToolId::Codex)
    }
    if configured(&home.join(".config/opencode")) {
        hs.push(AgentToolId::Opencode)
    }
    if pi.as_ref().is_some_and(|p| configured(p)) {
        hs.push(AgentToolId::Pi)
    }
    let mut skills = vec![];
    let mut ins = vec![];
    for h in &hs {
        let (dir, file) = match h {
            AgentToolId::Claude => (home.join(".claude/skills"), home.join(".claude/CLAUDE.md")),
            AgentToolId::Codex => (
                if home.join(".agents").exists() {
                    home.join(".agents/skills")
                } else {
                    codex.join("skills")
                },
                codex.join("AGENTS.md"),
            ),
            AgentToolId::Opencode => (
                home.join(".claude/skills"),
                home.join(".config/opencode/AGENTS.md"),
            ),
            AgentToolId::Pi => {
                let p = pi.as_ref().unwrap();
                (
                    if home.join(".agents").exists() {
                        home.join(".agents/skills")
                    } else {
                        p.join("skills")
                    },
                    p.join("AGENTS.md"),
                )
            }
        };
        skills.push((*h, resolve(&dir)));
        ins.push((*h, resolve(&file)));
    }
    Targets {
        skills: dedupe_skills(skills),
        instructions: dedupe_ins(ins),
        harnesses: hs,
    }
}
pub fn harness_skill_dirs(h: HarnessId, o: &TargetOptions) -> Vec<PathBuf> {
    match h {
        HarnessId::Claude | HarnessId::Opencode => vec![resolve(&o.home.join(".claude/skills"))],
        HarnessId::Codex => {
            let preferred = if o.home.join(".agents").exists() {
                o.home.join(".agents/skills")
            } else {
                o.codex_home
                    .clone()
                    .unwrap_or_else(|| o.home.join(".codex"))
                    .join("skills")
            };
            let fallback = o
                .codex_home
                .clone()
                .unwrap_or_else(|| o.home.join(".codex"))
                .join("skills");
            if resolve(&preferred) == resolve(&fallback) {
                vec![resolve(&preferred)]
            } else {
                vec![resolve(&preferred), resolve(&fallback)]
            }
        }
    }
}
pub fn harness_can_resolve_skill(h: HarnessId, name: &str, o: &TargetOptions) -> bool {
    harness_skill_dirs(h, o).iter().any(|d| {
        fs::read_to_string(d.join(name).join("SKILL.md")).is_ok_and(|s| !s.trim().is_empty())
    })
}
pub fn no_tools_hint() -> &'static str {
    "no configured coding-agent dirs found (~/.claude, ~/.codex or ~/.agents, ~/.config/opencode, ~/.pi)"
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn symlinked_skills_and_rulesync_are_deduped() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path();
        let c = h.join(".claude");
        fs::create_dir_all(c.join("skills")).unwrap();
        fs::write(c.join("settings.json"), "{}").unwrap();
        let a = h.join(".agents");
        std::os::unix::fs::symlink(&c, &a).unwrap();
        let root = h.join("repo");
        fs::create_dir_all(root.join(".rulesync/rules")).unwrap();
        fs::create_dir_all(root.join(".rulesync/skills")).unwrap();
        fs::write(
            root.join(".rulesync/rules/root.md"),
            "---\nroot: true\n---\n",
        )
        .unwrap();
        fs::remove_dir(c.join("skills")).unwrap();
        std::os::unix::fs::symlink(root.join(".rulesync/skills"), c.join("skills")).unwrap();
        let targets = detect_targets(&TargetOptions {
            home: h.into(),
            ..Default::default()
        });
        assert_eq!(targets.skills.len(), 1);
        assert!(matches!(targets.skills[0], SkillsTarget::Rulesync { .. }));
    }
}
