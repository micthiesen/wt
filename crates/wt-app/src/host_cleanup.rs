//! Best-effort cleanup for resources owned by one worktree.
//!
//! Removal calls `before_remove` only after the worker has revalidated its
//! immutable target and removal revision. Browser state is closed separately
//! after the checkout is actually gone.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process,
    time::Duration,
};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use wt_platform::process::CommandSpec;
use wt_tmux::{TmuxClient, TmuxServer};

use crate::context::AppContext;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RemovalCleanup {
    pub stopped_sessions: Vec<String>,
    pub reaped_listeners: Vec<u32>,
    pub warnings: Vec<String>,
}

pub async fn before_remove(
    context: &AppContext,
    slug: &str,
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<RemovalCleanup> {
    let mut report = RemovalCleanup::default();
    let tmux = TmuxClient::new(
        context.processes.clone(),
        TmuxServer::named(context.config.tmux.socket.clone()).with_cwd(&context.home),
    );
    match tmux.list_sessions(cancellation).await {
        Ok(sessions) => {
            for session in sessions
                .into_iter()
                .filter(|session| bare_slug(&session.name) == slug)
            {
                // Names alone are ambiguous: foo-codex may be foo's Codex
                // pane or the primary pane of a distinct foo-codex checkout.
                let panes = match tmux.list_panes(&session.name, cancellation).await {
                    Ok(panes) => panes,
                    Err(error) => {
                        report.warnings.push(format!(
                            "could not verify tmux session {} ownership: {error}",
                            session.name
                        ));
                        continue;
                    }
                };
                if !panes.iter().any(|pane| {
                    session_owned_by_target(slug, &session.name, &pane.current_path, path)
                }) {
                    continue;
                }
                match tmux.kill_session_id(&session.id, cancellation).await {
                    Ok(true) => report.stopped_sessions.push(session.name),
                    Ok(false) => {}
                    Err(error) => report.warnings.push(format!(
                        "could not stop tmux session {}: {error}",
                        session.name
                    )),
                }
            }
        }
        Err(error) => report.warnings.push(format!(
            "could not inspect tmux sessions for {slug}: {error}"
        )),
    }
    match crate::actions::service(context)?
        .kill(slug, cancellation)
        .await
    {
        Ok(true) => report.stopped_sessions.push(format!("{slug} action")),
        Ok(false) => {}
        Err(error) => report
            .warnings
            .push(format!("could not stop action for {slug}: {error}")),
    }
    match reap_worktree_listeners(context, path, cancellation).await {
        Ok(pids) => report.reaped_listeners = pids,
        Err(error) if !cancellation.is_cancelled() => report.warnings.push(format!(
            "could not reap listeners under {}: {error:#}",
            path.display()
        )),
        Err(error) => return Err(error),
    }
    Ok(report)
}

pub async fn after_remove(context: &AppContext, slug: &str, dev_port: Option<u16>) -> Vec<String> {
    let mut warnings = Vec::new();
    let sessions = browser_sessions(context, slug, dev_port).await;
    for id in sessions {
        match run(
            context,
            command(
                "browser-control",
                ["session", "delete", id.as_str()],
                Duration::from_secs(10),
            ),
            &context.cancellation,
        )
        .await
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => warnings.push(format!(
                "browser session {id} cleanup exited {:?}: {}",
                output.status.code(),
                output.stderr_text().trim()
            )),
            Err(error) => warnings.push(format!("browser session {id} cleanup failed: {error}")),
        }
    }
    if let Some(port) = dev_port {
        warnings.extend(close_tabs_on_port(context, port).await);
    }
    warnings
}

pub async fn after_dev_stop(context: &AppContext, slug: &str, port: Option<u16>) -> Vec<String> {
    let Some(port) = port else { return Vec::new() };
    let mut warnings = Vec::new();
    for id in browser_sessions(context, slug, Some(port)).await {
        match run(
            context,
            command(
                "browser-control",
                ["session", "delete", id.as_str()],
                Duration::from_secs(10),
            ),
            &context.cancellation,
        )
        .await
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => warnings.push(format!(
                "browser session {id} cleanup exited {:?}: {}",
                output.status.code(),
                output.stderr_text().trim()
            )),
            Err(error) => warnings.push(format!("browser session {id} cleanup failed: {error}")),
        }
    }
    warnings.extend(close_tabs_on_port(context, port).await);
    warnings
}

