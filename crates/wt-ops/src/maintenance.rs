//! One conservative pass over what wt leaves behind and nothing else
//! removes: installed versions, finished action runs, dev-server state,
//! inspector sockets and session logs of worktrees that are gone, and, when
//! asked, the slug records of worktrees removed long ago.
//!
//! Every rule needs proof that nothing still owns the file. Where the proof
//! cannot be had (the worktree list, tmux or the process list could not be
//! read) that class is left alone and named in the pass's log line.
//!
//! Lock files are never removed. A lock is the file's inode, lockers keep
//! the file after release and do not check it again once open, so unlinking
//! one can put two holders in the same critical section.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt as _;
use serde_json::Value;
use wt_app_core::context::AppContext;
use wt_platform::process::CommandSpec;
use wt_tmux::{TmuxClient, TmuxServer};
use wt_update::InstallPaths;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
/// A file younger than this may still be on its way to an owner.
const SETTLED: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Finished action runs are history the action pane and the automation
/// engine read back: the newest of them stay whatever their age.
const KEEP_RUNS: usize = 500;
const RUN_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Session names that belong to no worktree.
const RESERVED: [&str; 4] = ["main", "manager", "wt", "dotfiles"];
/// What a slot's session name adds to its slug.
const SESSION_SUFFIXES: [&str; 7] = [
    "-codex",
    "-opencode",
    "-pi",
    "-action",
    "-shell",
    "-diff",
    "-dev",
];
const STAMP: &str = "maintenance.json";
const LOCK: &str = "__maintenance__";

/// What a pass covers beyond leftover files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    /// Drop the slug records of long-removed worktrees from the repository
    /// state. Durable state goes only when a person asked for the pass or
    /// the configuration says the daily one may.
    pub state_records: bool,
}

impl Scope {
    /// A pass a person asked for.
    pub const ASKED: Self = Self {
        state_records: true,
    };

    /// The pass a running wt makes on its own.
    pub fn daily(ctx: &AppContext) -> Self {
        Self {
            state_records: ctx.config.maintenance.state_records,
        }
    }
}

/// What a pass may remove, by class. A class in `unknown` was not judged.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Leftovers {
    pub versions: Vec<PathBuf>,
    pub runs: Vec<PathBuf>,
    pub dev_files: Vec<PathBuf>,
    pub sockets: Vec<PathBuf>,
    pub session_logs: Vec<PathBuf>,
    pub slug_records: Vec<String>,
    pub unknown: Vec<&'static str>,
    /// Checks whose first-due time a pass read back from history. Not a
    /// leftover, and never counted as one.
    pub redated_checks: Vec<wt_store::RedatedCheck>,
}

impl Leftovers {
    pub fn is_empty(&self) -> bool {
        self.versions.is_empty()
            && self.runs.is_empty()
            && self.dev_files.is_empty()
            && self.sockets.is_empty()
            && self.session_logs.is_empty()
            && self.slug_records.is_empty()
    }

    /// The classes with anything in them, as `3 action runs`, in a fixed
    /// order.
    pub fn counts(&self) -> Vec<String> {
        [
            (
                self.versions.len(),
                "installed version",
                "installed versions",
            ),
            (self.runs.len(), "action run", "action runs"),
            (self.dev_files.len(), "dev-server file", "dev-server files"),
            (self.sockets.len(), "inspector socket", "inspector sockets"),
            (self.session_logs.len(), "session log", "session logs"),
            (self.slug_records.len(), "slug record", "slug records"),
        ]
        .into_iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, one, many)| format!("{count} {}", if count == 1 { one } else { many }))
        .collect()
    }
}

/// What the machine says is alive. `None` is "could not be read", which
/// proves nothing gone.
#[derive(Clone, Debug, Default)]
struct Alive {
    slugs: Option<BTreeSet<String>>,
    /// The repository's tmux sessions.
    sessions: Option<BTreeSet<String>>,
    /// Version keys a running process runs from or carries in its
    /// environment, or a tmux session hands to what it starts: a session
    /// stays pinned to the build that started it through its `PATH`.
    versions: Option<BTreeSet<String>>,
}

impl Alive {
    /// Whether `name` (a slug, or a session named after one) may belong to a
    /// live worktree or session. A slug that is a prefix of another makes
    /// this say yes too often, never too rarely.
    fn owns(&self, name: &str) -> Option<bool> {
        let slugs = self.slugs.as_ref()?;
        let sessions = self.sessions.as_ref()?;
        let reserved = |name: &str| {
            RESERVED.iter().any(|reserved| {
                name.strip_prefix(reserved).is_some_and(|rest| {
                    rest.is_empty() || rest.starts_with('~') || SESSION_SUFFIXES.contains(&rest)
                })
            })
        };
        Some(
            sessions.contains(name)
                || slugs.iter().any(|slug| name.starts_with(slug.as_str()))
                || reserved(name),
        )
    }
}

