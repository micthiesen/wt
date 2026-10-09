use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

pub(crate) const REPOSITORY_CONFIG: &str = ".wt.toml";
pub(crate) const REPOSITORY_CONFIG_ENV: &str = "WT_REPO_CONFIG";

pub(crate) fn env<'a>(env: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key).map(String::as_str)
}

pub(crate) fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

pub(crate) fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    lexical_normalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

fn lexical_normalize(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub(crate) fn canonical(path: &Path, cwd: &Path) -> PathBuf {
    let abs = absolute(path, cwd);
    fs::canonicalize(&abs).unwrap_or(abs)
}

pub(crate) fn is_inside(parent: &Path, child: &Path, cwd: &Path) -> bool {
    let parent = canonical(parent, cwd);
    let child = canonical(child, cwd);
    child
        .strip_prefix(parent)
        .is_ok_and(|relative| !relative.as_os_str().is_empty())
}

/// Find the nearest repository config, falling back to a linked worktree's
/// main clone only when walking to the filesystem root found no config.
pub(crate) fn repository_config(
    cwd: &Path,
    environment: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    if let Some(path) = env(environment, REPOSITORY_CONFIG_ENV).filter(|p| !p.is_empty()) {
        return Some(absolute(Path::new(path), cwd));
    }
    let mut current = absolute(cwd, cwd);
    let mut git_root = None;
    loop {
        let candidate = current.join(REPOSITORY_CONFIG);
        if candidate.is_file() {
            return Some(candidate);
        }
        if git_root.is_none() && current.join(".git").exists() {
            git_root = Some(current.clone());
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    git_root.and_then(linked_main_config)
}

fn linked_main_config(worktree: PathBuf) -> Option<PathBuf> {
    let pointer = fs::read_to_string(worktree.join(".git")).ok()?;
    let gitdir = pointer.trim().strip_prefix("gitdir:")?.trim();
    let gitdir = absolute(Path::new(gitdir), &worktree);
    let common = fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = absolute(Path::new(common.trim()), &gitdir);
    if common.file_name()?.to_string_lossy() != ".git" {
        return None;
    }
    let candidate = common.parent()?.join(REPOSITORY_CONFIG);
    candidate.is_file().then_some(candidate)
}

pub(crate) fn merge(base: &mut toml::Value, overlay: &toml::Value) {
    let (Some(base), Some(overlay)) = (base.as_table_mut(), overlay.as_table()) else {
        *base = overlay.clone();
        return;
    };
    for (key, value) in overlay {
        match (base.get_mut(key), value) {
            (Some(existing), toml::Value::Table(_)) if existing.is_table() => {
                merge(existing, value)
            }
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Stable path-derived name used for repository state partitioning.
pub(crate) fn path_namespace(path: &Path, home: &Path, cwd: &Path) -> String {
    let canonical = canonical(path, cwd);
    let home = absolute(home, cwd);
    let identity = canonical
        .strip_prefix(&home)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .unwrap_or_else(|| canonical.strip_prefix(Path::new("/")).unwrap_or(&canonical));
    let normalized = identity
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let normalized: String = normalized.nfkd().collect::<String>().to_lowercase();
    let mut slug = String::new();
    let mut dash = false;
    for ch in normalized.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            dash = false;
        } else if !slug.is_empty() && !dash {
            slug.push('-');
            dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() { "repo".into() } else { slug }
}

pub(crate) fn repository_namespace(config: &Path, home: &Path, cwd: &Path) -> String {
    let config = canonical(config, cwd);
    let directory = config.parent().unwrap_or(Path::new("."));
    path_namespace(directory, home, cwd)
}

pub(crate) fn canonical_repository_config(
    discovered: Option<PathBuf>,
    main_clone: &Path,
    worktree_root: &Path,
    cwd: &Path,
) -> Option<PathBuf> {
    if main_clone.as_os_str().is_empty() {
        return discovered;
    }
    let own = canonical(main_clone, cwd).join(REPOSITORY_CONFIG);
    let own_exists = own.is_file();
    let Some(discovered) = discovered else {
        return own_exists.then_some(own);
    };
    if worktree_root.as_os_str().is_empty() {
        return Some(discovered);
    }
    let parent = discovered.parent().unwrap_or(Path::new("."));
    if is_inside(worktree_root, parent, cwd) {
        own_exists.then_some(own).or(Some(discovered))
    } else {
        Some(discovered)
    }
}
