use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use wt_platform::process::CommandSpec;

use crate::context::AppContext;

#[derive(Debug, Clone, Args, Default)]
pub struct PerfArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessRow {
    pub pid: u32,
    pub ppid: u32,
    pub cpu: f64,
    pub rss_kb: u64,
    pub elapsed: String,
    pub command: String,
    pub category: &'static str,
    pub downstream: bool,
    pub session: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub sampled_at_ms: u128,
    pub cpu_note: &'static str,
    pub system_cpu: f64,
    pub wt_cpu: f64,
    pub wt_rss_kb: u64,
    pub load_average: Option<[f64; 3]>,
    pub memory: Option<MemorySnapshot>,
    pub process_count: usize,
    pub downstream_count: usize,
    pub other_count: usize,
    pub categories: BTreeMap<&'static str, usize>,
    pub category_totals: BTreeMap<&'static str, (f64, u64)>,
    pub sessions: Vec<SessionSummary>,
    pub orphans: Vec<ProcessRow>,
    pub orphan_probe_available: bool,
    pub tmux_probe_available: bool,
    pub top_downstream: Vec<ProcessRow>,
    pub top_other: Vec<ProcessRow>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MemorySnapshot {
    pub used_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionSummary {
    pub name: String,
    pub cpu: f64,
    pub rss_kb: u64,
    pub count: usize,
    pub summary: String,
}

pub async fn run(ctx: &AppContext, args: &PerfArgs) -> Result<i32> {
    let snapshot = snapshot(ctx, true).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(0);
    }
    println!("wt perf: {} processes", snapshot.process_count);
    if let Some(load) = snapshot.load_average {
        println!("load average: {:.2} {:.2} {:.2}", load[0], load[1], load[2]);
    }
    if let Some(memory) = snapshot.memory {
        println!(
            "memory: {:.1} / {:.1} GiB",
            memory.used_bytes as f64 / 1_073_741_824.0,
            memory.total_bytes as f64 / 1_073_741_824.0
        );
    }
    println!(
        "CPU: {:.0}% total, {:.0}% wt-downstream ({} processes, {:.1} GiB RSS)",
        snapshot.system_cpu,
        snapshot.wt_cpu,
        snapshot.downstream_count,
        snapshot.wt_rss_kb as f64 / 1024.0 / 1024.0
    );
    println!(
        "categories: {}",
        snapshot
            .categories
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect::<Vec<_>>()
            .join("  ")
    );
    for session in &snapshot.sessions {
        println!(
            "session {}: {:.1}% CPU, {:.1} MiB RSS, {} proc(s) — {}",
            session.name,
            session.cpu,
            session.rss_kb as f64 / 1024.0,
            session.count,
            session.summary
        );
    }
    print_processes("wt downstream", &snapshot.top_downstream);
    print_processes("rest of machine", &snapshot.top_other);
    if !snapshot.orphans.is_empty() {
        println!(
            "\nLEAKED: {} headless wt instance(s)",
            snapshot.orphans.len()
        );
        println!(
            "These are launchd-reparented wt app processes not claimed by a com.wt.* job. Verify identity before terminating:"
        );
        println!(
            "  kill {}",
            snapshot
                .orphans
                .iter()
                .map(|row| row.pid.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );
        for row in &snapshot.orphans {
            println!(
                "  pid {}  {:.1}% CPU  {:.1} MiB RSS  up {}  {}",
                row.pid,
                row.cpu,
                row.rss_kb as f64 / 1024.0,
                row.elapsed,
                row.command
            );
        }
    }
    if !snapshot.orphan_probe_available {
        println!(
            "\nheadless wt check unavailable: this platform or launchd probe cannot distinguish a lost terminal from an intentionally supervised daemon, so no kill candidates are reported."
        );
    }
    println!(
        "\nCPU is ps's decaying average (up to about one minute on macOS), not a live profile. RSS is resident memory."
    );
    if !snapshot.tmux_probe_available {
        println!(
            "tmux pane/session attribution unavailable; the process tree still includes the wt process and any discoverable tmux server."
        );
    }
    Ok(0)
}

/// Reusable bounded process snapshot for both `wt perf` and the TUI P overlay.
/// `root_at_wt_instances` is true for the one-shot CLI, whose own parentage
/// cannot include other live wt processes; the overlay roots at its own PID.
pub async fn snapshot(ctx: &AppContext, root_at_wt_instances: bool) -> Result<Snapshot> {
    let mut spec = CommandSpec::new("ps").args(["-Ao", "pid=,ppid=,pcpu=,rss=,etime=,args="]);
    spec.timeout = Duration::from_secs(10);
    spec.output_limit = 4 * 1024 * 1024;
    let output = ctx
        .processes
        .run(spec, &ctx.cancellation)
        .await
        .context("snapshot processes")?;
    let output = output.checked("ps")?;
    let mut rows = output
        .stdout_text()
        .lines()
        .filter_map(parse_process)
        .collect::<Vec<_>>();
    let executable = std::env::current_exe().ok();
    let source = ctx.config.paths.wt_source.as_deref();
    let panes = tmux_panes(ctx).await;
    let server_pid = tmux_server_pid(ctx).await;
    let owned = launchd_owned_pids(ctx).await;
    classify_process_tree(
        &mut rows,
        panes.as_ref().unwrap_or(&HashMap::new()),
        server_pid,
        &executable,
        source,
        root_at_wt_instances,
    );
    classify_orphans(&mut rows, &executable, source, owned.as_ref());
    let mut downstream = rows
        .iter()
        .filter(|row| row.downstream)
        .cloned()
        .collect::<Vec<_>>();
    let mut other = rows
        .iter()
        .filter(|row| !row.downstream)
        .cloned()
        .collect::<Vec<_>>();
    downstream.sort_by(process_weight);
    other.sort_by(process_weight);
    let mut categories = BTreeMap::new();
    let mut category_totals = BTreeMap::new();
    for row in rows.iter().filter(|row| row.downstream) {
        *categories.entry(row.category).or_insert(0) += 1;
        let total = category_totals
            .entry(row.category)
            .or_insert((0.0_f64, 0_u64));
        total.0 += row.cpu;
        total.1 += row.rss_kb;
    }
    let downstream_rows = rows.iter().filter(|row| row.downstream).collect::<Vec<_>>();
    let sessions = summarize_sessions(&downstream_rows);
    let snapshot = Snapshot {
        sampled_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        cpu_note: "ps %CPU is a decaying average (up to about one minute on macOS), not an instantaneous profile",
        system_cpu: rows.iter().map(|row| row.cpu).sum(),
        wt_cpu: downstream.iter().map(|row| row.cpu).sum(),
        wt_rss_kb: downstream.iter().map(|row| row.rss_kb).sum(),
        load_average: load_average(ctx).await,
        memory: memory(ctx).await,
        process_count: rows.len(),
        downstream_count: downstream.len(),
        other_count: other.len(),
        categories,
        category_totals,
        sessions,
        orphans: rows
            .iter()
            .filter(|row| {
                row.pid != std::process::id()
                    && row.ppid == 1
                    && is_wt_identity(row, &executable, source)
                    && owned
                        .as_ref()
                        .is_some_and(|owned| !owned.contains(&row.pid))
            })
            .cloned()
            .collect(),
        orphan_probe_available: owned.is_some(),
        tmux_probe_available: panes.is_some()
            && (server_pid.is_some() || panes.as_ref().is_some_and(HashMap::is_empty)),
        top_downstream: downstream.into_iter().take(12).collect(),
        top_other: other.into_iter().take(6).collect(),
    };
    Ok(snapshot)
}

fn parse_process(line: &str) -> Option<ProcessRow> {
    let mut fields = line.split_whitespace();
    let pid = fields.next()?.parse().ok()?;
    let ppid = fields.next()?.parse().ok()?;
    let cpu = fields.next()?.parse().ok()?;
    let rss_kb = fields.next()?.parse().ok()?;
    let elapsed = fields.next()?.to_owned();
    let command = fields.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return None;
    }
    let category = classify_command(&command);
    Some(ProcessRow {
        pid,
        ppid,
        cpu,
        rss_kb,
        elapsed,
        command,
        category,
        downstream: false,
        session: None,
    })
}

fn classify_command(command: &str) -> &'static str {
    let lower = command.to_ascii_lowercase();
    let argv0 = command.split_whitespace().next().unwrap_or("");
    let name = Path::new(argv0)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let words = lower.split_whitespace().collect::<Vec<_>>();
    let contains = |needle: &str| lower.contains(needle);
    if ["zsh", "bash", "sh", "fish"]
        .iter()
        .any(|shell| name.trim_start_matches('-') == *shell)
    {
        "shell"
    } else if name == "wt"
        || lower.contains("/.wt/bin/")
        || lower.contains("/.wt/src/")
        || (name == "bun" && lower.contains("/src/main.ts"))
        || (words.first() == Some(&"wt")
            && words
                .get(1)
                .is_some_and(|arg| ["ls", "new", "rm", "logs", "doctor", "status"].contains(arg)))
    {
        "wt"
    } else if ["claude", "codex", "opencode"].iter().any(|name| {
        words
            .iter()
            .any(|word| word.trim_matches(['/', '\\']) == *name)
    }) {
        "harness"
    } else if ["vitest", "jest", "playwright"]
        .iter()
        .any(|term| contains(term))
        || contains("deno test")
        || contains("cargo test")
        || contains("nextest")
    {
        "test"
    } else if [
        "tsc", "eslint", "biome", "esbuild", "rollup", "webpack", "swc", "prettier",
    ]
    .iter()
    .any(|term| contains(term))
        || contains("vite build")
        || contains("cargo build")
        || contains("rustc")
    {
        "build"
    } else if contains("vite") || contains("next dev") || contains(".cache/wt/dev/") {
        "dev"
    } else if ["pnpm", "npm", "npx", "yarn", "bun"]
        .iter()
        .any(|runner| words.contains(runner))
    {
        if ["test", "test:edge", "test:unit", "test:watch"]
            .iter()
            .any(|script| words.contains(script))
        {
            "test"
        } else if ["typecheck", "lint", "build", "format"]
            .iter()
            .any(|script| words.contains(script))
        {
            "build"
        } else if ["dev", "preview", "start", "serve"]
            .iter()
            .any(|script| words.contains(script))
        {
            "dev"
        } else {
            "other"
        }
    } else if name == "tmux" || lower.contains("tmux: server") {
        "tmux"
    } else {
        "other"
    }
}