/// The directory names `git worktree list --porcelain -z` printed.
fn listed_worktrees(porcelain: &str) -> BTreeSet<String> {
    porcelain
        .split('\0')
        .filter_map(|field| field.strip_prefix("worktree "))
        .filter_map(|path| Some(Path::new(path).file_name()?.to_string_lossy().into_owned()))
        .collect()
}

/// Every name a worktree answers to right now: each checkout Git itself
/// lists, wherever it is and whether or not the board shows it, and each row
/// of the inventory, which also holds clones Git does not list. `None`
/// unless both could be read.
async fn live_slugs(ctx: &AppContext) -> Option<BTreeSet<String>> {
    let (listed, inventory) = tokio::join!(
        ctx.processes.run(
            CommandSpec::new("git")
                .args(["worktree", "list", "--porcelain", "-z"])
                .cwd(&ctx.config.paths.main_clone),
            &ctx.cancellation,
        ),
        ctx.repository.inventory(&ctx.cancellation),
    );
    let listed = listed
        .ok()
        .filter(|output| output.status.success() && !output.stdout_truncated)?;
    let mut slugs = listed_worktrees(&listed.stdout_text());
    slugs.extend(
        inventory
            .ok()?
            .iter()
            .map(|row| row.target.slug().to_owned()),
    );
    slugs.insert("main".to_owned());
    Some(slugs)
}

/// The version keys the environment of each of `sessions`, and of the tmux
/// server itself, names. `None` when any of them could not be asked.
async fn session_pins(
    ctx: &AppContext,
    sessions: &BTreeSet<String>,
    versions: &Path,
) -> Option<BTreeSet<String>> {
    if sessions.is_empty() {
        return Some(BTreeSet::new());
    }
    let mut targets: Vec<Vec<String>> = vec![vec!["-g".to_owned()]];
    for name in sessions {
        targets.push(vec!["-t".to_owned(), wt_tmux::exact_session_target(name)]);
    }
    let mut asks = Vec::new();
    for target in targets {
        asks.push(async move {
            let shown = ctx
                .processes
                .run(
                    CommandSpec::new("tmux")
                        .args(["-L", ctx.config.tmux.socket.as_str(), "show-environment"])
                        .args(target)
                        .cwd(&ctx.home),
                    &ctx.cancellation,
                )
                .await
                .ok()
                .filter(|output| output.status.success() && !output.stdout_truncated)?;
            Some(shown.stdout_text())
        });
    }
    let asked: Vec<Option<String>> = futures_util::stream::iter(asks)
        .buffer_unordered(8)
        .collect()
        .await;
    let mut pinned = BTreeSet::new();
    for shown in asked {
        pinned.extend(pinned_versions(&shown?, versions));
    }
    Some(pinned)
}

async fn alive(ctx: &AppContext, install: Option<&InstallPaths>) -> Alive {
    let tmux = TmuxClient::new(
        ctx.processes.clone(),
        TmuxServer::named(ctx.config.tmux.socket.clone()).with_cwd(ctx.home.clone()),
    );
    let processes = async {
        let versions = install?.root().join("versions");
        // `e` adds each process's environment, where a pinned PATH shows.
        let listed = ctx
            .processes
            .run(
                CommandSpec::new("ps").args(["axeww", "-o", "command="]),
                &ctx.cancellation,
            )
            .await
            .ok()
            .filter(|output| output.status.success() && !output.stdout_truncated)?;
        let mut pinned = pinned_versions(&listed.stdout_text(), &versions);
        // This process too, however it was started.
        if let Some(own) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.canonicalize().ok())
        {
            pinned.extend(pinned_versions(&own.to_string_lossy(), &versions));
        }
        Some(pinned)
    };
    let (slugs, sessions, running) = tokio::join!(
        live_slugs(ctx),
        tmux.list_sessions(&ctx.cancellation),
        processes,
    );
    let sessions: Option<BTreeSet<String>> = sessions
        .ok()
        .map(|sessions| sessions.into_iter().map(|session| session.name).collect());
    // A session with nothing running in it right now still pins the build
    // its next command resolves to.
    let versions = match (install, running, &sessions) {
        (Some(install), Some(mut running), Some(sessions)) => {
            session_pins(ctx, sessions, &install.root().join("versions"))
                .await
                .map(|pinned| {
                    running.extend(pinned);
                    running
                })
        }
        _ => None,
    };
    Alive {
        slugs,
        sessions,
        versions,
    }
}

/// The version keys `text` names under `versions`, spelled as given or
/// with its symlinks resolved.
fn pinned_versions(text: &str, versions: &Path) -> BTreeSet<String> {
    let mut roots = vec![versions.to_string_lossy().into_owned()];
    if let Ok(resolved) = versions.canonicalize() {
        roots.push(resolved.to_string_lossy().into_owned());
    }
    let mut keys = BTreeSet::new();
    for root in roots {
        let prefix = format!("{root}/");
        for (at, _) in text.match_indices(&prefix) {
            let rest = &text[at + prefix.len()..];
            let key = rest
                .split(|c: char| c == '/' || c == ':' || c.is_whitespace())
                .next()
                .unwrap_or_default();
            if !key.is_empty() {
                keys.insert(key.to_owned());
            }
        }
    }
    keys
}

