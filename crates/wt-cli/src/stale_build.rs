//! A session keeps the wt build that started it (docs/updates.md), so a fix
//! in a newer install never reaches a running agent on its own. This tells
//! that agent, once an hour, where the installed version is.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use wt_update::{InstallPaths, StateStore};

const REPEAT_AFTER: Duration = Duration::from_secs(60 * 60);

/// Print the notice to stderr when it applies to this invocation.
pub fn warn(args: &[OsString], running_version: &str, running_build: &str) {
    if !wants_notice(args) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(text) = notice(&exe, running_version, running_build) else {
        return;
    };
    let pane = std::env::var("TMUX_PANE").unwrap_or_default();
    if claim(&stamp_path(&std::env::temp_dir(), running_build, &pane)) {
        eprintln!("{text}");
    }
}

/// The board, wt's own hidden workers, machine-read output and a harness hook
/// stay silent: a line on stderr there is noise at best and breaks a `2>&1`
/// JSON parse at worst.
fn wants_notice(args: &[OsString]) -> bool {
    let mut args = args.iter().skip(1).map(|arg| arg.to_string_lossy());
    let Some(command) = args.next() else {
        return false;
    };
    !command.starts_with(['_', '-']) && !args.any(|arg| arg == "--json" || arg == "--claude-hook")
}

/// The notice for a binary run from an install's `versions` directory while
/// that install's current version is a different build.
fn notice(exe: &Path, running_version: &str, running_build: &str) -> Option<String> {
    let versions = exe.parent()?.parent()?.parent()?;
    if versions.file_name()? != "versions" {
        return None;
    }
    let paths = InstallPaths::new(versions.parent()?).ok()?;
    let current = StateStore::new(paths.clone()).load().ok()?.current?;
    if current.build_id() == running_build {
        return None;
    }
    Some(format!(
        "wt: this session runs the wt that started it ({running_version}); {} is installed. Run {} for it, or restart the session.",
        current.release_version(),
        paths.launcher().display()
    ))
}

fn stamp_path(dir: &Path, running_build: &str, pane: &str) -> PathBuf {
    let pane: String = pane.chars().filter(char::is_ascii_alphanumeric).collect();
    let build: String = running_build.chars().take(12).collect();
    dir.join(format!("wt-stale-build-{build}-{pane}"))
}

/// True when no notice was printed for this stamp within the repeat window;
/// it then records this one.
fn claim(stamp: &Path) -> bool {
    let recent = std::fs::metadata(stamp)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_some_and(|age| age < REPEAT_AFTER);
    if recent {
        return false;
    }
    let _ = std::fs::write(stamp, b"");
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_update::{Channel, InstallState, VersionId};

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn only_a_pinned_binary_behind_the_installed_version_is_told() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let paths = InstallPaths::new(&root_path).unwrap();
        let new = VersionId::new("v0.2.0", "newbuild", "aarch64-apple-darwin").unwrap();
        let mut state = InstallState::new(Channel::Stable);
        state.current = Some(new.clone());
        std::fs::create_dir_all(root_path.join("versions")).unwrap();
        StateStore::new(paths.clone()).save(&state).unwrap();

        let old_exe = root_path.join("versions/old/bin/wt");
        let text = notice(&old_exe, "0.1.0", "oldbuild").unwrap();
        assert!(text.contains("v0.2.0 is installed"));
        assert!(text.contains(&paths.launcher().display().to_string()));

        assert_eq!(notice(&paths.app_binary(&new), "0.2.0", "newbuild"), None);
        // A development build is not part of any install.
        assert_eq!(
            notice(&root_path.join("target/debug/wt"), "0.1.0", "oldbuild"),
            None
        );
    }

    #[test]
    fn the_board_workers_and_json_output_stay_silent_and_the_notice_is_hourly() {
        assert!(wants_notice(&args(&["wt", "ls"])));
        assert!(!wants_notice(&args(&["wt"])));
        assert!(!wants_notice(&args(&["wt", "_claude_hook", "stop"])));
        assert!(!wants_notice(&args(&["wt", "ls", "--json"])));
        assert!(wants_notice(&args(&["wt", "mark", "note"])));
        assert!(!wants_notice(&args(&["wt", "mark", "--claude-hook"])));
        assert!(!wants_notice(&args(&["wt", "--version"])));

        let dir = tempfile::tempdir().unwrap();
        let stamp = stamp_path(dir.path(), "0123456789abcdef", "%12");
        assert!(stamp.ends_with("wt-stale-build-0123456789ab-12"));
        assert!(claim(&stamp));
        assert!(!claim(&stamp));
    }
}