fn process_weight(a: &ProcessRow, b: &ProcessRow) -> std::cmp::Ordering {
    b.cpu
        .total_cmp(&a.cpu)
        .then_with(|| b.rss_kb.cmp(&a.rss_kb))
}

fn print_processes(title: &str, rows: &[ProcessRow]) {
    println!("\n{title} (CPU%, RSS MiB, PID, category, command):");
    for row in rows {
        println!(
            "{:5.1} {:7.1} {:>7} {:10} {}",
            row.cpu,
            row.rss_kb as f64 / 1024.0,
            row.pid,
            row.category,
            row.command
        );
    }
}

fn classify_process_tree(
    rows: &mut [ProcessRow],
    panes: &HashMap<u32, String>,
    server_pid: Option<u32>,
    executable: &Option<PathBuf>,
    source: Option<&Path>,
    root_at_wt_instances: bool,
) {
    let children = child_index(rows);
    for (pane, session) in panes {
        for pid in descendants(&[*pane], &children) {
            if let Some(row) = rows.iter_mut().find(|row| row.pid == pid) {
                row.session.get_or_insert_with(|| session.clone());
            }
        }
    }
    let mut roots = vec![std::process::id()];
    if let Some(server_pid) = server_pid {
        roots.push(server_pid);
    }
    if root_at_wt_instances {
        roots.extend(
            rows.iter()
                .filter(|row| is_wt_identity(row, executable, source))
                .map(|row| row.pid),
        );
    }
    let downstream = descendants(&roots, &children);
    for row in rows {
        row.downstream = downstream.contains(&row.pid);
    }
}