fn older_than(path: &Path, age: Duration, now: SystemTime) -> bool {
    fs::symlink_metadata(path)
        .and_then(|meta| meta.modified())
        .is_ok_and(|modified| now.duration_since(modified).is_ok_and(|old| old > age))
}

fn files_in(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .collect();
    files.sort();
    files
}

/// When the action run `id` (`<epoch ms>-<pid>-<seq>`) started.
fn run_started(id: &str) -> Option<SystemTime> {
    let mut parts = id.split('-');
    let ms: u64 = parts.next()?.parse().ok()?;
    let rest: Vec<&str> = parts.collect();
    (rest.len() == 2 && rest.iter().all(|part| part.parse::<u64>().is_ok()))
        .then(|| UNIX_EPOCH + Duration::from_millis(ms))
}

/// Whether nothing holds the lock at `path`. The file is neither created
/// nor removed; one that does not exist is held by nobody.
fn lock_free(path: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return error.kind() == io::ErrorKind::NotFound,
    };
    // SAFETY: the descriptor is owned by `file` and open for both calls.
    unsafe {
        let free = libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0;
        if free {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
        free
    }
}

/// Whether no worker of the run in `path` can be alive. A worker holds the
/// lock of its run's worktree; one from a build that named the lock for the
/// run holds that instead. A run whose worktree cannot be read proves
/// nothing.
fn worker_gone(actions: &Path, lock_dir: &Path, id: &str, path: &Path) -> bool {
    let slug = fs::read(path.join("meta.json"))
        .ok()
        .and_then(|meta| serde_json::from_slice::<Value>(&meta).ok())
        .and_then(|meta| Some(meta.get("slug")?.as_str()?.to_owned()));
    let (Some(slug), Some(log_dir)) = (slug, actions.parent()) else {
        return false;
    };
    let by_worktree = wt_actions::worker_lock_key(log_dir, &slug);
    lock_free(&lock_dir.join(format!("{by_worktree}.lock")))
        && lock_free(&lock_dir.join(format!("action-run-{id}.lock")))
}

/// Finished runs past the newest `keep` that started over [`RUN_MAX_AGE`]
/// ago and whose worker no longer holds its lock: a held lock is a run that
/// is not over, whatever its files say.
fn old_runs(actions: &Path, lock_dir: &Path, keep: usize, now: SystemTime) -> Vec<PathBuf> {
    let mut runs: Vec<(SystemTime, String, PathBuf)> = files_in(actions)
        .into_iter()
        .filter_map(|(id, path)| Some((run_started(&id)?, id, path)))
        .collect();
    runs.sort_by(|a, b| b.cmp(a));
    runs.into_iter()
        .skip(keep)
        .filter(|(started, id, path)| {
            now.duration_since(*started)
                .is_ok_and(|old| old > RUN_MAX_AGE)
                && path.is_dir()
                && path.join("done.json").is_file()
                && worker_gone(actions, lock_dir, id, path)
        })
        .map(|(_, _, path)| path)
        .collect()
}

/// Files in `dir` named `<owner><suffix>` for one of `suffixes`, whose owner
/// is gone and which have sat untouched for [`SETTLED`].
fn ownerless_files(
    dir: &Path,
    suffixes: &[&str],
    owned: impl Fn(&str) -> Option<bool>,
    now: SystemTime,
) -> Vec<PathBuf> {
    files_in(dir)
        .into_iter()
        .filter(|(name, path)| {
            suffixes
                .iter()
                .find_map(|suffix| name.strip_suffix(suffix))
                .is_some_and(|owner| owned(owner) == Some(false))
                && older_than(path, SETTLED, now)
                && fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_dir())
        })
        .map(|(_, path)| path)
        .collect()
}

/// Whether other repositories keep their files in this cache root too: a
/// repository without its own `.wt.toml` gets the common one, with its tmux
/// socket and lock directory.
fn shared_cache_root(ctx: &AppContext) -> bool {
    ctx.config.repository_config.is_none()
}

/// The pass's stamp file and lock key. In a shared cache root each
/// repository has its own, or only the first one's pass would run each day.
fn pass_names(ctx: &AppContext) -> (PathBuf, String) {
    let root = &ctx.config.paths.cache_root;
    if shared_cache_root(ctx) {
        let id = &ctx.config.repo_id;
        (
            root.join(format!("maintenance.{id}.json")),
            format!("{LOCK}{id}"),
        )
    } else {
        (root.join(STAMP), LOCK.to_owned())
    }
}