pub async fn stored_dev_port(context: &AppContext, slug: &str) -> Option<u16> {
    let slug = slug.to_owned();
    context
        .database
        .call(move |store| {
            let state = store.read_wt_state()?;
            Ok(state
                .pointer(&format!("/slugs/{slug}/devPort"))
                .and_then(Value::as_u64)
                .and_then(|port| u16::try_from(port).ok()))
        })
        .await
        .ok()
        .flatten()
}

fn bare_slug(name: &str) -> &str {
    let name = name.rsplit_once('~').map_or(name, |(base, _)| base);
    ["-opencode", "-codex", "-action", "-shell", "-diff", "-dev"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name)
}

fn session_owned_by_target(slug: &str, name: &str, pane_cwd: &Path, checkout: &Path) -> bool {
    bare_slug(name) == slug && path_is_within(pane_cwd, checkout)
}

async fn reap_worktree_listeners(
    context: &AppContext,
    path: &Path,
    cancel: &CancellationToken,
) -> Result<Vec<u32>> {
    let canonical = tokio::fs::canonicalize(path)
        .await
        .context("canonicalize checkout for listener cleanup")?;
    let main = tokio::fs::canonicalize(&context.config.paths.main_clone)
        .await
        .unwrap_or_else(|_| context.config.paths.main_clone.clone());
    if canonical == main || canonical.components().count() < 3 {
        return Ok(Vec::new());
    }
    let listeners = run(
        context,
        command(
            "lsof",
            ["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"],
            Duration::from_secs(5),
        ),
        cancel,
    )
    .await?;
    if !listeners.status.success() && listeners.stdout.is_empty() {
        return Ok(Vec::new());
    }
    let entries = parse_listener_records(&listeners.stdout_text());
    let mut pids = entries
        .iter()
        .map(|entry| entry.0)
        .filter(|pid| *pid != process::id() && *pid != unsafe { libc::getppid() as u32 })
        .collect::<Vec<_>>();
    pids.sort_unstable();
    pids.dedup();
    if pids.is_empty() {
        return Ok(Vec::new());
    }
    let csv = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let cwd = run(
        context,
        command(
            "lsof",
            ["-a", "-p", csv.as_str(), "-d", "cwd", "-Fn"],
            Duration::from_secs(5),
        ),
        cancel,
    )
    .await?;
    if !cwd.status.success() && cwd.stdout.is_empty() {
        return Ok(Vec::new());
    }
    let cwd_map = parse_cwd_records(&cwd.stdout_text());
    let mine = entries
        .into_iter()
        .filter(|(pid, _, _)| {
            *pid != process::id()
                && *pid != unsafe { libc::getppid() as u32 }
                && cwd_map
                    .get(pid)
                    .is_some_and(|cwd| path_is_within(cwd, &canonical))
        })
        .map(|entry| entry.0)
        .collect::<Vec<_>>();
    for pid in &mine {
        unsafe {
            libc::kill(*pid as i32, libc::SIGTERM);
        }
    }
    wait_dead(&mine, Duration::from_secs(2)).await;
    for pid in &mine {
        if process_alive(*pid) {
            unsafe {
                libc::kill(*pid as i32, libc::SIGKILL);
            }
        }
    }
    wait_dead(&mine, Duration::from_secs(2)).await;
    Ok(mine
        .into_iter()
        .filter(|pid| !process_alive(*pid))
        .collect())
}

