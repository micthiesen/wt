use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const NEVER_SHIM: &[&str] = &["claude", "codex", "opencode", "pi"];
const ALWAYS_SHIM: &[&str] = &["bun"];
const LAUNCHER_SHIM: &[&str] = &["pnpm", "pnpx", "npx", "yarn"];
/// The guard's exit code for "do not create this pull request". No other wt
/// exit shares it, so a wt that failed for its own reasons is never read as
/// a refusal.
pub const GH_GUARD_REFUSED: i32 = 83;
const MIN_COMPILED_BYTES: u64 = 8 * 1024 * 1024;
const HEAD_BYTES: usize = 64 * 1024;
/// `gh pr checks --watch` polls every 10 seconds unless told otherwise.
const WATCH_INTERVAL_SECS: u32 = 30;
/// The gh call log moves aside past this size, so it and its one
/// predecessor stay near a megabyte together.
const GH_CALL_LOG_CAP_BYTES: u64 = 512 * 1024;
/// A shim write finishes in well under this; an older temp file is orphaned.
const ORPHANED_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// The directory that goes first on the `PATH` of every session wt starts.
pub fn shim_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("shims")
}

/// Where the `gh` shim records each call: the time, the session and the
/// subcommand words.
pub(crate) fn gh_call_log(cache_dir: &Path) -> PathBuf {
    cache_dir.join("logs/gh-calls.log")
}

/// Best-effort machine-local shims: the BUN_INSPECT shields and the `gh`
/// guard. Failure to generate them must not prevent a session from starting.
fn ensure_session_shims(cache_dir: &Path, path_value: &str, tmux: &wt_tmux::TmuxClient) {
    let launcher = stable_launcher(tmux);
    if ensure_inspect_shims_at(&shim_dir(cache_dir), path_value, launcher.as_deref()).is_err() { /* advisory by contract */
    }
}

/// The installed launcher of the sessions `tmux` creates. It always runs the
/// current release, where the `wt` on a session's `PATH` is the build that
/// started the session and may predate the guard. Only a client that pins an
/// install root names one; nothing is guessed from the caller's environment.
fn stable_launcher(tmux: &wt_tmux::TmuxClient) -> Option<PathBuf> {
    tmux.session_environment_value("WT_INSTALL_ROOT")
        .filter(|root| !root.is_empty())
        .map(|root| Path::new(root).join("bin/wt"))
}

/// Writes the shims for a session of `tmux` whose `PATH` is `path_value` and
/// returns that `PATH` with their directory first.
pub(crate) fn session_path(
    cache_dir: &Path,
    path_value: &str,
    tmux: &wt_tmux::TmuxClient,
) -> String {
    ensure_session_shims(cache_dir, path_value, tmux);
    prepend_path(&shim_dir(cache_dir), path_value)
}

/// `command` as a pane of `tmux` runs it, behind the shims and carrying
/// `agent` as `WT_AGENT`. The directory is prepended inside the pane because
/// only there is `PATH` final: the session environment is applied after the
/// user's shell startup files.
pub(crate) fn session_command(
    cache_dir: &Path,
    tmux: &wt_tmux::TmuxClient,
    agent: &str,
    command: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let path = tmux
        .session_environment_value("PATH")
        .map(str::to_owned)
        .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
    ensure_session_shims(cache_dir, &path, tmux);
    [
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        r#"PATH=$1${PATH:+:$PATH}; WT_AGENT=$2; export PATH WT_AGENT; shift 2; exec "$@""#
            .to_owned(),
        "sh".to_owned(),
        shim_dir(cache_dir).to_string_lossy().into_owned(),
        agent.to_owned(),
    ]
    .into_iter()
    .chain(command)
    .collect()
}

fn prepend_path(shims: &Path, path: &str) -> String {
    // A trailing ':' would add an empty entry, which shells read as the
    // current directory.
    if path.is_empty() {
        shims.to_string_lossy().into_owned()
    } else {
        format!("{}:{path}", shims.to_string_lossy())
    }
}

