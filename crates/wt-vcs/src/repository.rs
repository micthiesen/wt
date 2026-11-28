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
use wt_platform::lock::{LockError, MachineSlot};
use wt_platform::process::{CommandSpec, ProcessClass, ProcessError, ProcessRunner};

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
    /// The fleet-scan turn this process holds, shared by every handle on the
    /// repository.
    scan_turn: Arc<Mutex<std::sync::Weak<MachineSlot>>>,
}

/// This process's turn to read every checkout's status; given back when the
/// last read sharing it ends. A read that got no turn (none are rationed, it
/// is one a person waits on, or the wait ran out) proceeds without.
pub struct FleetScan {
    slot: Option<Arc<MachineSlot>>,
}

impl FleetScan {
    pub fn held(&self) -> bool {
        self.slot.is_some()
    }
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
            scan_turn: Arc::default(),
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

    /// How many checkouts may have a status read in flight at once.
    pub fn max_concurrent_status(&self) -> NonZeroUsize {
        self.max_concurrent_status
    }

    /// The same repository with its Git commands admitted as `class`.
    pub fn with_process_class(&self, class: ProcessClass) -> Self {
        self.with_process_runner(self.runner.with_class(class))
    }

    /// Wait for this process's turn to read every checkout's status, and
    /// hold what is returned until that read ends. See
    /// `ProcessRunner::fleet_scan`.
    ///
    /// The turn rations processes against each other, so one process takes
    /// one: a second fleet read in this process (a command the host runs
    /// while its own sweep is under way) shares the turn already held rather
    /// than queueing behind it for a slot the process itself occupies.
    pub async fn fleet_scan(&self, cancellation: &CancellationToken) -> FleetScan {
        if matches!(
            self.runner.class(),
            ProcessClass::Interactive | ProcessClass::Confirming
        ) {
            return FleetScan { slot: None };
        }
        let mut turn = self.scan_turn.lock().await;
        if let Some(slot) = turn.upgrade() {
            return FleetScan { slot: Some(slot) };
        }
        let slot = self.runner.fleet_scan(cancellation).await.map(Arc::new);
        *turn = slot.as_ref().map(Arc::downgrade).unwrap_or_default();
        FleetScan { slot }
    }