async fn browser_sessions(context: &AppContext, slug: &str, port: Option<u16>) -> Vec<String> {
    let Ok(output) = run(
        context,
        command(
            "browser-control",
            ["status", "--json"],
            Duration::from_secs(5),
        ),
        &context.cancellation,
    )
    .await
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Vec::new();
    };
    if value.pointer("/relay/running").and_then(Value::as_bool) != Some(true) {
        return Vec::new();
    }
    value
        .pointer("/extension/sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|session| {
            let id = session.get("id")?.as_str()?;
            let named = id == format!("wt-{slug}");
            let on_port = port.is_some_and(|port| {
                session
                    .get("pageUrl")
                    .and_then(Value::as_str)
                    .is_some_and(|url| url_on_loopback_port(url, port))
            });
            (named || on_port).then(|| id.to_owned())
        })
        .collect()
}

async fn close_tabs_on_port(context: &AppContext, port: u16) -> Vec<String> {
    let Ok(output) = run(
        context,
        command("ps", ["-Aco", "command"], Duration::from_secs(5)),
        &context.cancellation,
    )
    .await
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    const APPS: &[&str] = &[
        "Brave Browser",
        "Google Chrome",
        "Google Chrome Canary",
        "Google Chrome Beta",
        "Google Chrome Dev",
        "Chromium",
        "Microsoft Edge",
        "Vivaldi",
        "Opera",
        "Arc",
    ];
    let command_list = output.stdout_text();
    let running = command_list
        .lines()
        .map(str::trim)
        .collect::<std::collections::BTreeSet<_>>();
    let mut warnings = Vec::new();
    for app in APPS.iter().copied().filter(|app| running.contains(app)) {
        let escaped_app = applescript_quote(app);
        let script = format!(
            "tell application {escaped_app}\n set out to \"\"\n repeat with w in windows\n repeat with t in tabs of w\n set out to out & (URL of t) & linefeed\n end repeat\n end repeat\n return out\nend tell"
        );
        let mut list_spec = command("osascript", ["-"], Duration::from_secs(15));
        list_spec.input = Some(script.into_bytes());
        let Ok(list) = run(context, list_spec, &context.cancellation).await else {
            continue;
        };
        if !list.status.success() {
            continue;
        }
        let urls = list
            .stdout_text()
            .lines()
            .filter(|url| url_on_loopback_port(url, port))
            .map(applescript_quote)
            .collect::<Vec<_>>();
        if urls.is_empty() {
            continue;
        }
        let script = format!(
            "set doomed to {{{}}}\nset closedCount to 0\ntell application {escaped_app}\n repeat with wi from (count of windows) to 1 by -1\n set w to window wi\n repeat with i from (count of tabs of w) to 1 by -1\n set u to (URL of tab i of w) as text\n if doomed contains u then\n close tab i of w\n set closedCount to closedCount + 1\n end if\n end repeat\n end repeat\nend tell\nreturn closedCount",
            urls.join(", ")
        );
        let mut close_spec = command("osascript", ["-"], Duration::from_secs(5));
        close_spec.input = Some(script.into_bytes());
        match run(context, close_spec, &context.cancellation).await {
            Ok(result) if result.status.success() => {}
            Ok(result) => {
                let stderr = result.stderr_text();
                if !stderr.trim().is_empty() {
                    warnings.push(format!(
                        "could not close {app} tabs on port {port}: {}",
                        stderr.trim()
                    ));
                }
            }
            Err(error) => warnings.push(format!(
                "could not close {app} tabs on port {port}: {error}"
            )),
        }
    }
    warnings
}

async fn run(
    context: &AppContext,
    spec: CommandSpec,
    cancel: &CancellationToken,
) -> Result<wt_platform::process::ProcessOutput> {
    context
        .processes
        .run(spec, cancel)
        .await
        .context("run host cleanup command")
}

fn command<const N: usize>(program: &str, args: [&str; N], timeout: Duration) -> CommandSpec {
    let mut spec = CommandSpec::new(program).args(args);
    spec.timeout = timeout;
    spec
}