fn classify_orphans(
    rows: &mut [ProcessRow],
    executable: &Option<PathBuf>,
    source: Option<&Path>,
    owned: Option<&HashSet<u32>>,
) {
    for row in rows {
        if row.pid != std::process::id()
            && row.ppid == 1
            && is_wt_identity(row, executable, source)
            && owned.is_some_and(|owned| !owned.contains(&row.pid))
        {
            row.category = "wt";
            row.downstream = false;
        }
    }
}

fn is_wt_identity(row: &ProcessRow, executable: &Option<PathBuf>, source: Option<&Path>) -> bool {
    let argv0 = row.command.split_whitespace().next().unwrap_or("");
    let same_executable = executable.as_ref().is_some_and(|expected| {
        argv0 == expected.to_string_lossy()
            || (Path::new(argv0).is_absolute()
                && Path::new(argv0).file_name() == expected.file_name()
                && Path::new(argv0)
                    .canonicalize()
                    .ok()
                    .zip(expected.canonicalize().ok())
                    .is_some_and(|(actual, expected)| actual == expected))
    });
    let source_main = source.is_some_and(|root| {
        let main = root.join("src/main.ts");
        row.command.contains(&main.to_string_lossy().to_string())
            && (argv0.ends_with("bun") || argv0.ends_with("node") || argv0.ends_with("deno"))
    });
    same_executable || source_main
}

