use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessRunner};

use crate::repository::RepositoryError;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatus {
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub upstream: Option<String>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub tracked_changes: u32,
    pub untracked_files: u32,
    pub dirty: bool,
}

pub(crate) async fn read_status(
    runner: &ProcessRunner,
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<GitStatus, RepositoryError> {
    let mut spec = CommandSpec::new("git").args(["status", "--porcelain=v2", "--branch", "-z"]);
    spec.cwd = Some(path.to_path_buf());
    spec.env = common_git_env();
    let output = runner
        .run(spec, cancellation)
        .await
        .map_err(|source| RepositoryError::Process {
            operation: "read Git status",
            path: path.to_path_buf(),
            source,
        })?
        .checked("git")
        .map_err(|source| RepositoryError::Process {
            operation: "read Git status",
            path: path.to_path_buf(),
            source,
        })?;
    parse_status(&output.stdout).map_err(|message| RepositoryError::Parse {
        operation: "parse Git status",
        path: path.to_path_buf(),
        message,
    })
}

pub(crate) fn common_git_env() -> Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> {
    vec![
        ("GIT_OPTIONAL_LOCKS".into(), Some("0".into())),
        ("GIT_TERMINAL_PROMPT".into(), Some("0".into())),
        ("LC_ALL".into(), Some("C".into())),
    ]
}

fn parse_status(bytes: &[u8]) -> Result<GitStatus, String> {
    let mut status = GitStatus::default();
    let mut fields = bytes.split(|byte| *byte == 0).peekable();
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let record = std::str::from_utf8(record).map_err(|_| "status record is not UTF-8")?;
        if let Some(header) = record.strip_prefix("# ") {
            if let Some(value) = header.strip_prefix("branch.oid ") {
                if value != "(initial)" {
                    status.head_sha = Some(value.to_owned());
                }
            } else if let Some(value) = header.strip_prefix("branch.head ") {
                if value != "(detached)" && value != "(unknown)" {
                    status.branch = Some(value.to_owned());
                }
            } else if let Some(value) = header.strip_prefix("branch.upstream ") {
                status.upstream = Some(value.to_owned());
            } else if let Some(value) = header.strip_prefix("branch.ab ") {
                let mut pair = value.split_ascii_whitespace();
                status.ahead = Some(signed_count(pair.next(), '+')?);
                status.behind = Some(signed_count(pair.next(), '-')?);
            }
            continue;
        }
        match record.as_bytes().first().copied() {
            Some(b'1') | Some(b'2') | Some(b'u') => {
                status.tracked_changes += 1;
                if record.starts_with("2 ") {
                    let _ = fields.next();
                }
            }
            Some(b'?') => status.untracked_files += 1,
            Some(other) => {
                return Err(format!(
                    "unknown porcelain v2 record kind {:?}",
                    other as char
                ));
            }
            None => {}
        }
    }
    status.dirty = status.tracked_changes != 0 || status.untracked_files != 0;
    Ok(status)
}

fn signed_count(value: Option<&str>, sign: char) -> Result<u32, String> {
    let Some(value) = value else {
        return Ok(0);
    };
    value
        .strip_prefix(sign)
        .ok_or_else(|| format!("invalid branch.ab field {value:?}"))?
        .parse()
        .map_err(|_| format!("invalid branch.ab count {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::parse_status;

    #[test]
    fn parses_branch_counts_and_rename_paths() {
        let status = parse_status(
            b"# branch.oid abc123\0# branch.head feature/a\0# branch.upstream origin/feature/a\0# branch.ab +2 -1\x001 M. N... 100644 100644 100644 a b file name\x002 R. N... 100644 100644 100644 a b R100 new name\0old name\0? untracked file\0",
        ).unwrap();
        assert_eq!(status.branch.as_deref(), Some("feature/a"));
        assert_eq!(status.head_sha.as_deref(), Some("abc123"));
        assert_eq!((status.ahead, status.behind), (Some(2), Some(1)));
        assert_eq!((status.tracked_changes, status.untracked_files), (2, 1));
        assert!(status.dirty);
    }
}
