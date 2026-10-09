use std::path::{Path, PathBuf};
use uuid::{Uuid, uuid};

const WT_SESSION_NAMESPACE: Uuid = uuid!("ad7c39f4-4b63-4d1c-9b9a-66c44e5a1e58");

/// Stable UUIDv5 identity shared with wt's Claude sessions.
pub fn claude_session_id(worktree_path: &Path, managed_name: Option<&str>) -> String {
    let key = match managed_name {
        Some(name) => format!("{} {name}", worktree_path.to_string_lossy()),
        None => worktree_path.to_string_lossy().into_owned(),
    };
    Uuid::new_v5(&WT_SESSION_NAMESPACE, key.as_bytes()).to_string()
}

/// Claude slugifies every non-ASCII-alphanumeric character in cwd to `-`.
pub fn project_dir(home: &Path, worktree_path: &Path) -> PathBuf {
    let slug: String = worktree_path
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    home.join(".claude").join("projects").join(slug)
}

pub fn session_jsonl_path(home: &Path, worktree_path: &Path, session_id: &str) -> PathBuf {
    project_dir(home, worktree_path).join(format!("{session_id}.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v5_matches_stable_identity_inputs() {
        let path = Path::new("/Users/michael/.wt");
        assert_eq!(
            claude_session_id(path, None),
            Uuid::new_v5(&WT_SESSION_NAMESPACE, b"/Users/michael/.wt").to_string()
        );
        assert_ne!(
            claude_session_id(path, None),
            claude_session_id(path, Some("review"))
        );
        assert_ne!(
            claude_session_id(path, Some("review")),
            claude_session_id(Path::new("/other"), Some("review"))
        );
    }

    #[test]
    fn project_slug_collapses_all_non_ascii_alphanumeric_characters() {
        assert_eq!(
            project_dir(Path::new("/home/u"), Path::new("/a_b/c.d")),
            PathBuf::from("/home/u/.claude/projects/-a-b-c-d")
        );
    }
}