fn child_index(rows: &[ProcessRow]) -> HashMap<u32, Vec<u32>> {
    let mut children = HashMap::new();
    for row in rows {
        children
            .entry(row.ppid)
            .or_insert_with(Vec::new)
            .push(row.pid);
    }
    children
}

fn descendants(roots: &[u32], children: &HashMap<u32, Vec<u32>>) -> HashSet<u32> {
    let mut seen = HashSet::new();
    let mut stack = roots.to_vec();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        stack.extend(children.get(&pid).into_iter().flatten().copied());
    }
    seen
}

fn summarize_sessions(rows: &[&ProcessRow]) -> Vec<SessionSummary> {
    let mut groups: BTreeMap<&str, Vec<&ProcessRow>> = BTreeMap::new();
    for row in rows {
        if let Some(session) = row.session.as_deref() {
            groups.entry(session).or_default().push(row);
        }
    }
    let mut result = groups
        .into_iter()
        .map(|(name, rows)| {
            let cpu = rows.iter().map(|row| row.cpu).sum();
            let rss_kb = rows.iter().map(|row| row.rss_kb).sum();
            let mut categories: BTreeMap<&str, usize> = BTreeMap::new();
            for row in &rows {
                *categories.entry(row.category).or_default() += 1;
            }
            let summary = categories
                .iter()
                .map(|(name, count)| {
                    if *count > 1 {
                        format!("{name}×{count}")
                    } else {
                        name.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" + ");
            SessionSummary {
                name: name.to_owned(),
                cpu,
                rss_kb,
                count: rows.len(),
                summary,
            }
        })
        .collect::<Vec<_>>();
    result.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    result
}

async fn tmux_panes(ctx: &AppContext) -> Option<HashMap<u32, String>> {
    let mut spec = tmux_spec(
        ctx,
        ["list-panes", "-a", "-F", "#{session_name}\t#{pane_pid}"],
    );
    spec.timeout = Duration::from_secs(4);
    spec.output_limit = 256 * 1024;
    let Ok(out) = ctx.processes.run(spec, &ctx.cancellation).await else {
        return None;
    };
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout_text()
            .lines()
            .filter_map(|line| {
                let (session, pid) = line.split_once('\t')?;
                Some((pid.parse().ok()?, session.to_owned()))
            })
            .collect(),
    )
}

async fn tmux_server_pid(ctx: &AppContext) -> Option<u32> {
    let mut spec = tmux_spec(ctx, ["display-message", "-p", "#{pid}"]);
    spec.timeout = Duration::from_secs(4);
    let out = ctx.processes.run(spec, &ctx.cancellation).await.ok()?;
    out.stdout_text().trim().parse().ok()
}

fn tmux_spec<const N: usize>(ctx: &AppContext, args: [&str; N]) -> CommandSpec {
    let mut spec = CommandSpec::new("tmux");
    spec.args = vec!["-L".into(), ctx.config.tmux.socket.clone().into()];
    spec.args.extend(args.into_iter().map(Into::into));
    spec.cwd = Some(ctx.home.clone());
    spec
}

async fn launchd_owned_pids(ctx: &AppContext) -> Option<HashSet<u32>> {
    #[cfg(target_os = "macos")]
    {
        let mut spec = CommandSpec::new("launchctl").args(["list"]);
        spec.timeout = Duration::from_secs(5);
        spec.output_limit = 1024 * 1024;
        let Ok(out) = ctx.processes.run(spec, &ctx.cancellation).await else {
            return None;
        };
        if !out.status.success() {
            return None;
        }
        Some(
            out.stdout_text()
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split('\t');
                    let pid = fields.next()?.parse::<u32>().ok()?;
                    let _status = fields.next()?;
                    let label = fields.next()?;
                    label.starts_with("com.wt.").then_some(pid)
                })
                .collect(),
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = ctx;
        None
    }
}

async fn load_average(ctx: &AppContext) -> Option<[f64; 3]> {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = ctx
            .processes
            .run(
                CommandSpec::new("sysctl").args(["-n", "vm.loadavg"]),
                &ctx.cancellation,
            )
            .await
            && out.status.success()
        {
            let text = out.stdout_text();
            let mut values = text
                .trim()
                .trim_matches(['{', '}'])
                .split_whitespace()
                .filter_map(|v| v.parse().ok());
            if let (Some(a), Some(b), Some(c)) = (values.next(), values.next(), values.next()) {
                return Some([a, b, c]);
            }
        }
    }
    let values = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut fields = values.split_whitespace();
    Some([
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
    ])
}