fn survey_files(ctx: &AppContext, alive: &Alive, now: SystemTime) -> Leftovers {
    let paths = &ctx.config.paths;
    let mut found = Leftovers {
        runs: old_runs(
            &paths.log_dir.join("actions"),
            &paths.lock_dir,
            KEEP_RUNS,
            now,
        ),
        ..Leftovers::default()
    };
    // These files are named by slug or session alone, and this repository
    // knows only its own: in a shared cache root another repository's would
    // look ownerless.
    if shared_cache_root(ctx) {
        found
            .unknown
            .push("files in a cache root shared by repositories");
        return found;
    }
    if alive.slugs.is_none() {
        found.unknown.push("worktrees");
    }
    if alive.sessions.is_none() {
        found.unknown.push("sessions");
    }
    let by_slug = |owner: &str| {
        let slugs = alive.slugs.as_ref()?;
        Some(slugs.contains(owner))
    };
    // The longest suffix first: `.state.attempts` also ends a `.state` name.
    found.dev_files = ownerless_files(
        &paths.cache_root.join("dev"),
        &[".state.attempts", ".state", ".crash.log"],
        by_slug,
        now,
    );
    let by_session = |owner: &str| alive.owns(owner);
    found.sockets = ownerless_files(&paths.cache_root.join("insp"), &[".sock"], by_session, now);
    found.session_logs = ownerless_files(
        &paths.cache_root.join("sessions"),
        &[".err"],
        by_session,
        now,
    );
    found
}

/// Directory scans and removals stay off the async workers.
async fn off_thread(work: impl FnOnce() -> Leftovers + Send + 'static) -> Leftovers {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "maintenance scan failed");
            Leftovers::default()
        })
}

fn epoch_ms(now: SystemTime) -> i64 {
    i64::try_from(
        now.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// What a pass of `scope` would remove right now. Reads only, and counts
/// nothing such a pass would leave.
pub async fn survey(ctx: &AppContext, install: Option<&InstallPaths>, scope: Scope) -> Leftovers {
    let now = SystemTime::now();
    let alive = alive(ctx, install).await;
    let mut found = off_thread({
        let (ctx, alive) = (ctx.clone(), alive.clone());
        move || survey_files(&ctx, &alive, now)
    })
    .await;
    match (install, &alive.versions) {
        (Some(install), Some(pinned)) => {
            let store = wt_update::StateStore::new(install.clone());
            match store
                .load()
                .map_err(|error| error.to_string())
                .and_then(|state| {
                    wt_update::prunable_versions(
                        install,
                        &state,
                        pinned,
                        wt_update::KEEP_NEWEST_VERSIONS,
                    )
                    .map_err(|error| error.to_string())
                }) {
                Ok(versions) => found.versions = versions,
                Err(_) => found.unknown.push("installed versions"),
            }
        }
        (Some(_), None) => found.unknown.push("pinned versions"),
        (None, _) => {}
    }
    if scope.state_records
        && let Some(live) = alive.slugs.clone()
    {
        let state = ctx.database.call(|store| Ok(store.read_wt_state()?)).await;
        match state {
            Ok(state) => {
                found.slug_records = wt_store::prunable_slug_records(&state, &live, epoch_ms(now));
            }
            Err(_) => found.unknown.push("stored state"),
        }
    }
    found
}

fn removed(paths: Vec<PathBuf>, remove: impl Fn(&Path) -> io::Result<bool>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|path| match remove(path) {
            Ok(removed) => removed,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "maintenance could not remove a leftover");
                false
            }
        })
        .collect()
}