fn parse_listener_records(output: &str) -> Vec<(u32, String, Vec<String>)> {
    let mut records = Vec::new();
    let mut pid = None;
    let mut command = String::new();
    let mut names = Vec::new();
    let flush = |records: &mut Vec<(u32, String, Vec<String>)>,
                 pid: &mut Option<u32>,
                 command: &mut String,
                 names: &mut Vec<String>| {
        if let Some(pid) = pid.take() {
            records.push((pid, std::mem::take(command), std::mem::take(names)));
        }
    };
    for line in output.lines() {
        let Some((kind, value)) = line.split_at_checked(1) else {
            continue;
        };
        match kind {
            "p" => {
                flush(&mut records, &mut pid, &mut command, &mut names);
                pid = value.parse().ok();
            }
            "c" => command = value.into(),
            "n" => names.push(value.into()),
            _ => {}
        }
    }
    flush(&mut records, &mut pid, &mut command, &mut names);
    records
}

fn parse_cwd_records(output: &str) -> BTreeMap<u32, PathBuf> {
    let mut result = BTreeMap::new();
    let mut pid = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = value.parse().ok();
        } else if let Some(value) = line.strip_prefix('n')
            && let Some(pid) = pid
        {
            result.insert(pid, PathBuf::from(value));
        }
    }
    result
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}
fn process_alive(pid: u32) -> bool {
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}
async fn wait_dead(pids: &[u32], limit: Duration) {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline && pids.iter().any(|pid| process_alive(*pid)) {
        sleep(Duration::from_millis(100)).await;
    }
}
fn applescript_quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}
fn url_on_loopback_port(text: &str, port: u16) -> bool {
    let Some((scheme, rest)) = text.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let valid = ["localhost", "127.0.0.1", "[::1]"];
    valid
        .iter()
        .any(|host| host_port == format!("{host}:{port}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_match_only_selects_candidates_and_cwd_proves_ownership() {
        for name in [
            "eng-1",
            "eng-1-codex",
            "eng-1-opencode",
            "eng-1-diff",
            "eng-1-shell",
            "eng-1~review",
            "eng-1~review-codex",
        ] {
            assert_eq!(bare_slug(name), "eng-1", "{name}");
        }
        for name in ["eng-10", "eng-1x-codex", "other-eng-1", "eng-1-manager"] {
            assert_ne!(bare_slug(name), "eng-1", "{name}");
        }
        let foo = Path::new("/worktrees/foo");
        assert!(path_is_within(Path::new("/worktrees/foo/src"), foo));
        assert!(!path_is_within(Path::new("/worktrees/foo-codex"), foo));
        assert!(session_owned_by_target(
            "foo",
            "foo-codex",
            Path::new("/worktrees/foo"),
            foo
        ));
        assert!(!session_owned_by_target(
            "foo",
            "foo-codex",
            Path::new("/worktrees/foo-codex"),
            foo
        ));
    }

    #[test]
    fn listener_and_cwd_records_are_grouped_without_prefix_path_confusion() {
        let listeners =
            parse_listener_records("p123\ncnode\nn127.0.0.1:3000\np124\ncpython\nn*:4000\n");
        assert_eq!(listeners.len(), 2);
        assert_eq!(
            listeners[0],
            (123, "node".into(), vec!["127.0.0.1:3000".into()])
        );
        let cwd =
            parse_cwd_records("p123\nn/Users/me/worktrees/a\np124\nn/Users/me/worktrees/ab\n");
        let root = Path::new("/Users/me/worktrees/a");
        assert!(path_is_within(&cwd[&123], root));
        assert!(!path_is_within(&cwd[&124], root));
    }

    #[test]
    fn loopback_port_match_parses_authority_not_arbitrary_url_text() {
        assert!(url_on_loopback_port("http://localhost:8123/path", 8123));
        assert!(url_on_loopback_port("https://127.0.0.1:8123", 8123));
        assert!(url_on_loopback_port("http://[::1]:8123", 8123));
        assert!(!url_on_loopback_port("http://localhost:81230", 8123));
        assert!(!url_on_loopback_port(
            "https://evil.test/?next=http://localhost:8123",
            8123
        ));
    }
}