    /// Rebind process admission without losing repository-wide fetch coalescing.
    pub fn with_process_runner(&self, runner: ProcessRunner) -> Self {
        Self {
            runner,
            ..self.clone()
        }
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
        let root = self.config.worktree_root.clone();
        let main_clone = self.config.main_clone.clone();
        // The listing runs with every read of the fleet, however small. One
        // hop to the blocking pool covers every checkout's files; a hop per
        // file would queue behind the pool's other work several hundred
        // times.
        let located = tokio::task::spawn_blocking(move || {
            let root = normalize_path_blocking(&root);
            let main_clone = normalize_path_blocking(&main_clone);
            records
                .into_iter()
                .filter_map(|mut record| {
                    let canonical = normalize_path_blocking(Path::new(&record.target.path));
                    record.is_main = canonical == main_clone;
                    if !record.is_main
                        && (!canonical.starts_with(&root)
                            || record.target.slug().starts_with("wt-verify-"))
                    {
                        return None;
                    }
                    let (git_dir, common_dir) = git_metadata_paths_blocking(&canonical);
                    if record.target.branch.is_empty() {
                        record.target.branch =
                            rebase_branch(git_dir.as_deref()).unwrap_or_default();
                    }
                    let stage = read_stage_blocking(&canonical);
                    record.git_dir = git_dir;
                    record.common_dir = common_dir;
                    Some((record, canonical, stage))
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|source| RepositoryError::Io {
            operation: "inspect Git worktrees",
            path: self.config.main_clone.clone(),
            source: std::io::Error::other(source),
        })?;
        let mut records = Vec::with_capacity(located.len());
        for (mut record, canonical, stage) in located {
            let stage = stage.unwrap_or_else(|| self.derive_stage(record.target.slug()));
            let slug = if record.is_main {
                "main".to_owned()
            } else {
                record.target.slug().to_owned()
            };
            record.target = local_worktree_target(
                slug,
                std::mem::take(&mut record.target.branch),
                canonical.to_string_lossy(),
                stage,
            );
            records.push(record);
        }
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
        Ok(self.statuses(records, cancellation).await)
    }

    /// The status half of [`Self::inventory_status`], for a caller that
    /// listed the worktrees itself and has other reads to start meanwhile.
    pub async fn statuses(
        &self,
        records: Vec<WorktreeRecord>,
        cancellation: &CancellationToken,
    ) -> Vec<WorktreeSnapshot> {
        // Every agent's `wt` command reads the fleet through here with a
        // subprocess budget of its own; the turn bounds their sum.
        let _turn = self.fleet_scan(cancellation).await;
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
        rows.into_iter().map(|(_, snapshot)| snapshot).collect()
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
        let root = self.config.worktree_root.clone();
        let listed: Vec<PathBuf> = records
            .iter()
            .map(|record| PathBuf::from(&record.target.path))
            .collect();
        // One hop to the blocking pool finds the few directories that could
        // be clones; see `inventory`.
        type Scanned = (HashSet<PathBuf>, Vec<PathBuf>);
        let scanned = tokio::task::spawn_blocking(move || -> Result<Scanned, RepositoryError> {
            let io = |operation, path: &Path| {
                let path = path.to_path_buf();
                move |source| RepositoryError::Io {
                    operation,
                    path,
                    source,
                }
            };
            let entries = std::fs::read_dir(&root).map_err(io("scan worktree root", &root))?;
            let known: HashSet<PathBuf> = listed
                .iter()
                .map(|path| normalize_path_blocking(path))
                .collect();
            let mut candidates = Vec::new();
            for entry in entries {
                let entry = entry.map_err(io("read worktree root", &root))?;
                if Self::is_removal_quarantine(&entry.file_name()) {
                    continue;
                }
                let path = normalize_path_blocking(&entry.path());
                if known.contains(&path) {
                    continue;
                }
                // Removal deletes clones while this scan runs. An entry that
                // vanished is not part of the inventory; it must not fail it.
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => {}
                    Ok(_) => continue,
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(source) => {
                        return Err(io("inspect worktree root entry", &path)(source));
                    }
                }
                let marker = path.join(".rift");
                if marker
                    .try_exists()
                    .map_err(io("inspect Rift marker", &marker))?
                {
                    candidates.push(path);
                }
            }
            Ok((known, candidates))
        })
        .await
        .map_err(|source| RepositoryError::Io {
            operation: "scan worktree root",
            path: self.config.worktree_root.clone(),
            source: std::io::Error::other(source),
        })?;
        let (mut known, candidates) = scanned?;
        for path in candidates {
            if known.contains(&path) {
                continue;
            }
            let top = match self
                .run_git(
                    &path,
                    ["rev-parse", "--show-toplevel"],
                    cancellation,
                    "identify Rift clone",
                )
                .await
            {
                Ok(top) => {
                    normalize_path(&PathBuf::from(String::from_utf8_lossy(&top).trim())).await
                }
                Err(error) if cancellation.is_cancelled() => return Err(error),
                Err(RepositoryError::Cancelled) => return Err(RepositoryError::Cancelled),
                Err(_) => {
                    // One broken or half-deleted clone is that row's problem.
                    // Keep it listed so its status read reports the error,
                    // unless it has disappeared entirely.
                    if matches!(fs::try_exists(&path).await, Ok(false)) {
                        continue;
                    }
                    known.insert(path.clone());
                    records.push(
                        self.make_record(path, None, None, false, false, RepositoryKind::RiftClone)
                            .await?,
                    );
                    continue;
                }
            };
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
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_stage_blocking(&path))
        .await
        .ok()
        .flatten()
}

fn read_stage_blocking(path: &Path) -> Option<String> {
    std::fs::read_to_string(path.join(".sst/stage"))
        .ok()
        .map(|stage| stage.trim().to_owned())
        .filter(|stage| !stage.is_empty())
}

/// Count newlines in one untracked regular file. Git also lists symlinks and
/// nested repositories (`sub/`), and a path can vanish after `ls-files`; none
/// of those have countable lines. Opening a FIFO would block indefinitely.
async fn count_lines(path: &Path) -> Result<u64, std::io::Error> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    }
    let mut file = match fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
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
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || git_metadata_paths_blocking(&path))
        .await
        .unwrap_or_default()
}