fn remove_file(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn sweep_files(ctx: &AppContext, alive: &Alive, now: SystemTime) -> Leftovers {
    let mut found = survey_files(ctx, alive, now);
    found.runs = removed(std::mem::take(&mut found.runs), |run| {
        fs::remove_dir_all(run)?;
        Ok(true)
    });
    found.dev_files = removed(std::mem::take(&mut found.dev_files), remove_file);
    found.sockets = removed(std::mem::take(&mut found.sockets), remove_file);
    found.session_logs = removed(std::mem::take(&mut found.session_logs), remove_file);
    found
}

async fn sweep(ctx: &AppContext, install: Option<&InstallPaths>, scope: Scope) -> Leftovers {
    let now = SystemTime::now();
    let alive = alive(ctx, install).await;
    let mut gone = off_thread({
        let (ctx, alive) = (ctx.clone(), alive.clone());
        move || sweep_files(&ctx, &alive, now)
    })
    .await;
    match (install, &alive.versions) {
        (Some(install), Some(pinned)) => {
            let pruned = tokio::task::spawn_blocking({
                let (install, pinned) = (install.clone(), pinned.clone());
                move || {
                    wt_update::prune_versions(&install, &pinned, wt_update::KEEP_NEWEST_VERSIONS)
                        .map_err(|error| error.to_string())
                }
            })
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
            match pruned {
                Ok(versions) => gone.versions = versions,
                Err(error) => {
                    tracing::warn!(%error, "maintenance could not prune installed versions");
                    gone.unknown.push("installed versions");
                }
            }
        }
        (Some(_), None) => gone.unknown.push("pinned versions"),
        (None, _) => {}
    }
    // The worktrees are read again here: a record is durable state, and the
    // list read when the pass began may be seconds old by now.
    let live = if scope.state_records {
        let live = live_slugs(ctx).await;
        if live.is_none() && alive.slugs.is_some() {
            gone.unknown.push("worktrees");
        }
        live
    } else {
        None
    };
    let at = epoch_ms(now);
    let stored = ctx
        .database
        .call(move |store| {
            // Both write the durable state, so both keep to the scope.
            let redated = if scope.state_records {
                store.redate_checks_from_history()?
            } else {
                Vec::new()
            };
            let pruned = match &live {
                Some(live) => store.prune_slug_records(live, at)?,
                None => Vec::new(),
            };
            store.checkpoint()?;
            Ok((pruned, redated))
        })
        .await;
    match stored {
        Ok((pruned, redated)) => {
            gone.slug_records = pruned;
            gone.redated_checks = redated;
        }
        Err(error) => {
            tracing::warn!(%error, "maintenance could not update the stored state");
            gone.unknown.push("stored state");
        }
    }
    let versions: Vec<String> = gone
        .versions
        .iter()
        .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
        .collect();
    let redated: Vec<String> = gone
        .redated_checks
        .iter()
        .map(|check| {
            format!(
                "{} {} {}->{}",
                check.slug,
                check.stage.as_str(),
                check.from_ms,
                check.to_ms
            )
        })
        .collect();
    if gone.is_empty() && gone.redated_checks.is_empty() && gone.unknown.is_empty() {
        tracing::debug!(target: "wt::maintenance", "maintenance pass found nothing to remove");
        return gone;
    }
    tracing::info!(
        target: "wt::maintenance",
        versions = ?versions,
        action_runs = gone.runs.len(),
        dev_files = gone.dev_files.len(),
        inspector_sockets = gone.sockets.len(),
        session_logs = gone.session_logs.len(),
        slug_records = ?gone.slug_records,
        redated_checks = ?redated,
        not_judged = ?gone.unknown,
        "maintenance pass removed leftovers"
    );
    gone
}

fn stamp_due(stamp: &Path, now: SystemTime) -> bool {
    let at = fs::read_to_string(stamp)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get("atMs")?.as_u64());
    let Some(at) = at else { return true };
    // A stamp from the future is a clock that moved: due.
    now.duration_since(UNIX_EPOCH + Duration::from_millis(at))
        .map_or(true, |since| since >= DAY)
}

async fn pass_lock(ctx: &AppContext) -> Option<wt_platform::lock::FileLock> {
    wt_platform::lock::FileLock::try_acquire(
        &ctx.config.paths.lock_dir,
        &pass_names(ctx).1,
        "maintenance",
    )
    .await
    .ok()?
}

async fn locked_pass(ctx: &AppContext, install: Option<&InstallPaths>, scope: Scope) -> Leftovers {
    let gone = sweep(ctx, install, scope).await;
    let stamp = pass_names(ctx).0;
    let at = serde_json::json!({"atMs": epoch_ms(SystemTime::now())});
    if let Err(error) = fs::write(&stamp, at.to_string()) {
        tracing::warn!(%error, "maintenance could not record that it ran");
    }
    gone
}

/// Run the pass now and return what it removed, or `None` when another
/// pass holds this repository's maintenance lock. One log line says what
/// went; the versions, slug records and re-dated checks are named in it.
pub async fn run(
    ctx: &AppContext,
    install: Option<&InstallPaths>,
    scope: Scope,
) -> Option<Leftovers> {
    let _lock = pass_lock(ctx).await?;
    Some(locked_pass(ctx, install, scope).await)
}

/// The daily pass, unless it is switched off or one ran for this repository
/// within the last day, in this process or another. `None` when it did not
/// run.
pub async fn sweep_if_due(ctx: &AppContext, install: Option<&InstallPaths>) -> Option<Leftovers> {
    let stamp = pass_names(ctx).0;
    if !ctx.config.maintenance.enabled || !stamp_due(&stamp, SystemTime::now()) {
        return None;
    }
    let _lock = pass_lock(ctx).await?;
    // Two processes started together both found it due.
    if !stamp_due(&stamp, SystemTime::now()) {
        return None;
    }
    Some(locked_pass(ctx, install, Scope::daily(ctx)).await)
}

/// Keep the pass running once a day until `stop`. The first check waits
/// until startup is over.
pub async fn daily(
    ctx: AppContext,
    install: Option<InstallPaths>,
    stop: tokio_util::sync::CancellationToken,
) {
    const SETTLE: Duration = Duration::from_secs(5 * 60);
    const RECHECK: Duration = Duration::from_secs(60 * 60);
    if !ctx.config.maintenance.enabled {
        return;
    }
    let mut wait = SETTLE;
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
        // A removal already under way finishes on its own thread.
        tokio::select! {
            () = stop.cancelled() => return,
            _ = sweep_if_due(&ctx, install.as_ref()) => {}
        }
        wait = RECHECK;
    }
}

