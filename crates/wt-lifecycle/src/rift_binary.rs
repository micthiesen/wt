//! Locate Rift without depending on an interactive launcher's PATH.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::fs;
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessRunner};

use crate::LifecycleError;

pub(crate) async fn resolve(
    configured: &OsStr,
    shell: &OsStr,
    cwd: &Path,
    search_path: &OsStr,
    runner: &ProcessRunner,
    cancellation: &CancellationToken,
) -> Result<OsString, LifecycleError> {
    if configured != OsStr::new("rift") {
        return Ok(configured.to_owned());
    }
    for directory in std::env::split_paths(search_path) {
        if cancellation.is_cancelled() {
            return Err(LifecycleError::Cancelled);
        }
        let candidate = cwd.join(directory).join("rift");
        if executable(&candidate).await {
            return Ok(candidate.into_os_string());
        }
    }

    // The command is constant, never assembled from a branch, path or config
    // value. Only discovery uses a login shell; Rift mutations remain argv-based.
    let mut spec = CommandSpec::new(shell);
    spec.args = ["-lc", "command -v rift"]
        .into_iter()
        .map(Into::into)
        .collect();
    spec.cwd = Some(cwd.to_owned());
    spec.env.push(("PATH".into(), Some(search_path.to_owned())));
    spec.timeout = Duration::from_secs(10);
    spec.output_limit = 16 * 1024;
    let output =
        runner
            .run(spec, cancellation)
            .await
            .map_err(|source| LifecycleError::Process {
                operation: "find Rift in login shell",
                path: cwd.to_owned(),
                source,
            })?;
    if output.status.success() && !output.stdout_truncated {
        // Login profiles can print a greeting. `command -v` is the final line;
        // reject aliases/functions and require an actual executable file.
        if let Some(line) = output.stdout_text().lines().last() {
            let candidate = PathBuf::from(line.trim());
            if candidate.is_absolute() && executable(&candidate).await {
                return Ok(candidate.into_os_string());
            }
        }
    }
    Err(LifecycleError::Refused(
        "Rift executable was not found on PATH or through the login shell; install Rift or set [backend] kind = \"git-worktree\"".into(),
    ))
}

async fn executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path).await else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    async fn script(path: &Path, content: &str) {
        fs::write(path, content).await.unwrap();
        fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn path_wins_and_login_shell_recovers_a_missing_path() {
        let scratch = tempfile::tempdir().unwrap();
        let directory = scratch.path().canonicalize().unwrap();
        let directory = directory.as_path();
        let binary = directory.join("rift");
        script(&binary, "#!/bin/sh\nexit 0\n").await;
        let shell = directory.join("login-shell");
        script(&shell, "#!/bin/sh\n[ \"$1\" = -lc ] || exit 3\n[ \"$2\" = 'command -v rift' ] || exit 4\nprintf 'greeting\\n%s/rift\\n' \"$PWD\"\n").await;
        let runner = ProcessRunner::new(std::num::NonZeroUsize::new(1).unwrap());
        let cancel = CancellationToken::new();
        let direct = resolve(
            OsStr::new("rift"),
            OsStr::new("missing-shell"),
            directory,
            directory.as_os_str(),
            &runner,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(direct, binary.as_os_str());
        let fallback = resolve(
            OsStr::new("rift"),
            shell.as_os_str(),
            directory,
            OsStr::new("/absent"),
            &runner,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(fallback, binary.as_os_str());
        script(&shell, "#!/bin/sh\nprintf 'rift is a shell function\\n'\n").await;
        assert!(
            resolve(
                OsStr::new("rift"),
                shell.as_os_str(),
                directory,
                OsStr::new("/absent"),
                &runner,
                &cancel
            )
            .await
            .is_err()
        );
    }
}
