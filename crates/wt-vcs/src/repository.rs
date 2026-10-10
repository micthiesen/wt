use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{StreamExt, stream};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use wt_core::{WorktreeTarget, local_worktree_target};
use wt_platform::lock::LockError;
use wt_platform::process::{CommandSpec, ProcessError, ProcessRunner};

use crate::status::{GitStatus, common_git_env, read_status};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct StageConfig {
    pub prefix: String,
    pub issue_id_pattern: String,
}

#[derive(Clone, Debug)]
pub struct RepositoryConfig {
    pub main_clone: PathBuf,
    pub worktree_root: PathBuf,
    /// Kept verbatim. Git branch names such as `origin/main` are meaningful.
    pub trunk_branch: String,
    pub stage: StageConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RepositoryKind {
    LinkedWorktree,
    RiftClone,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRecord {
    pub target: WorktreeTarget,
    pub is_main: bool,
    pub head_sha: Option<String>,
    pub detached: bool,
    pub kind: RepositoryKind,
    pub locked: bool,
    pub prunable: bool,
    /// Git's administration directory (the `.git` directory for the main clone).
    pub git_dir: Option<PathBuf>,
    /// The common Git directory that owns refs and linked-worktree metadata.
    pub common_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSnapshot {
    pub worktree: WorktreeRecord,
    pub status: Option<GitStatus>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffStats {
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
    pub untracked_files: u32,
    pub untracked_lines: u64,
}

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("{operation} at {path}: {source}")]
    Process {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: ProcessError,
    },
    #[error("{operation} at {path}: {message}")]
    Parse {
        operation: &'static str,
        path: PathBuf,
        message: String,
    },
    #[error("{operation} at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Git operation cancelled")]
    Cancelled,
    #[error("fetch origin at {path}: {message}")]
    FetchOrigin { path: PathBuf, message: String },
    #[error("fetch origin lock at {path}: {source}")]
    FetchLock {
        path: PathBuf,
        #[source]
        source: LockError,
    },
}

#[derive(Clone)]
pub struct GitRepository {
    config: RepositoryConfig,
    runner: ProcessRunner,
    max_concurrent_status: NonZeroUsize,
    stage_issue_pattern: Option<Regex>,
    pub(crate) fetch_flight: Arc<Mutex<Option<Arc<crate::origin::FetchFlight>>>>,
}

#[derive(Debug, Default)]
struct PorcelainWorktree {
    path: Option<PathBuf>,
    head_sha: Option<String>,
    branch: Option<String>,
    detached: bool,
    locked: bool,
    prunable: bool,
}

impl GitRepository {
    pub fn new(config: RepositoryConfig, runner: ProcessRunner) -> Self {
        let stage_issue_pattern = RegexBuilder::new(&config.stage.issue_id_pattern)
            .case_insensitive(true)
            .build()
            .ok();
        Self {
            config,
            runner,
            max_concurrent_status: NonZeroUsize::new(4).expect("four is nonzero"),
            stage_issue_pattern,
            fetch_flight: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn origin_config(&self) -> &RepositoryConfig {
        &self.config
    }

    pub(crate) fn origin_runner(&self) -> &ProcessRunner {
        &self.runner
    }

    pub fn with_max_concurrent_status(mut self, limit: NonZeroUsize) -> Self {
        self.max_concurrent_status = limit;
        self
    }

    /// Enumerate linked worktrees and independently cloned `.rift` checkouts.
    /// Enumeration errors fail the call; individual checkout status is a separate operation.
    pub async fn inventory(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<WorktreeRecord>, RepositoryError> {
        let bytes = self
            .run_git(
                &self.config.main_clone,
                ["worktree", "list", "--porcelain", "-z"],
                cancellation,
                "list Git worktrees",
            )
            .await?;
        let mut records = parse_worktree_list(&bytes, &self.config.main_clone)?;
        self.append_rift_clones(&mut records, cancellation).await?;
        let root = normalize_path(&self.config.worktree_root).await;
        let main_clone = normalize_path(&self.config.main_clone).await;
        let mut selected = Vec::with_capacity(records.len());
        for mut record in records.drain(..) {
            let path = PathBuf::from(&record.target.path);
            let canonical = normalize_path(&path).await;
            record.is_main = canonical == main_clone;
            if !record.is_main
                && (!canonical.starts_with(&root) || record.target.slug().starts_with("wt-verify-"))
            {
                continue;
            }
            let (git_dir, common_dir) = git_metadata_paths(&canonical).await;
            if record.target.branch.is_empty() {
                record.target.branch = rebase_branch(git_dir.as_deref()).await.unwrap_or_default();
            }
            record.target.stage = read_stage(&canonical)
                .await
                .unwrap_or_else(|| self.derive_stage(record.target.slug()));
            let slug = if record.is_main {
                "main".to_owned()
            } else {
                record.target.slug().to_owned()
            };
            record.target = local_worktree_target(
                slug,
                std::mem::take(&mut record.target.branch),
                canonical.to_string_lossy(),
                std::mem::take(&mut record.target.stage),
            );
            record.git_dir = git_dir;
            record.common_dir = common_dir;
            selected.push(record);
        }
        records = selected;
        records.sort_by(|a, b| (!a.is_main, a.target.slug()).cmp(&(!b.is_main, b.target.slug())));
        Ok(records)
    }

    /// Read one status per worktree with bounded concurrency. A broken or removed
    /// checkout becomes a row error; it does not erase the rest of the inventory.
    pub async fn inventory_status(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<WorktreeSnapshot>, RepositoryError> {
        let records = self.inventory(cancellation).await?;
        let runner = self.runner.clone();
        let limit = self.max_concurrent_status.get();
        let mut rows = stream::iter(records.into_iter().enumerate())
            .map(|(index, worktree)| {
                let runner = runner.clone();
                let cancellation = cancellation.child_token();
                async move {
                    let path = PathBuf::from(&worktree.target.path);
                    let (status, error) = match read_status(&runner, &path, &cancellation).await {
                        Ok(status) => (Some(status), None),
                        Err(error) => (None, Some(error.to_string())),
                    };
                    (
                        index,
                        WorktreeSnapshot {
                            worktree,
                            status,
                            error,
                        },
                    )
                }
            })
            .buffer_unordered(limit)
            .collect::<Vec<_>>()
            .await;
        rows.sort_by_key(|(index, _)| *index);
        Ok(rows.into_iter().map(|(_, snapshot)| snapshot).collect())
    }

    pub async fn status(
        &self,
        worktree: &WorktreeRecord,
        cancellation: &CancellationToken,
    ) -> Result<GitStatus, RepositoryError> {
        read_status(&self.runner, Path::new(&worktree.target.path), cancellation).await
    }

    /// Return conventional Git diff totals for tracked changes since `base`.
    /// This intentionally excludes untracked paths, which are reported separately.
    pub async fn diff_stats(
        &self,
        path: &Path,
        base: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<DiffStats>, RepositoryError> {
        let merge_base = self
            .run_git(
                path,
                ["merge-base", base, "HEAD"],
                cancellation,
                "find Git diff base",
            )
            .await;
        let merge_base = match merge_base {
            Ok(output) => output,
            Err(
                error @ RepositoryError::Process {
                    source: ProcessError::Exit { .. },
                    ..
                },
            ) => {
                let _ = error;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let merge_base = String::from_utf8_lossy(&merge_base).trim().to_owned();
        if merge_base.is_empty() {
            return Ok(None);
        }
        let diff = self
            .run_git(
                path,
                ["diff", "--shortstat", merge_base.as_str()],
                cancellation,
                "read Git diff statistics",
            )
            .await?;
        let mut stats = parse_shortstat(&diff)?;
        let untracked = self
            .run_git(
                path,
                ["ls-files", "--others", "--exclude-standard", "-z"],
                cancellation,
                "list untracked files",
            )
            .await?;
        let raw_paths: Vec<&[u8]> = untracked
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .collect();
        stats.untracked_files = raw_paths.len() as u32;
        let paths = raw_paths
            .into_iter()
            .map(|relative| os_path_from_git(relative, path).map(|relative| path.join(relative)))
            .collect::<Result<Vec<_>, _>>()?;
        let root = path.to_path_buf();
        let mut counts = stream::iter(paths.into_iter().map(|file| {
            let root = root.clone();
            let cancellation = cancellation.clone();
            async move {
                tokio::select! {
                    _ = cancellation.cancelled() => Err(RepositoryError::Cancelled),
                    count = count_lines(&file) => count.map_err(|source| RepositoryError::Io {
                        operation: "count untracked file lines",
                        path: file.strip_prefix(&root).unwrap_or(&file).to_path_buf(),
                        source,
                    }),
                }
            }
        }))
        .buffer_unordered(4);
        while let Some(count) = counts.next().await {
            stats.untracked_lines += count?;
        }
        Ok(Some(stats))
    }

    async fn append_rift_clones(
        &self,
        records: &mut Vec<WorktreeRecord>,
        cancellation: &CancellationToken,
    ) -> Result<(), RepositoryError> {
        let mut entries = fs::read_dir(&self.config.worktree_root)
            .await
            .map_err(|source| RepositoryError::Io {
                operation: "scan worktree root",
                path: self.config.worktree_root.clone(),
                source,
            })?;
        let mut known = HashSet::new();
        for record in records.iter() {
            known.insert(normalize_path(Path::new(&record.target.path)).await);
        }
        while let Some(entry) =
            entries
                .next_entry()
                .await
                .map_err(|source| RepositoryError::Io {
                    operation: "read worktree root",
                    path: self.config.worktree_root.clone(),
                    source,
                })?
        {
            let path = normalize_path(&entry.path()).await;
            if known.contains(&path)
                || !entry
                    .file_type()
                    .await
                    .map_err(|source| RepositoryError::Io {
                        operation: "inspect worktree root entry",
                        path: path.clone(),
                        source,
                    })?
                    .is_dir()
            {
                continue;
            }
            let marker = path.join(".rift");
            if !fs::try_exists(&marker)
                .await
                .map_err(|source| RepositoryError::Io {
                    operation: "inspect Rift marker",
                    path: marker.clone(),
                    source,
                })?
            {
                continue;
            }
            let top = self
                .run_git(
                    &path,
                    ["rev-parse", "--show-toplevel"],
                    cancellation,
                    "identify Rift clone",
                )
                .await?;
            let top = PathBuf::from(String::from_utf8_lossy(&top).trim());
            let top = normalize_path(&top).await;
            if known.contains(&top) {
                continue;
            }
            let head = self
                .run_git(
                    &top,
                    ["rev-parse", "--verify", "HEAD"],
                    cancellation,
                    "read Rift clone HEAD",
                )
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).trim().to_owned());
            let branch = self
                .run_git(
                    &top,
                    ["symbolic-ref", "--quiet", "--short", "HEAD"],
                    cancellation,
                    "read Rift clone branch",
                )
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).trim().to_owned());
            known.insert(top.clone());
            records.push(
                self.make_record(top, head, branch, false, false, RepositoryKind::RiftClone)
                    .await?,
            );
        }
        Ok(())
    }

    async fn make_record(
        &self,
        path: PathBuf,
        head_sha: Option<String>,
        branch: Option<String>,
        detached: bool,
        locked: bool,
        kind: RepositoryKind,
    ) -> Result<WorktreeRecord, RepositoryError> {
        let path = normalize_path(&path).await;
        let main = normalize_path(&self.config.main_clone).await;
        let is_main = path == main;
        let slug = if is_main {
            "main".to_owned()
        } else {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let branch = branch.unwrap_or_default();
        let stage = read_stage(&path)
            .await
            .unwrap_or_else(|| self.derive_stage(&slug));
        let target = local_worktree_target(slug, branch, path.to_string_lossy(), stage);
        let (git_dir, common_dir) = git_metadata_paths(&path).await;
        Ok(WorktreeRecord {
            target,
            is_main,
            head_sha,
            detached,
            kind,
            locked,
            prunable: false,
            git_dir,
            common_dir,
        })
    }

    fn derive_stage(&self, slug: &str) -> String {
        wt_core::stage_name(
            slug,
            &self.config.stage.prefix,
            self.stage_issue_pattern.as_ref(),
        )
    }

    async fn run_git<const N: usize>(
        &self,
        cwd: &Path,
        args: [&str; N],
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<Vec<u8>, RepositoryError> {
        let mut spec = CommandSpec::new("git").args(args);
        spec.cwd = Some(cwd.to_path_buf());
        spec.env = common_git_env();
        spec.timeout = GIT_TIMEOUT;
        spec.output_limit = GIT_OUTPUT_LIMIT;
        let output = self
            .runner
            .run(spec, cancellation)
            .await
            .map_err(|source| RepositoryError::Process {
                operation,
                path: cwd.to_path_buf(),
                source,
            })?;
        Ok(output
            .checked("git")
            .map_err(|source| RepositoryError::Process {
                operation,
                path: cwd.to_path_buf(),
                source,
            })?
            .stdout)
    }
}

fn parse_worktree_list(
    bytes: &[u8],
    main_clone: &Path,
) -> Result<Vec<WorktreeRecord>, RepositoryError> {
    let fields = bytes.split(|byte| *byte == 0);
    let mut parsed = Vec::new();
    let mut current = PorcelainWorktree::default();
    for field in fields {
        if field.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(field).map_err(|_| RepositoryError::Parse {
            operation: "parse Git worktree list",
            path: main_clone.to_path_buf(),
            message: "porcelain field is not UTF-8".into(),
        })?;
        if let Some(value) = text.strip_prefix("worktree ") {
            if current.path.is_some() {
                parsed.push(current);
                current = PorcelainWorktree::default();
            }
            current.path = Some(PathBuf::from(value));
        } else if let Some(value) = text.strip_prefix("HEAD ") {
            current.head_sha = Some(value.to_owned());
        } else if let Some(value) = text.strip_prefix("branch ") {
            current.branch = Some(
                value
                    .strip_prefix("refs/heads/")
                    .unwrap_or(value)
                    .to_owned(),
            );
        } else if text == "detached" {
            current.detached = true;
        } else if text.starts_with("locked") {
            current.locked = true;
        } else if text.starts_with("prunable") {
            current.prunable = true;
        }
    }
    if current.path.is_some() {
        parsed.push(current);
    }

    // `-z` makes each field NUL-terminated; each new `worktree` starts a record.
    let mut records = Vec::new();
    for item in parsed {
        let path = item.path.expect("filtered above");
        let main = path == main_clone;
        let slug = if main {
            "main".to_owned()
        } else {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let target = local_worktree_target(
            slug,
            item.branch.clone().unwrap_or_default(),
            path.to_string_lossy(),
            String::new(),
        );
        records.push(WorktreeRecord {
            target,
            is_main: main,
            head_sha: item.head_sha,
            detached: item.detached,
            kind: RepositoryKind::LinkedWorktree,
            locked: item.locked,
            prunable: item.prunable,
            git_dir: None,
            common_dir: None,
        });
    }
    Ok(records)
}

fn parse_shortstat(bytes: &[u8]) -> Result<DiffStats, RepositoryError> {
    let text = String::from_utf8_lossy(bytes);
    let mut stats = DiffStats::default();
    for part in text.split(',') {
        let mut words = part.split_ascii_whitespace();
        let Some(number) = words.next() else {
            continue;
        };
        let Ok(number) = number.parse::<u32>() else {
            continue;
        };
        match words.next() {
            Some("files") | Some("file") => stats.files_changed = number,
            Some("insertions(+)") | Some("insertion(+)") => stats.insertions = number,
            Some("deletions(-)") | Some("deletion(-)") => stats.deletions = number,
            _ => {}
        }
    }
    Ok(stats)
}

fn os_path_from_git(bytes: &[u8], repository: &Path) -> Result<PathBuf, RepositoryError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let _ = repository;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(not(unix))]
    {
        let path = std::str::from_utf8(bytes).map_err(|_| RepositoryError::Parse {
            operation: "parse untracked Git path",
            path: repository.to_path_buf(),
            message: "non-UTF-8 paths are unsupported on this platform".into(),
        })?;
        Ok(PathBuf::from(path))
    }
}

async fn read_stage(path: &Path) -> Option<String> {
    fs::read_to_string(path.join(".sst/stage"))
        .await
        .ok()
        .map(|stage| stage.trim().to_owned())
        .filter(|stage| !stage.is_empty())
}

async fn count_lines(path: &Path) -> Result<u64, std::io::Error> {
    let mut file = fs::File::open(path).await?;
    let mut buffer = [0u8; 16 * 1024];
    let mut count = 0u64;
    loop {
        let bytes = file.read(&mut buffer).await?;
        if bytes == 0 {
            return Ok(count);
        }
        count += buffer[..bytes]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count() as u64;
    }
}

pub(crate) async fn git_metadata_paths(path: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let dot_git = path.join(".git");
    let git_dir = match fs::read_to_string(&dot_git).await {
        Ok(pointer) => pointer.trim().strip_prefix("gitdir:").map(|raw| {
            let raw = PathBuf::from(raw.trim());
            if raw.is_absolute() {
                raw
            } else {
                path.join(raw)
            }
        }),
        Err(_) => fs::try_exists(&dot_git)
            .await
            .ok()
            .filter(|exists| *exists)
            .map(|_| dot_git),
    };
    let common_dir = if let Some(git_dir) = &git_dir {
        match fs::read_to_string(git_dir.join("commondir")).await {
            Ok(value) => {
                let raw = PathBuf::from(value.trim());
                Some(if raw.is_absolute() {
                    raw
                } else {
                    git_dir.join(raw)
                })
            }
            Err(_) => Some(git_dir.clone()),
        }
    } else {
        None
    };
    let git_dir = match git_dir {
        Some(path) => Some(fs::canonicalize(&path).await.unwrap_or(path)),
        None => None,
    };
    let common_dir = match common_dir {
        Some(path) => Some(fs::canonicalize(&path).await.unwrap_or(path)),
        None => None,
    };
    (git_dir, common_dir)
}

async fn rebase_branch(git_dir: Option<&Path>) -> Option<String> {
    let git_dir = git_dir?;
    for metadata in ["rebase-merge/head-name", "rebase-apply/head-name"] {
        if let Ok(name) = fs::read_to_string(git_dir.join(metadata)).await {
            let name = name.trim();
            if let Some(branch) = name.strip_prefix("refs/heads/") {
                return Some(branch.to_owned());
            }
        }
    }
    None
}

async fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{parse_shortstat, parse_worktree_list};
    use std::path::Path;

    #[test]
    fn porcelain_paths_keep_spaces_and_branch_names() {
        let records = parse_worktree_list(
            b"worktree /tmp/main clone\0HEAD abc\0branch refs/heads/main\0\0worktree /tmp/wt root/one\0HEAD def\0branch refs/heads/team/one\0locked by user\0\0",
            Path::new("/tmp/main clone"),
        ).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records[0].is_main);
        assert_eq!(records[1].target.path, "/tmp/wt root/one");
        assert_eq!(records[1].target.branch, "team/one");
        assert!(records[1].locked);
    }

    #[test]
    fn parses_git_shortstat_variants() {
        let stats = parse_shortstat(b" 2 files changed, 4 insertions(+), 1 deletion(-)\n").unwrap();
        assert_eq!(
            (stats.files_changed, stats.insertions, stats.deletions),
            (2, 4, 1)
        );
    }
}