/// Files from before the native rewrite that wt keeps and never removes:
/// how many and their size. `roots` are the cache root and its parent.
pub fn legacy_files(roots: &[&Path], install: Option<&InstallPaths>) -> (usize, u64) {
    let dated_log = |name: &str| {
        name.strip_prefix("wt-")
            .and_then(|rest| rest.strip_suffix(".log"))
            .is_some_and(|date| {
                date.len() == 10
                    && date
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte == b'-')
            })
    };
    let backup =
        |name: &str| name.starts_with("state.json.bak") || name.starts_with("archive.json.bak");
    let mut files: BTreeSet<PathBuf> = BTreeSet::new();
    for root in roots {
        let take = |dir: PathBuf, wanted: &dyn Fn(&str) -> bool, files: &mut BTreeSet<PathBuf>| {
            files.extend(
                files_in(&dir)
                    .into_iter()
                    .filter(|(name, _)| wanted(name))
                    .map(|(_, path)| path),
            );
        };
        take(root.to_path_buf(), &backup, &mut files);
        take(root.join("logs"), &dated_log, &mut files);
        take(root.join("logs/app"), &dated_log, &mut files);
    }
    if let Some(install) = install {
        files.extend(
            files_in(&install.root().join("migrations"))
                .into_iter()
                .filter(|(name, _)| name.starts_with("legacy-"))
                .map(|(_, path)| path),
        );
    }
    let bytes = files
        .iter()
        .filter_map(|path| fs::symlink_metadata(path).ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum();
    (files.len(), bytes)
}