pub(crate) fn ensure_inspect_shims_at(
    dir: &Path,
    path_value: &str,
    launcher: Option<&Path>,
) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let mut targets = discover_bun_executables(path_value, dir);
    for cmd in ALWAYS_SHIM.iter().chain(LAUNCHER_SHIM) {
        if !targets.contains_key(*cmd)
            && let Some(path) = resolve_outside_shims(cmd, path_value, dir)
        {
            targets.insert((*cmd).to_owned(), path);
        }
    }
    for cmd in NEVER_SHIM {
        targets.remove(*cmd);
    }
    for (cmd, real) in &targets {
        let file = dir.join(cmd);
        let body = shim_body(cmd, real, dir);
        if fs::read_to_string(&file).ok().as_deref() != Some(body.as_str()) {
            write_shim(dir, cmd, body.as_bytes())?;
        }
        fs::set_permissions(file, fs::Permissions::from_mode(0o700))?;
    }
    let mut keep: HashSet<_> = targets.keys().cloned().collect();
    if let Some(real) = resolve_outside_shims("gh", path_value, dir) {
        let file = dir.join("gh");
        let log = dir.parent().map(gh_call_log);
        if let Some(logs) = log.as_deref().and_then(Path::parent) {
            // The shim only appends; without the directory it logs nothing.
            let _ = fs::create_dir_all(logs);
        }
        let body = gh_shim_body(&real, dir, launcher, log.as_deref());
        if fs::read_to_string(&file).ok().as_deref() != Some(body.as_str()) {
            write_shim(dir, "gh", body.as_bytes())?;
        }
        fs::set_permissions(file, fs::Permissions::from_mode(0o700))?;
        keep.insert("gh".to_owned());
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let remove = if name.starts_with('.') {
                // A young temp file is another starter's in-flight
                // replacement; an old one was orphaned by a crash between
                // create and rename.
                name.ends_with(".tmp") && is_orphaned_temp(&entry)
            } else {
                !keep.contains(&name)
            };
            if remove {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

fn is_orphaned_temp(entry: &fs::DirEntry) -> bool {
    entry
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= ORPHANED_TEMP_AGE)
}

/// Running sessions execute these shims while another session starts, so a
/// rewrite must replace the file whole rather than truncate it in place.
fn write_shim(dir: &Path, cmd: &str, body: &[u8]) -> std::io::Result<()> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = dir.join(format!(".{cmd}.{}.{sequence}.tmp", std::process::id()));
    let result = (|| {
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&temporary)?;
        out.write_all(body)?;
        fs::rename(&temporary, dir.join(cmd))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn stale_harness_shims(cache_dir: &Path) -> Vec<String> {
    let dir = shim_dir(cache_dir);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let present: HashSet<_> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    NEVER_SHIM
        .iter()
        .filter(|name| present.contains(**name))
        .map(|name| (*name).to_owned())
        .collect()
}

fn discover_bun_executables(path_value: &str, shim_dir: &Path) -> HashMap<String, String> {
    let mut found = HashMap::new();
    let mut claimed = HashSet::new();
    for directory in std::env::split_paths(path_value) {
        if directory == shim_dir {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !claimed.insert(name.clone()) {
                continue;
            }
            let path = entry.path();
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file()
                || metadata.permissions().mode() & 0o111 == 0
                || metadata.len() < MIN_COMPILED_BYTES
            {
                continue;
            }
            if is_bun_compiled(&path) {
                found.insert(name, path.to_string_lossy().into_owned());
            }
        }
    }
    found
}

fn is_bun_compiled(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut head = vec![0; HEAD_BYTES];
    let Ok(len) = file.read(&mut head) else {
        return false;
    };
    head.truncate(len);
    head.windows(b"__BUN".len())
        .any(|window| window == b"__BUN")
}

fn resolve_outside_shims(command: &str, path_value: &str, shim_dir: &Path) -> Option<String> {
    for directory in std::env::split_paths(path_value) {
        if directory == shim_dir {
            continue;
        }
        let candidate = directory.join(command);
        if let Ok(meta) = fs::metadata(&candidate)
            && meta.is_file()
            && meta.permissions().mode() & 0o111 != 0
        {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

fn shim_body(command: &str, real: &str, shim_dir: &Path) -> String {
    let cmd = sh_quote(command);
    let real = sh_quote(real);
    let shims = sh_quote(&shim_dir.to_string_lossy());
    format!(
        "#!/bin/sh\n# Generated by wt. Strips BUN_INSPECT from child programs.\nreal={real}\nif [ -x \"$real\" ]; then exec env -u BUN_INSPECT \"$real\" \"$@\"; fi\nself=${{0%/*}}\nnewpath=\nIFS=:\nfor p in $PATH; do\n  [ -z \"$p\" ] && continue\n  [ \"$p\" = {shims} ] && continue\n  [ \"$p\" = \"$self\" ] && continue\n  if [ -z \"$newpath\" ]; then newpath=$p; else newpath=$newpath:$p; fi\ndone\nunset IFS\nPATH=$newpath\nexport PATH\nreal=$(command -v {cmd}) || {{ echo \"wt shim: {command} not found outside shim directory\" >&2; exit 127; }}\nexec env -u BUN_INSPECT \"$real\" \"$@\"\n"
    )
}

/// `gh` runs untouched except for `gh pr create`, which first asks
/// `wt _gh-guard` whether this worktree may open a pull request. Only the
/// guard's explicit refusal stops it; a missing `wt` or any other failure
/// says so in one line and lets `gh` run. Which repository the pull request
/// is for (`--repo`, `-R`, `GH_REPO`) is the guard's to compare with the
/// worktree's, so those reach it untouched. The guard is the installed
/// `launcher` when there is one, else the `wt` on `PATH`; a wt from before
/// the guard rejects the subcommand (exit 2) and `gh` runs with nothing said.
///
/// Two things apply to every call. Each is appended to `log` as its time,
/// its session and its subcommand words, never an argument: a word that is
/// not all lowercase letters and hyphens is a flag's value, and `gh api`
/// takes none. And `gh pr checks --watch` with no interval of its own polls
/// every [`WATCH_INTERVAL_SECS`]. Neither can stop `gh` from running.
///
/// Concurrent calls that all find the log over its cap rotate it once: the
/// one that makes `<log>.lock` measures again before it renames, so a
/// second rename cannot put the fresh log over the rotated one. A call that
/// died holding the lock would end rotation for good, so a log that has
/// reached twice the cap takes the lock away for the next call. A pull request aimed at another
/// repository (`--repo`, `-R`, `GH_REPO`) is not this worktree's and is
/// not asked about. The guard never sees stdin, which belongs to `gh`
/// (`--body-file -`).
fn gh_shim_body(
    real: &str,
    shim_dir: &Path,
    launcher: Option<&Path>,
    log: Option<&Path>,
) -> String {
    let log = sh_quote(&log.map(|path| path.to_string_lossy()).unwrap_or_default());
    let real = sh_quote(real);
    let launcher = sh_quote(
        &launcher
            .map(|path| path.to_string_lossy())
            .unwrap_or_default(),
    );
    let shims = sh_quote(&shim_dir.to_string_lossy());
    format!(
        r#"#!/bin/sh
# Generated by wt. One pull request per worktree: asks wt before gh pr create.
real={real}
if [ ! -x "$real" ]; then
  self=${{0%/*}}
  newpath=
  IFS=:
  for p in $PATH; do
    [ -z "$p" ] && continue
    [ "$p" = {shims} ] && continue
    [ "$p" = "$self" ] && continue
    if [ -z "$newpath" ]; then newpath=$p; else newpath=$newpath:$p; fi
  done
  unset IFS
  real=$(PATH=$newpath command -v gh) || {{ echo "wt shim: gh not found outside shim directory" >&2; exit 127; }}
fi
first=
second=
value=
watch=
interval=
for arg in "$@"; do
  if [ -n "$value" ]; then value=; continue; fi
  case $arg in
    -R|--repo) value=1 ;;
    --watch) watch=1 ;;
    -i*|--interval|--interval=*) interval=1 ;;
    -*) ;;
    *)
      if [ -z "$first" ]; then first=$arg; elif [ -z "$second" ]; then second=$arg; fi
      ;;
  esac
done
log={log}
if [ -n "$log" ]; then
  {{
    words=
    case $first in
      ''|*[!a-z-]*) ;;
      *)
        words=$first
        case $second in
          ''|*[!a-z-]*) ;;
          *) [ "$first" != api ] && words="$words $second" ;;
        esac
        ;;
    esac
    size=$(wc -c <"$log") && [ "$size" -gt {cap} ] && {{
      if mkdir "$log.lock"; then
        size=$(wc -c <"$log") && [ "$size" -gt {cap} ] && mv -f "$log" "$log.1"
        rmdir "$log.lock"
      elif [ "$size" -gt {abandoned} ]; then
        rmdir "$log.lock"
      fi
    }}
    printf '%s\t%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${{WT_AGENT:-$PWD}}" "${{words:--}}" >>"$log"
  }} 2>/dev/null
fi
if [ "$first" = pr ] && [ "$second" = checks ] && [ -n "$watch" ] && [ -z "$interval" ]; then
  set -- "$@" --interval {watch}
fi
if [ "$first" = pr ] && [ "$second" = create ] && [ -z "$WT_GH_GUARD" ]; then
  wt={launcher}
  [ -x "$wt" ] || wt=$(command -v wt) || wt=
  if [ -n "$wt" ]; then
    said=$(WT_GH_GUARD=1 WT_REAL_GH=$real "$wt" _gh-guard "$@" 2>&1 >/dev/null </dev/null)
    code=$?
    if [ "$code" -ne 2 ]; then
      [ -n "$said" ] && printf '%s\n' "$said" >&2
      [ "$code" -eq {refused} ] && exit 1
      [ "$code" -ne 0 ] && echo "wt shim: could not check this worktree's pull request (wt exit $code); running gh unchecked" >&2
    fi
  else
    echo "wt shim: wt is not on PATH; running gh unchecked" >&2
  fi
fi
exec "$real" "$@"
"#,
        refused = GH_GUARD_REFUSED,
        cap = GH_CALL_LOG_CAP_BYTES,
        abandoned = 2 * GH_CALL_LOG_CAP_BYTES,
        watch = WATCH_INTERVAL_SECS,
    )
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Fakes for a session behind the shims: a `gh` that prints its arguments
/// and a `wt` whose guard refuses every pull request.
#[cfg(test)]
pub(crate) mod fixture {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    pub(crate) const REPORT: &str = "create=1\ngh:pr view\nview=0\n";

    fn executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub(crate) fn refusing_bin(root: &Path) -> PathBuf {
        let bin = root.join("guard-bin");
        fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("gh"), "#!/bin/sh\necho \"gh:$*\"\n");
        executable(
            &bin.join("wt"),
            &format!("#!/bin/sh\nexit {}\n", super::GH_GUARD_REFUSED),
        );
        bin
    }

    /// A stand-in harness named `name` that writes to `report` what
    /// `gh pr create` and `gh pr view` did inside its session.
    pub(crate) fn reporting_harness(bin: &Path, name: &str, report: &Path) {
        executable(
            &bin.join(name),
            &format!(
                "#!/bin/sh\nr='{report}'\ngh pr create --draft >/dev/null 2>&1\necho \"create=$?\" >\"$r.tmp\"\ngh pr view >>\"$r.tmp\"\necho \"view=$?\" >>\"$r.tmp\"\nmv \"$r.tmp\" \"$r\"\nexec sleep 30\n",
                report = report.display()
            ),
        );
    }

    /// The session environment of a pane that finds the fakes first, whatever
    /// the test process itself was started with.
    pub(crate) fn session_environment(bin: &Path) -> Vec<(String, String)> {
        vec![
            ("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())),
            ("GH_REPO".into(), String::new()),
            ("WT_GH_GUARD".into(), String::new()),
        ]
    }

    pub(crate) fn await_report(report: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(text) = fs::read_to_string(report) {
                return text;
            }
            assert!(Instant::now() < deadline, "the session never reported");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn the_shim_directory_goes_first_without_an_empty_entry() {
        assert_eq!(
            prepend_path(Path::new("/cache/shims"), "/usr/bin:/bin"),
            "/cache/shims:/usr/bin:/bin"
        );
        assert_eq!(prepend_path(Path::new("/cache/shims"), ""), "/cache/shims");
    }

    /// The pane's own PATH decides what runs, not the one the shims were
    /// generated from, a pane with no PATH gains no empty entry, and the
    /// session is named by its own slug whoever launched it.
    #[test]
    fn a_session_command_runs_behind_the_shims_on_the_panes_path() {
        let tmp = tempdir().unwrap();
        let bin = fixture::refusing_bin(tmp.path());
        let report = tmp.path().join("report");
        fixture::reporting_harness(&bin, "agent", &report);
        let path = format!("{}:/usr/bin:/bin", bin.display());
        let tmux = wt_tmux::TmuxClient::new(
            wt_platform::process::ProcessRunner::default(),
            wt_tmux::TmuxServer::at(tmp.path().join("unused.sock")),
        )
        .with_session_environment([("PATH".to_owned(), path.clone())]);
        let cache = tmp.path().join("cache");
        let command = session_command(&cache, &tmux, "fixture-slug", ["agent".to_owned()]);
        let mut child = std::process::Command::new(&command[0])
            .args(&command[1..])
            .env("PATH", &path)
            .env_remove("WT_GH_GUARD")
            .env_remove("GH_REPO")
            .spawn()
            .unwrap();
        let reported = fixture::await_report(&report);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(reported, fixture::REPORT);

        let shown = std::process::Command::new(&command[0])
            .args(&command[1..command.len() - 1])
            .args(["/bin/sh", "-c", "echo \"$PATH $WT_AGENT\""])
            .env("PATH", "")
            .env("WT_AGENT", "the-launching-session")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&shown.stdout).trim_end(),
            format!("{} fixture-slug", shim_dir(&cache).to_string_lossy())
        );
    }

    #[test]
    fn shims_strip_bun_inspect_and_never_shadow_claude() {
        let tmp = tempdir().unwrap();
        let bin = tmp.path().join("bin");
        let shims = tmp.path().join("cache/shims");
        fs::create_dir_all(&bin).unwrap();
        let bun = bin.join("bun");
        fs::write(&bun, b"fake executable __BUN").unwrap();
        fs::set_permissions(&bun, fs::Permissions::from_mode(0o755)).unwrap();
        // Small fixture: the Mach-O marker is ignored under the configured
        // binary size floor, while bun itself is forced into the shim set.
        ensure_inspect_shims_at(&shims, &bin.to_string_lossy(), None).unwrap();
        assert!(shims.join("bun").exists());
        assert!(!shims.join("claude").exists());
        assert_eq!(
            fs::metadata(shims.join("bun"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(stale_harness_shims(tmp.path()).is_empty());
        // Another starter's in-flight replacement survives cleanup, while a
        // rewritten shim keeps the same path as a whole new file.
        fs::write(shims.join(".bun.1.0.tmp"), "partial").unwrap();
        fs::write(shims.join("bun"), "stale").unwrap();
        ensure_inspect_shims_at(&shims, &bin.to_string_lossy(), None).unwrap();
        assert!(shims.join(".bun.1.0.tmp").exists());
        assert!(
            fs::read_to_string(shims.join("bun"))
                .unwrap()
                .contains("BUN_INSPECT")
        );
    }

    fn executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn run_gh_shim(guard: Option<i32>, args: &[&str]) -> (i32, String, String) {
        run_gh_shim_with(guard, args, "", None, None)
    }

    /// Runs the generated gh shim with fakes for gh and wt on PATH. The fake
    /// gh prints its arguments and its stdin and exits 7; the fake wt reads
    /// whatever stdin it is given and exits `guard`.
    fn run_gh_shim_with(
        guard: Option<i32>,
        args: &[&str],
        stdin: &str,
        gh_repo: Option<&str>,
        launcher: Option<&Path>,
    ) -> (i32, String, String) {
        let tmp = tempdir().unwrap();
        run_gh_shim_in(tmp.path(), guard, args, stdin, gh_repo, launcher)
    }

    /// The same, in a cache root the caller keeps to read afterwards.
    fn run_gh_shim_in(
        root: &Path,
        guard: Option<i32>,
        args: &[&str],
        stdin: &str,
        gh_repo: Option<&str>,
        launcher: Option<&Path>,
    ) -> (i32, String, String) {
        let bin = root.join("bin");
        let shims = root.join("shims");
        fs::create_dir_all(&bin).unwrap();
        executable(
            &bin.join("gh"),
            "#!/bin/sh\nprintf 'gh'; for a in \"$@\"; do printf ' [%s]' \"$a\"; done; echo\ncat\nexit 7\n",
        );
        if let Some(code) = guard {
            executable(
                &bin.join("wt"),
                &format!(
                    "#!/bin/sh\ncat >/dev/null\necho \"guard $WT_GH_GUARD $*${{GH_REPO:+ GH_REPO=$GH_REPO}}\" >&2\n[ -x \"$WT_REAL_GH\" ] || exit 99\nexit {code}\n"
                ),
            );
        }
        let path = format!("{}:/usr/bin:/bin", bin.display());
        ensure_inspect_shims_at(&shims, &path, launcher).unwrap();
        let mut command = std::process::Command::new(shims.join("gh"));
        command
            .args(args)
            .env("PATH", format!("{}:{path}", shims.display()))
            .env_remove("WT_GH_GUARD")
            .env_remove("GH_REPO")
            .env("WT_AGENT", "fixture-slug")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(repo) = gh_repo {
            command.env("GH_REPO", repo);
        }
        let mut child = command.spawn().unwrap();
        // Dropping the handle closes the pipe, which ends the fakes' `cat`.
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    #[test]
    fn the_gh_shim_asks_wt_only_before_pr_create_and_fails_open() {
        // Anything but pr create reaches gh untouched: same arguments, same
        // exit code, and wt is never run.
        let (code, out, err) =
            run_gh_shim(Some(GH_GUARD_REFUSED), &["pr", "view", "--json", "a b"]);
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (7, "gh [pr] [view] [--json] [a b]\n", "")
        );
        let (code, out, _) = run_gh_shim(Some(GH_GUARD_REFUSED), &["api", "pr", "create"]);
        assert_eq!((code, out.as_str()), (7, "gh [api] [pr] [create]\n"));

        // An allowed pr create runs gh with every argument intact.
        let (code, out, err) = run_gh_shim(Some(0), &["pr", "create", "--title", "a b"]);
        assert_eq!(
            (code, out.as_str()),
            (7, "gh [pr] [create] [--title] [a b]\n")
        );
        assert_eq!(err, "guard 1 _gh-guard pr create --title a b\n");

        // A refusal stops before gh and exits non-zero.
        let (code, out, _) = run_gh_shim(Some(GH_GUARD_REFUSED), &["pr", "create", "--draft"]);
        assert_eq!((code, out.as_str()), (1, ""));

        // wt failing or missing never breaks gh: one warning line, then gh.
        let (code, out, err) = run_gh_shim(Some(1), &["pr", "create"]);
        assert_eq!((code, out.as_str()), (7, "gh [pr] [create]\n"));
        assert!(err.contains("running gh unchecked"), "{err}");
        let (code, out, err) = run_gh_shim(None, &["pr", "create"]);
        assert_eq!((code, out.as_str()), (7, "gh [pr] [create]\n"));
        assert_eq!(err, "wt shim: wt is not on PATH; running gh unchecked\n");
    }

    /// A session keeps the wt that started it on PATH, which may predate the
    /// guard, so the installed launcher answers when there is one.
    #[test]
    fn the_installed_launcher_answers_before_the_wt_on_path() {
        let tmp = tempdir().unwrap();
        let launcher = tmp.path().join("install/bin/wt");
        fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        executable(
            &launcher,
            &format!("#!/bin/sh\necho refused by the launcher >&2\nexit {GH_GUARD_REFUSED}\n"),
        );
        let create = &["pr", "create"];
        let (code, out, err) = run_gh_shim_with(Some(0), create, "", None, Some(&launcher));
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (1, "", "refused by the launcher\n")
        );
        // No launcher installed there: the wt on PATH is asked instead.
        let missing = tmp.path().join("absent/bin/wt");
        let (code, _, err) = run_gh_shim_with(Some(0), create, "", None, Some(&missing));
        assert_eq!((code, err.as_str()), (7, "guard 1 _gh-guard pr create\n"));
        // A wt from before the guard rejects the subcommand as a usage
        // error: gh runs, and neither that error nor a warning is shown.
        let (code, out, err) = run_gh_shim(Some(2), create);
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (7, "gh [pr] [create]\n", "")
        );
    }

    #[test]
    fn watching_checks_polls_every_thirty_seconds_unless_told_otherwise() {
        let (code, out, err) = run_gh_shim(None, &["pr", "checks", "12", "--watch"]);
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (7, "gh [pr] [checks] [12] [--watch] [--interval] [30]\n", "")
        );
        for untouched in [
            &["pr", "checks", "--watch", "--interval", "5"][..],
            &["pr", "checks", "--watch", "--interval=5"],
            &["pr", "checks", "-i", "5", "--watch"],
            &["pr", "checks", "-i5", "--watch"],
            &["pr", "checks", "12"],
            &["run", "watch", "12"],
            &["run", "view", "--watch"],
        ] {
            let (code, out, _) = run_gh_shim(None, untouched);
            assert_eq!(
                (code, out),
                (7, format!("gh [{}]\n", untouched.join("] [")))
            );
        }
    }

    #[test]
    fn each_gh_call_is_logged_by_session_and_subcommand_without_arguments() {
        let tmp = tempdir().unwrap();
        let log = gh_call_log(tmp.path());
        let call = |args: &[&str]| run_gh_shim_in(tmp.path(), None, args, "", None, None);
        call(&["pr", "view", "12", "--json", "secret body"]);
        call(&["api", "-H", "Authorization: token ghp_Secret1", "graphql"]);
        call(&["api", "graphql", "-f", "query=secret"]);
        call(&["--version"]);
        call(&["-R", "o/r", "pr", "list"]);
        let logged = fs::read_to_string(&log).unwrap();
        let rows: Vec<Vec<&str>> = logged
            .lines()
            .map(|line| line.split('\t').collect())
            .collect();
        assert!(
            rows.iter()
                .all(|row| row.len() == 3 && row[1] == "fixture-slug")
        );
        assert!(
            rows.iter()
                .all(|row| row[0].len() == 20 && row[0].ends_with('Z'))
        );
        assert_eq!(
            rows.iter().map(|row| row[2]).collect::<Vec<_>>(),
            ["pr view", "api", "api", "-", "pr list"]
        );
        assert!(!logged.contains("ecret"), "{logged}");

        // Past the cap the file moves aside once and starts again.
        fs::write(&log, vec![b'x'; GH_CALL_LOG_CAP_BYTES as usize + 1]).unwrap();
        call(&["pr", "list"]);
        assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
        assert_eq!(
            fs::metadata(log.with_extension("log.1")).unwrap().len(),
            GH_CALL_LOG_CAP_BYTES + 1
        );

        // Another call is rotating: this one leaves the log where it is. A
        // lock left behind by a call that died is taken away once the log
        // has reached twice the cap, and the call after that rotates.
        let lock = log.with_extension("log.lock");
        fs::create_dir(&lock).unwrap();
        fs::write(&log, vec![b'y'; GH_CALL_LOG_CAP_BYTES as usize + 1]).unwrap();
        call(&["pr", "list"]);
        assert!(lock.exists());
        assert!(fs::metadata(&log).unwrap().len() > GH_CALL_LOG_CAP_BYTES);
        assert_eq!(fs::read(log.with_extension("log.1")).unwrap()[0], b'x');
        fs::write(&log, vec![b'z'; 2 * GH_CALL_LOG_CAP_BYTES as usize + 1]).unwrap();
        call(&["pr", "list"]);
        assert!(!lock.exists());
        call(&["pr", "list"]);
        assert!(!lock.exists());
        assert_eq!(fs::read_to_string(&log).unwrap().lines().count(), 1);
        assert_eq!(fs::read(log.with_extension("log.1")).unwrap()[0], b'z');

        // A log that cannot be written changes nothing about the call.
        fs::remove_file(&log).unwrap();
        fs::create_dir(&log).unwrap();
        let (code, out, err) = call(&["pr", "view"]);
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (7, "gh [pr] [view]\n", "")
        );
    }

    /// Whether a named repository is the worktree's own is the guard's to
    /// say, so the shim asks whatever `--repo`, `-R` or `GH_REPO` name and
    /// hands them over as given.
    #[test]
    fn a_pull_request_that_names_a_repository_is_still_asked_about() {
        for spelling in [
            &["pr", "create", "--repo", "o/r"][..],
            &["pr", "create", "--repo=o/r", "--draft"],
            &["pr", "create", "-R", "o/r"],
            &["pr", "create", "-Ro/r"],
            &["-R", "o/r", "pr", "create"],
            &["--repo", "o/r", "pr", "create"],
        ] {
            let (code, out, err) = run_gh_shim(Some(GH_GUARD_REFUSED), spelling);
            let asked = format!("guard 1 _gh-guard {}\n", spelling.join(" "));
            assert_eq!((code, out.as_str(), err), (1, "", asked), "{spelling:?}");
        }
        let (code, out, err) = run_gh_shim_with(
            Some(GH_GUARD_REFUSED),
            &["pr", "create"],
            "",
            Some("o/r"),
            None,
        );
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (1, "", "guard 1 _gh-guard pr create GH_REPO=o/r\n")
        );
        // A repository named ahead of the subcommand is not the subcommand.
        let (code, out, _) = run_gh_shim(Some(GH_GUARD_REFUSED), &["-R", "pr", "create", "x"]);
        assert_eq!((code, out.as_str()), (7, "gh [-R] [pr] [create] [x]\n"));
    }

    #[test]
    fn stdin_reaches_gh_and_not_the_guard() {
        let (code, out, _) = run_gh_shim_with(
            Some(0),
            &["pr", "create", "--body-file", "-"],
            "the body\nline two\n",
            None,
            None,
        );
        assert_eq!(
            (code, out.as_str()),
            (
                7,
                "gh [pr] [create] [--body-file] [-]\nthe body\nline two\n"
            )
        );
    }

    #[test]
    fn orphaned_temp_files_are_removed_but_in_flight_ones_survive() {
        let tmp = tempdir().unwrap();
        let bin = tmp.path().join("bin");
        let shims = tmp.path().join("shims");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&shims).unwrap();
        let orphan = shims.join(".bun.99.0.tmp");
        let in_flight = shims.join(".bun.98.0.tmp");
        let other_dot = shims.join(".keep");
        for path in [&orphan, &in_flight, &other_dot] {
            fs::write(path, "partial").unwrap();
        }
        let old = std::time::SystemTime::now() - ORPHANED_TEMP_AGE * 2;
        for path in [&orphan, &other_dot] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        ensure_inspect_shims_at(&shims, &bin.to_string_lossy(), None).unwrap();
        assert!(!orphan.exists());
        assert!(in_flight.exists());
        assert!(other_dot.exists());
    }

    #[test]
    fn unknown_old_harness_shims_are_reported_for_repair() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("shims")).unwrap();
        fs::write(tmp.path().join("shims/claude"), "bad").unwrap();
        assert_eq!(stale_harness_shims(tmp.path()), vec!["claude"]);
    }
}
