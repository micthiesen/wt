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
    /// How Claude Code gets the instructions block, when Claude is configured.
    pub claude_instructions: Option<ClaudeInstructionsMode>,
}
/// Whether wt writes its block into `~/.claude/CLAUDE.md`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeInstructions {
    /// Write unless Claude Code already reads `~/.codex/AGENTS.md`.
    #[default]
    Auto,
    Write,
    Skip,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeInstructionsMode {
    pub setting: ClaudeInstructions,
    pub write: bool,
    pub reason: String,
    /// The CLAUDE.md that sync should clear of wt's block when skipping. None
    /// when it resolves to a file wt still writes the block into.
    pub retired: Option<PathBuf>,
}
#[derive(Clone, Debug, Default)]
pub struct TargetOptions {
    pub home: PathBuf,
    pub codex_home: Option<PathBuf>,
    pub pi_dir: Option<PathBuf>,
    pub claude_instructions: ClaudeInstructions,
}

/// The `agents` mod serves the Codex-native layout, including
/// `~/.codex/AGENTS.md`, to Claude Code. It is registered as a plugin
/// directory in Claude's user settings.
fn agents_mod_dir(home: &Path) -> Option<PathBuf> {
    let raw = fs::read_to_string(home.join(".claude/settings.json")).ok()?;
    let settings: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let dirs = settings
        .get("env")?
        .get("CLAUDE_CODE_PLUGIN_DIRS")?
        .as_str()?;
    dirs.split([':', ','])
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .map(|dir| match dir.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None if dir == "~" => home.to_path_buf(),
            None => PathBuf::from(dir),
        })
        .find(|dir| {
            fs::read_to_string(dir.join(".claude-plugin/plugin.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .is_some_and(|plugin| plugin.get("name").and_then(|n| n.as_str()) == Some("agents"))
        })
}

fn claude_instructions_mode(o: &TargetOptions, codex_configured: bool) -> (bool, String) {
    match o.claude_instructions {
        ClaudeInstructions::Write => (true, "[skills] claude_instructions = \"write\"".into()),
        ClaudeInstructions::Skip => (false, "[skills] claude_instructions = \"skip\"".into()),
        ClaudeInstructions::Auto => match agents_mod_dir(&o.home) {
            Some(dir) if codex_configured => (
                false,
                format!(
                    "auto: the agents mod at {} loads the Codex AGENTS.md",
                    dir.display()
                ),
            ),
            _ => (true, "auto: Claude Code reads only CLAUDE.md".into()),
        },
    }
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
        [
            home.join(".pi/agent"),
            home.join(".pi"),
            home.join(".config/pi"),
        ]
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
    let mut claude = None;
    if hs.contains(&AgentToolId::Claude) {
        let (write, reason) = claude_instructions_mode(o, hs.contains(&AgentToolId::Codex));
        claude = Some(ClaudeInstructionsMode {
            setting: o.claude_instructions,
            write,
            reason,
            retired: None,
        });
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
                if home.join(".agents").exists() {
                    home.join(".agents/skills")
                } else {
                    home.join(".config/opencode/skills")
                },
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
        match &mut claude {
            Some(mode) if *h == AgentToolId::Claude && !mode.write => {
                mode.retired = Some(resolve(&file));
            }
            _ => ins.push((*h, resolve(&file))),
        }
    }
    let instructions = dedupe_ins(ins);
    // A CLAUDE.md linked to a file another tool reads keeps its block.
    if let Some(mode) = &mut claude
        && mode
            .retired
            .as_ref()
            .is_some_and(|file| instructions.iter().any(|t| t.path() == file))
    {
        mode.retired = None;
    }
    Targets {
        skills: dedupe_skills(skills),
        instructions,
        harnesses: hs,
        claude_instructions: claude,
    }
}
pub fn harness_skill_dirs(h: HarnessId, o: &TargetOptions) -> Vec<PathBuf> {
    match h {
        HarnessId::Claude => vec![resolve(&o.home.join(".claude/skills"))],
        HarnessId::Opencode => vec![
            resolve(&o.home.join(".config/opencode/skills")),
            resolve(&o.home.join(".agents/skills")),
            resolve(&o.home.join(".claude/skills")),
        ],
        HarnessId::Pi => vec![
            resolve(&o.home.join(".agents/skills")),
            resolve(
                &o.pi_dir
                    .clone()
                    .unwrap_or_else(|| o.home.join(".pi/agent"))
                    .join("skills"),
            ),
        ],
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

    fn claude_and_codex(home: &Path) {
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/CLAUDE.md"), "x").unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(home.join(".codex/AGENTS.md"), "x").unwrap();
    }

    fn install_agents_mod(home: &Path) {
        let dir = home.join("dotfiles/mod");
        fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
        fs::write(
            dir.join(".claude-plugin/plugin.json"),
            r#"{"name":"agents"}"#,
        )
        .unwrap();
        fs::write(
            home.join(".claude/settings.json"),
            r#"{"env":{"CLAUDE_CODE_PLUGIN_DIRS":"~/elsewhere:~/dotfiles/mod"}}"#,
        )
        .unwrap();
    }

    fn options(home: &Path, setting: ClaudeInstructions) -> TargetOptions {
        TargetOptions {
            home: home.to_path_buf(),
            claude_instructions: setting,
            ..Default::default()
        }
    }

    fn claude_md_targeted(targets: &Targets) -> bool {
        targets
            .instructions
            .iter()
            .any(|t| t.path().ends_with(".claude/CLAUDE.md"))
    }

    #[test]
    fn agents_mod_moves_claude_onto_the_codex_block_unless_overridden() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        claude_and_codex(home);

        let plain = detect_targets(&options(home, ClaudeInstructions::Auto));
        assert!(claude_md_targeted(&plain));
        assert!(plain.claude_instructions.as_ref().unwrap().write);

        install_agents_mod(home);
        let auto = detect_targets(&options(home, ClaudeInstructions::Auto));
        let mode = auto.claude_instructions.clone().unwrap();
        assert!(!claude_md_targeted(&auto) && !mode.write);
        assert!(mode.reason.contains("dotfiles/mod"), "{}", mode.reason);
        assert!(mode.retired.unwrap().ends_with(".claude/CLAUDE.md"));
        assert!(auto.harnesses.contains(&AgentToolId::Claude));
        assert!(
            auto.skills
                .iter()
                .any(|t| t.path().ends_with(".claude/skills"))
        );

        let forced = detect_targets(&options(home, ClaudeInstructions::Write));
        assert!(claude_md_targeted(&forced));
        assert!(forced.claude_instructions.unwrap().retired.is_none());
    }

    #[test]
    fn skip_never_retires_a_claude_md_linked_to_a_kept_target() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        claude_and_codex(home);
        fs::remove_file(home.join(".claude/CLAUDE.md")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            home.join(".codex/AGENTS.md"),
            home.join(".claude/CLAUDE.md"),
        )
        .unwrap();
        let skip = detect_targets(&options(home, ClaudeInstructions::Skip));
        let mode = skip.claude_instructions.unwrap();
        assert!(!mode.write);
        assert!(mode.retired.is_none());
        assert!(
            skip.instructions
                .iter()
                .any(|t| t.path().ends_with(".codex/AGENTS.md"))
        );
    }

    #[test]
    fn opencode_uses_native_or_shared_skills_without_creating_claude_layout() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join(".config/opencode")).unwrap();
        fs::write(t.path().join(".config/opencode/opencode.json"), "{}").unwrap();
        let options = TargetOptions {
            home: t.path().into(),
            ..Default::default()
        };
        let native = t.path().join(".config/opencode/skills/start");
        fs::create_dir_all(&native).unwrap();
        fs::write(native.join("SKILL.md"), "start").unwrap();
        assert!(harness_can_resolve_skill(
            HarnessId::Opencode,
            "start",
            &options
        ));
        assert_eq!(
            crate::start_skill_invocation(HarnessId::Opencode).unwrap(),
            "@start"
        );
        assert!(
            format!("{:?}", detect_targets(&options).skills).contains(".config/opencode/skills")
        );
        fs::create_dir_all(t.path().join(".agents")).unwrap();
        assert!(format!("{:?}", detect_targets(&options).skills).contains(".agents/skills"));
        assert!(!t.path().join(".claude").exists());
    }

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