/// The size of everything under `path`, without following links.
pub fn tree_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    files_in(path)
        .iter()
        .map(|(_, child)| tree_bytes(child))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_app_core::test_support::CommandFixture;

    fn aged(path: &Path, age: Duration) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let file = fs::File::create(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        let mut names: Vec<String> = paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn run_id(age: Duration, seq: u32) -> String {
        format!("{}-4242-{seq}", epoch_ms(SystemTime::now() - age))
    }

    fn hold(path: &Path) -> fs::File {
        use std::os::fd::AsRawFd;
        let file = fs::File::open(path).unwrap();
        // SAFETY: the descriptor is owned by `file` and open for this call.
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        file
    }

    /// Unlock, then close. Closing alone frees a lock only once every copy
    /// of the descriptor is closed, and a child another test's thread is
    /// starting holds a copy from its fork until its exec.
    fn release(file: fs::File) {
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor is owned by `file` and open for this call.
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) }, 0);
    }

    fn tmux(ctx: &AppContext, args: &[&str]) -> bool {
        std::process::Command::new("tmux")
            .args(["-L", ctx.config.tmux.socket.as_str()])
            .args(args)
            .current_dir(&ctx.home)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[test]
    fn a_pinned_version_is_read_from_a_path_or_an_executable() {
        let versions = Path::new("/home/me/.local/share/wt/versions");
        let listed = "claude PATH=/shims:/home/me/.local/share/wt/versions/v1-a-t/bin:/usr/bin HOME=/home/me\n\
            /home/me/.local/share/wt/versions/v2-b-t/bin/wt _restack-worker\n\
            vim /home/me/.local/share/wt/versions\n";
        assert_eq!(
            pinned_versions(listed, versions)
                .into_iter()
                .collect::<Vec<_>>(),
            ["v1-a-t", "v2-b-t"]
        );
    }

    #[test]
    fn every_listed_checkout_is_a_live_worktree() {
        let listed = "worktree /repo/main\0HEAD abc\0branch refs/heads/main\0\0\
            worktree /repo/wts/one\0HEAD abc\0detached\0\0\
            worktree /elsewhere/wt-verify-1\0HEAD abc\0\0";
        assert_eq!(
            listed_worktrees(listed).into_iter().collect::<Vec<_>>(),
            ["main", "one", "wt-verify-1"]
        );
    }

    #[test]
    fn only_old_finished_runs_past_the_newest_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (actions, locks) = (dir.path().join("actions"), dir.path().join("locks"));
        fs::create_dir_all(&locks).unwrap();
        let month = 40 * DAY;
        let finished_old = run_id(month, 1);
        let running_old = run_id(month, 2);
        let finished_recent = run_id(DAY, 3);
        let finished_older = run_id(month * 2, 4);
        let finished_held = run_id(month, 5);
        let finished_busy = run_id(month, 6);
        let finished_nameless = run_id(month, 7);
        for (id, done) in [
            (&finished_old, true),
            (&running_old, false),
            (&finished_recent, true),
            (&finished_older, true),
            (&finished_held, true),
            (&finished_busy, true),
            (&finished_nameless, true),
        ] {
            fs::create_dir_all(actions.join(id)).unwrap();
            let meta = if id == &finished_nameless {
                "{}".to_owned()
            } else if id == &finished_busy {
                r#"{"slug":"busy"}"#.to_owned()
            } else {
                format!(r#"{{"slug":"row-{id}"}}"#)
            };
            fs::write(actions.join(id).join("meta.json"), meta).unwrap();
            if done {
                fs::write(actions.join(id).join("done.json"), "{}").unwrap();
            }
        }
        fs::create_dir_all(actions.join("not-a-run")).unwrap();
        // Its worker is still alive, whatever done.json says.
        let lock = locks.join(format!("action-run-{finished_held}.lock"));
        fs::write(&lock, "").unwrap();
        fs::write(locks.join(format!("action-run-{finished_old}.lock")), "").unwrap();
        let held = hold(&lock);
        // A worker is alive on this run's worktree, under the lock named
        // for the worktree; and a run that names no worktree proves nothing.
        let busy = locks.join(format!(
            "{}.lock",
            wt_actions::worker_lock_key(dir.path(), "busy")
        ));
        fs::write(&busy, "").unwrap();
        let working = hold(&busy);
        let now = SystemTime::now();
        assert_eq!(
            names(&old_runs(&actions, &locks, 0, now)),
            names(&[actions.join(&finished_old), actions.join(&finished_older)])
        );
        // The newest runs stay whatever their age.
        assert_eq!(
            names(&old_runs(&actions, &locks, 6, now)),
            std::slice::from_ref(&finished_older)
        );
        assert!(old_runs(&actions, &locks, 7, now).is_empty());
        release(held);
        assert_eq!(old_runs(&actions, &locks, 0, now).len(), 3);
        release(working);
        assert_eq!(old_runs(&actions, &locks, 0, now).len(), 4);
        assert!(
            lock.exists(),
            "asking whether a lock is free removes nothing"
        );
    }

    /// One pass over a repository whose worktree "gone" was removed weeks
    /// ago while "one" and "two" live on, with a checkout outside the
    /// worktree root: everything of "gone" goes, nothing recent or owned by
    /// a worktree Git still lists does, and no lock file is touched.
    #[tokio::test]
    async fn a_pass_removes_only_what_nothing_owns() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let paths = &ctx.config.paths;
        let old = 10 * DAY;
        let outside = fixture._root.path().join("elsewhere/outside");
        let added = std::process::Command::new("git")
            .arg("-C")
            .arg(&paths.main_clone)
            .args(["worktree", "add", "-q", "-b", "feature/outside"])
            .arg(&outside)
            .output()
            .unwrap();
        assert!(added.status.success());
        let locks = [
            "gone.lock",
            "__start__gone-codex.lock",
            "automations.json.lock",
            "action-run-1-2-3.lock",
        ];
        for name in locks {
            aged(&paths.lock_dir.join(name), old);
        }
        let cache = &paths.cache_root;
        for name in [
            "gone.state",
            "gone.state.attempts",
            "gone.crash.log",
            "one.state",
            "outside.state",
        ] {
            aged(&cache.join("dev").join(name), old);
        }
        aged(&cache.join("dev/gone-recent.state"), DAY);
        fs::create_dir_all(cache.join("dev/waiting")).unwrap();
        for name in ["gone.sock", "gone~2.sock", "one.sock"] {
            aged(&cache.join("insp").join(name), old);
        }
        for name in ["gone.err", "two~2.err", "outside.err"] {
            aged(&cache.join("sessions").join(name), old);
        }

        let install = InstallPaths::new(fixture._root.path().join("install")).unwrap();
        let version = |release: &str| wt_update::VersionId::new(release, "b", "t").unwrap();
        for (age, release) in ["v6", "v5", "v4", "v3", "v2", "v1"].iter().enumerate() {
            let dir = install.version_dir(&version(release));
            fs::create_dir_all(dir.join("bin")).unwrap();
            fs::File::open(&dir)
                .unwrap()
                .set_modified(SystemTime::now() - DAY * (age as u32 + 1))
                .unwrap();
        }
        let mut installed = wt_update::InstallState::new(wt_update::Channel::default());
        installed.current = Some(version("v6"));
        wt_update::StateStore::new(install.clone())
            .save(&installed)
            .unwrap();
        // An idle session whose next `wt` resolves to the oldest build:
        // nothing running names it, only the session's environment.
        assert!(tmux(
            ctx,
            &["new-session", "-d", "-s", "idle", "sleep", "600"]
        ));
        let pin = format!(
            "{}:/usr/bin:/bin",
            install.version_dir(&version("v1")).join("bin").display()
        );
        assert!(tmux(ctx, &["set-environment", "-t", "=idle", "PATH", &pin]));

        ctx.database
            .call(|store| {
                let mut state = store.read_wt_state()?;
                let record = serde_json::json!({
                    "createdAt": "2026-01-01T00:00:00.000Z", "order": 1.0, "section": null
                });
                for slug in ["gone", "one", "outside"] {
                    state["slugs"][slug] = record.clone();
                }
                Ok(store.write_repository_state_json(&serde_json::to_string(&state)?)?)
            })
            .await
            .unwrap();

        let found = survey(ctx, Some(&install), Scope::ASKED).await;
        assert!(found.unknown.is_empty(), "{:?}", found.unknown);
        assert_eq!(found.slug_records, ["gone"]);
        assert!(
            survey(ctx, Some(&install), Scope::daily(ctx))
                .await
                .slug_records
                .is_empty(),
            "the daily pass leaves durable state alone unless told otherwise"
        );
        assert!(
            cache.join("dev/gone.state").exists(),
            "a survey removes nothing"
        );

        // The daily pass: leftover files, and nothing in the durable state.
        let before = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let daily = sweep_if_due(ctx, Some(&install)).await.unwrap();
        assert!(daily.slug_records.is_empty());
        let after = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(
            names(&daily.dev_files),
            ["gone.crash.log", "gone.state", "gone.state.attempts"]
        );
        assert_eq!(names(&daily.sockets), ["gone.sock", "gone~2.sock"]);
        assert_eq!(names(&daily.session_logs), ["gone.err"]);
        assert_eq!(names(&daily.versions), [version("v2").key()]);
        assert!(daily.runs.is_empty());
        assert!(sweep_if_due(ctx, Some(&install)).await.is_none());
        for name in locks {
            assert!(paths.lock_dir.join(name).exists(), "{name} must stay");
        }
        for path in [
            cache.join("dev/one.state"),
            cache.join("dev/outside.state"),
            cache.join("dev/gone-recent.state"),
            cache.join("dev/waiting"),
            cache.join("insp/one.sock"),
            cache.join("sessions/two~2.err"),
            cache.join("sessions/outside.err"),
            install.version_dir(&version("v1")),
            install.version_dir(&version("v3")),
        ] {
            assert!(path.exists(), "{} must stay", path.display());
        }

        // Asked for by a person: the record goes too, and only that one.
        let asked = run(ctx, Some(&install), Scope::ASKED).await.unwrap();
        assert_eq!(asked.slug_records, ["gone"]);
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert!(state["slugs"]["one"].is_object() && state["slugs"]["outside"].is_object());
        assert!(
            survey(ctx, Some(&install), Scope::ASKED).await.is_empty(),
            "what a survey counts is what a pass removes"
        );
        assert!(tmux(ctx, &["kill-server"]));
        fixture.close().await.unwrap();
    }

    /// Without its own `.wt.toml` a repository shares its cache root, tmux
    /// socket and lock directory with every other such repository, whose
    /// files look ownerless from here.
    #[tokio::test]
    async fn a_shared_cache_root_keeps_every_file_named_by_slug() {
        let fixture = CommandFixture::new().await.unwrap();
        let mut config = (*fixture.ctx.config).clone();
        config.repository_config = None;
        let mut ctx = fixture.ctx.clone();
        ctx.config = std::sync::Arc::new(config);
        let ctx = &ctx;
        let paths = &ctx.config.paths;
        let old = 40 * DAY;
        let theirs = [
            paths.cache_root.join("dev/another-repos-worktree.state"),
            paths.cache_root.join("insp/another-repos-worktree.sock"),
            paths.cache_root.join("sessions/another-repos-worktree.err"),
        ];
        for path in &theirs {
            aged(path, old);
        }
        let found = survey(ctx, None, Scope::ASKED).await;
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(
            found.unknown,
            ["files in a cache root shared by repositories"]
        );
        assert!(run(ctx, None, Scope::ASKED).await.unwrap().is_empty());
        assert!(theirs.iter().all(|path| path.exists()));

        // Each repository keeps its own day: this one's pass leaves the
        // shared stamp and lock to nobody, and another's is still due.
        let (stamp, lock) = pass_names(ctx);
        assert!(stamp.exists() && !paths.cache_root.join(STAMP).exists());
        assert!(!stamp_due(&stamp, SystemTime::now()));
        let mut other = (*ctx.config).clone();
        other.repo_id = "another-repository".into();
        let mut another = ctx.clone();
        another.config = std::sync::Arc::new(other);
        assert_ne!(pass_names(&another), (stamp, lock));
        let held = pass_lock(ctx).await.unwrap();
        assert!(sweep_if_due(&another, None).await.is_some());
        drop(held);
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn the_pass_runs_once_a_day_and_never_beside_another() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let stamp = ctx.config.paths.cache_root.join(STAMP);

        let mut off = (*ctx.config).clone();
        off.maintenance.enabled = false;
        let mut switched_off = ctx.clone();
        switched_off.config = std::sync::Arc::new(off);
        assert!(sweep_if_due(&switched_off, None).await.is_none());
        assert!(!stamp.exists());

        let other = pass_lock(ctx).await.unwrap();
        assert!(run(ctx, None, Scope::ASKED).await.is_none());
        assert!(sweep_if_due(ctx, None).await.is_none());
        drop(other);

        // A pass a person asked for counts as the day's pass.
        assert!(run(ctx, None, Scope::ASKED).await.is_some());
        assert!(sweep_if_due(ctx, None).await.is_none());
        let now = SystemTime::now();
        assert!(!stamp_due(&stamp, now));
        assert!(stamp_due(&stamp, now + DAY));
        assert!(stamp_due(&stamp, now - DAY), "a clock that moved back");
        fs::write(&stamp, "not json").unwrap();
        assert!(stamp_due(&stamp, now));
        assert!(sweep_if_due(ctx, None).await.is_some());
        fixture.close().await.unwrap();
    }
}