fn git_metadata_paths_blocking(path: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let dot_git = path.join(".git");
    let git_dir = match std::fs::read_to_string(&dot_git) {
        Ok(pointer) => pointer.trim().strip_prefix("gitdir:").map(|raw| {
            let raw = PathBuf::from(raw.trim());
            if raw.is_absolute() {
                raw
            } else {
                path.join(raw)
            }
        }),
        Err(_) => dot_git
            .try_exists()
            .ok()
            .filter(|exists| *exists)
            .map(|_| dot_git),
    };
    let common_dir =
        git_dir.as_ref().map(
            |git_dir| match std::fs::read_to_string(git_dir.join("commondir")) {
                Ok(value) => {
                    let raw = PathBuf::from(value.trim());
                    if raw.is_absolute() {
                        raw
                    } else {
                        git_dir.join(raw)
                    }
                }
                Err(_) => git_dir.clone(),
            },
        );
    let canonical = |path: PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
    (git_dir.map(canonical), common_dir.map(canonical))
}

/// The main-clone refs that keep a stack parent standing. While either
/// exists and the parent is not proven merged, its children still build on
/// commits only it carries, so they stay stacked on it. Removal and restack
/// both decide "the parent is gone" from this list.
pub fn stack_parent_refs(branch: &str) -> [String; 2] {
    [
        format!("refs/heads/{branch}"),
        format!("refs/remotes/origin/{branch}"),
    ]
}

/// A rebase, merge, cherry-pick or revert is stopped in this checkout. Its
/// HEAD is then transient (a rebase starts by checking out its target), so
/// no landing or cleanup decision may be read from it. Without a known git
/// directory this answers false and callers keep their other evidence.
pub async fn operation_in_progress(git_dir: Option<&Path>) -> bool {
    let Some(git_dir) = git_dir else {
        return false;
    };
    for marker in [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "sequencer",
    ] {
        if fs::try_exists(git_dir.join(marker)).await.unwrap_or(true) {
            return true;
        }
    }
    false
}

fn rebase_branch(git_dir: Option<&Path>) -> Option<String> {
    let git_dir = git_dir?;
    for metadata in ["rebase-merge/head-name", "rebase-apply/head-name"] {
        if let Ok(name) = std::fs::read_to_string(git_dir.join(metadata)) {
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

fn normalize_path_blocking(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{parse_shortstat, parse_worktree_list};
    use std::path::Path;

    #[tokio::test]
    async fn one_process_takes_one_fleet_scan_turn_however_many_reads_share_it() {
        use std::time::Duration;
        use wt_platform::lock::MachineSlots;
        let locks = tempfile::tempdir().unwrap();
        let slots = MachineSlots::open(locks.path(), "fleet-scan", 1.try_into().unwrap())
            .await
            .unwrap();
        let runner = super::ProcessRunner::default().with_fleet_scan_slots(slots.clone());
        let repository = super::GitRepository::new(
            super::RepositoryConfig {
                main_clone: "/repo/main".into(),
                worktree_root: "/repo/worktrees".into(),
                trunk_branch: "main".into(),
                stage: super::StageConfig {
                    prefix: "stage-".into(),
                    issue_id_pattern: String::new(),
                },
            },
            runner,
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        // The host's sweep holds the machine's only turn.
        let sweep = repository.fleet_scan(&cancel).await;
        assert!(sweep.held());
        // A command the host runs meanwhile reads the fleet on another
        // handle; it must not wait for the turn its own process holds.
        let command = repository
            .clone()
            .with_max_concurrent_status(8.try_into().unwrap());
        let shared = tokio::time::timeout(Duration::from_secs(1), command.fleet_scan(&cancel))
            .await
            .expect("queued behind this process's own sweep");
        assert!(shared.held());
        // Other processes still see one turn taken, until both reads end.
        let other = |wait| slots.acquire_within(Duration::from_millis(wait), &cancel);
        drop(sweep);
        assert!(other(50).await.unwrap().is_none());
        drop(shared);
        assert!(other(1000).await.unwrap().is_some());
        // A read someone is waiting on never takes or waits for a turn.
        let held = repository.fleet_scan(&cancel).await;
        let confirming = repository.with_process_class(super::ProcessClass::Confirming);
        assert!(!confirming.fleet_scan(&cancel).await.held());
        drop(held);
    }

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
