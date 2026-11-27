use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const NEVER_SHIM: &[&str] = &["claude", "codex", "opencode", "pi"];
const ALWAYS_SHIM: &[&str] = &["bun"];
const LAUNCHER_SHIM: &[&str] = &["pnpm", "pnpx", "npx", "yarn"];
/// The guard's exit code for "do not create this pull request".
pub const GH_GUARD_REFUSED: i32 = 75;
const MIN_COMPILED_BYTES: u64 = 8 * 1024 * 1024;
const HEAD_BYTES: usize = 64 * 1024;
/// A shim write finishes in well under this; an older temp file is orphaned.
const ORPHANED_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// Best-effort machine-local BUN_INSPECT shields. Failure to generate the
/// shims must not prevent a Claude session from starting.
pub fn ensure_inspect_shims(cache_dir: &Path, path_value: &str) {
    let dir = cache_dir.join("shims");
    if ensure_inspect_shims_at(&dir, path_value).is_err() { /* advisory by contract */ }
}

pub fn ensure_inspect_shims_at(dir: &Path, path_value: &str) -> std::io::Result<()> {
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
        let body = gh_shim_body(&real, dir);
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
    let dir = cache_dir.join("shims");
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
/// says so in one line and lets `gh` run.
fn gh_shim_body(real: &str, shim_dir: &Path) -> String {
    let real = sh_quote(real);
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
for arg in "$@"; do
  case $arg in
    -*) ;;
    *)
      if [ -z "$first" ]; then first=$arg; else second=$arg; break; fi
      ;;
  esac
done
if [ "$first" = pr ] && [ "$second" = create ] && [ -z "$WT_GH_GUARD" ]; then
  if wt=$(command -v wt); then
    WT_GH_GUARD=1 WT_REAL_GH=$real "$wt" _gh-guard "$@"
    code=$?
    [ "$code" -eq {refused} ] && exit 1
    [ "$code" -ne 0 ] && echo "wt shim: could not check this worktree's pull request (wt exit $code); running gh unchecked" >&2
  else
    echo "wt shim: wt is not on PATH; running gh unchecked" >&2
  fi
fi
exec "$real" "$@"
"#,
        refused = GH_GUARD_REFUSED,
    )
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
        ensure_inspect_shims_at(&shims, &bin.to_string_lossy()).unwrap();
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
        ensure_inspect_shims_at(&shims, &bin.to_string_lossy()).unwrap();
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

    /// Runs the generated gh shim with fakes for gh and wt on PATH. The fake
    /// gh prints its arguments and exits 7; the fake wt exits `guard`.
    fn run_gh_shim(guard: Option<i32>, args: &[&str]) -> (i32, String, String) {
        let tmp = tempdir().unwrap();
        let bin = tmp.path().join("bin");
        let shims = tmp.path().join("shims");
        fs::create_dir_all(&bin).unwrap();
        executable(
            &bin.join("gh"),
            "#!/bin/sh\nprintf 'gh'; for a in \"$@\"; do printf ' [%s]' \"$a\"; done; echo\nexit 7\n",
        );
        if let Some(code) = guard {
            executable(
                &bin.join("wt"),
                &format!(
                    "#!/bin/sh\necho \"guard $WT_GH_GUARD $*\" >&2\n[ -x \"$WT_REAL_GH\" ] || exit 99\nexit {code}\n"
                ),
            );
        }
        let path = format!("{}:/usr/bin:/bin", bin.display());
        ensure_inspect_shims_at(&shims, &path).unwrap();
        let output = std::process::Command::new(shims.join("gh"))
            .args(args)
            .env("PATH", format!("{}:{path}", shims.display()))
            .env_remove("WT_GH_GUARD")
            .output()
            .unwrap();
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
        ensure_inspect_shims_at(&shims, &bin.to_string_lossy()).unwrap();
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