async fn memory(ctx: &AppContext) -> Option<MemorySnapshot> {
    #[cfg(target_os = "macos")]
    {
        let total = ctx
            .processes
            .run(
                CommandSpec::new("sysctl").args(["-n", "hw.memsize"]),
                &ctx.cancellation,
            )
            .await
            .ok()?
            .stdout_text()
            .trim()
            .parse::<u64>()
            .ok()?;
        let stats = ctx
            .processes
            .run(CommandSpec::new("vm_stat"), &ctx.cancellation)
            .await
            .ok()?
            .stdout_text();
        let page = stats
            .lines()
            .next()?
            .split("page size of ")
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?;
        let pages = [
            "Pages active:",
            "Pages wired down:",
            "Pages occupied by compressor:",
        ]
        .iter()
        .filter_map(|name| {
            stats
                .lines()
                .find(|line| line.starts_with(name))
                .and_then(|line| line.split_whitespace().last())
                .and_then(|value| {
                    value
                        .trim_end_matches('.')
                        .replace('.', "")
                        .parse::<u64>()
                        .ok()
                })
        })
        .sum::<u64>();
        return Some(MemorySnapshot {
            used_bytes: pages.saturating_mul(page),
            total_bytes: total,
        });
    }
    #[cfg(target_os = "linux")]
    {
        let stats = std::fs::read_to_string("/proc/meminfo").ok()?;
        let value = |name: &str| {
            stats
                .lines()
                .find(|line| line.starts_with(name))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|n| n.parse::<u64>().ok())
                .map(|kb| kb * 1024)
        };
        let total = value("MemTotal:")?;
        let available = value("MemAvailable:")?;
        return Some(MemorySnapshot {
            used_bytes: total.saturating_sub(available),
            total_bytes: total,
        });
    }
    #[allow(unreachable_code)]
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_parser_handles_commands_and_classifies_harnesses() {
        let row = parse_process("123 2 14.5 1024 00:42 codex --resume abc").unwrap();
        assert_eq!(row.category, "harness");
        assert_eq!(row.rss_kb, 1024);
        assert!(parse_process("garbage").is_none());
    }

    #[test]
    fn process_tree_scopes_children_to_wt_and_leaves_unrelated_processes_outside() {
        let executable = PathBuf::from("/opt/wt");
        let mut rows = [
            parse_process("10 1 1.0 100 00:01 /opt/wt --help").unwrap(),
            parse_process("11 10 5.0 200 00:02 codex --resume x").unwrap(),
            parse_process("12 1 2.0 300 00:03 cargo build").unwrap(),
        ];
        classify_process_tree(
            &mut rows,
            &HashMap::new(),
            None,
            &Some(executable),
            None,
            true,
        );
        assert!(rows[0].downstream && rows[1].downstream);
        assert!(!rows[2].downstream);
        assert_eq!(rows[1].category, "harness");
    }

    #[test]
    fn session_ownership_follows_tmux_pane_descendants_and_orphans_fail_closed() {
        let executable = Some(PathBuf::from("/opt/wt"));
        let mut rows = [
            parse_process("10 1 0 100 00:01 /opt/wt").unwrap(),
            parse_process("20 10 1 200 00:02 tmux: server").unwrap(),
            parse_process("30 20 2 300 00:03 /bin/zsh").unwrap(),
            parse_process("31 30 90 400 00:04 codex --resume abc").unwrap(),
            parse_process("40 1 1 100 00:05 /opt/wt").unwrap(),
            parse_process("41 1 1 100 00:05 /opt/wt events serve").unwrap(),
            parse_process("50 1 1 100 00:05 /tmp/unrelated/wt").unwrap(),
        ];
        classify_process_tree(
            &mut rows,
            &HashMap::from([(30, "one".to_owned())]),
            Some(20),
            &executable,
            None,
            false,
        );
        let owned = HashSet::from([41]);
        classify_orphans(&mut rows, &executable, None, Some(&owned));
        assert!(rows[3].downstream);
        assert_eq!(rows[3].session.as_deref(), Some("one"));
        let orphan_pids = rows
            .iter()
            .filter(|row| row.ppid == 1 && is_wt_identity(row, &executable, None) && row.pid != 10)
            .filter(|row| row.pid != 41)
            .map(|row| row.pid)
            .collect::<Vec<_>>();
        assert_eq!(orphan_pids, [40]);
        assert!(!rows[6].downstream);
    }
}
