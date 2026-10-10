use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use wt_platform::process::CommandSpec;
use wt_tui::{PerfGroupView, PerfProcessView, PerfTone, PerfView};

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
    /// Logical CPUs; the CPU ceiling is `cores * 100`.
    pub cores: usize,
    /// One-line answer to "is this us?", as the overlay and report show it.
    pub verdict: String,
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
    for line in report(&snapshot) {
        println!("{line}");
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
    let own_pid = std::process::id();
    let mut rows = output
        .stdout_text()
        .lines()
        .filter_map(parse_process)
        .collect::<Vec<_>>();
    drop_own_probe(&mut rows, own_pid);
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
        own_pid,
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
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let system_cpu: f64 = rows.iter().map(|row| row.cpu).sum();
    let wt_cpu: f64 = downstream.iter().map(|row| row.cpu).sum();
    let snapshot = Snapshot {
        sampled_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        cpu_note: CPU_NOTE,
        cores,
        verdict: verdict(system_cpu, wt_cpu, cores).1,
        system_cpu,
        wt_cpu,
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
                row.pid != own_pid
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

const CPU_NOTE: &str =
    "ps %CPU is a decaying average (up to about one minute on macOS), not an instantaneous profile";

/// Below this share of the machine's CPU ceiling, whose load it is does not
/// matter: the machine is not busy.
const BUSY_AT: f64 = 0.25;

/// Remove the `ps` child that took this sample; it is the sampler, not load.
fn drop_own_probe(rows: &mut Vec<ProcessRow>, own_pid: u32) {
    rows.retain(|row| {
        let argv0 = row.command.split_whitespace().next().unwrap_or("");
        !(row.ppid == own_pid
            && Path::new(argv0)
                .file_name()
                .is_some_and(|name| name == "ps"))
    });
}

/// Human label for a process category key.
pub fn category_label(category: &str) -> &'static str {
    match category {
        "harness" => "agents",
        "test" => "tests",
        "build" => "typecheck/lint",
        "dev" => "dev servers",
        "wt" => "wt itself",
        "tmux" => "tmux",
        "shell" => "shells",
        _ => "other",
    }
}

/// The one-line answer to "is this us?". `system_cpu` sums every process's
/// %CPU, so the share is "of the work happening now, how much is ours", not
/// a share of installed capacity, which reads small on a many-core machine
/// even when wt owns all of it.
pub fn verdict(system_cpu: f64, wt_cpu: f64, cores: usize) -> (PerfTone, String) {
    let ceiling = cores.max(1) as f64 * 100.0;
    if system_cpu / ceiling < BUSY_AT {
        return (
            PerfTone::Calm,
            format!(
                "machine is not busy: {} of {} in use, so whatever feels slow is probably not CPU",
                percent(system_cpu),
                percent(ceiling)
            ),
        );
    }
    let share = if system_cpu <= 0.0 {
        0.0
    } else {
        wt_cpu / system_cpu
    };
    let figures = format!(
        "{} of the {} in use ({})",
        percent(wt_cpu),
        percent(system_cpu),
        percent(share * 100.0)
    );
    if share >= 0.5 {
        (PerfTone::Ours, format!("wt is most of the load: {figures}"))
    } else {
        (
            PerfTone::Elsewhere,
            format!("wt is not most of the load: {figures}"),
        )
    }
}

fn percent(value: f64) -> String {
    format!("{value:.0}%")
}

/// `48 MiB` under a gigabyte, `2.3 GiB` above, so small trees never read as
/// `0.0 GiB`.
pub fn format_memory(kb: u64) -> String {
    let mib = kb as f64 / 1024.0;
    if mib >= 1000.0 {
        format!("{:.1} GiB", mib / 1024.0)
    } else {
        format!("{mib:.0} MiB")
    }
}

/// The command with the directory of an absolute argv[0] removed. A macOS
/// app bundle path may contain spaces, so it is cut after `Contents/MacOS/`.
pub fn short_command(command: &str) -> String {
    const BUNDLE: &str = "/Contents/MacOS/";
    if command.starts_with('/')
        && let Some(at) = command.find(BUNDLE)
    {
        return command[at + BUNDLE.len()..].to_owned();
    }
    let argv0 = command.split_whitespace().next().unwrap_or("");
    match argv0.rsplit_once('/') {
        Some((_, name)) if argv0.starts_with('/') && !name.is_empty() => {
            format!("{name}{}", &command[argv0.len()..])
        }
        _ => command.to_owned(),
    }
}

/// Local wall-clock time of the sample, `2026-10-10 14:03:22`. Uses libc
/// timezone lookup, so call it only from a CLI or source task.
fn sampled_at_label(sampled_at_ms: u128) -> String {
    let millis = u64::try_from(sampled_at_ms).unwrap_or(u64::MAX);
    if let Some((year, month, day, hour, minute, second)) =
        crate::display_time::local_time_components(millis)
    {
        return format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}");
    }
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .ok()
        .and_then(|time| {
            time.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| "unknown time".into())
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}es")
    }
}

/// Plain-text report shared by `wt perf` and the overlay's `i` handoff.
/// Section headings end with `:` and entries are indented.
pub fn report(snapshot: &Snapshot) -> Vec<String> {
    let ceiling = snapshot.cores.max(1) as f64 * 100.0;
    let mut lines = vec![
        format!(
            "wt perf snapshot, sampled {}",
            sampled_at_label(snapshot.sampled_at_ms)
        ),
        format!("Verdict: {}", snapshot.verdict),
    ];
    let mut machine = format!("Machine: {} cores", snapshot.cores);
    if let Some(memory) = &snapshot.memory {
        machine.push_str(&format!(
            ", memory {} of {} in use",
            format_memory(memory.used_bytes / 1024),
            format_memory(memory.total_bytes / 1024)
        ));
    }
    if let Some(load) = snapshot.load_average {
        machine.push_str(&format!(
            ", load {:.2} {:.2} {:.2} (1m/5m/15m)",
            load[0], load[1], load[2]
        ));
    }
    lines.push(machine);
    lines.push(format!(
        "CPU: {} of {} machine-wide; {} of that is wt-downstream ({}, {} RSS)",
        percent(snapshot.system_cpu),
        percent(ceiling),
        percent(snapshot.wt_cpu),
        plural(snapshot.downstream_count, "process"),
        format_memory(snapshot.wt_rss_kb)
    ));
    lines.push(format!(
        "Note: {CPU_NOTE}. Process memory is RSS, not the physical footprint."
    ));
    lines.push(String::new());
    lines.push("wt downstream by category:".into());
    let categories = category_groups(snapshot);
    if categories.is_empty() {
        lines.push("  nothing running downstream of wt".into());
    }
    for group in &categories {
        lines.push(format!(
            "  {}: {}, {}, {}",
            group.label,
            percent(group.cpu),
            format_memory(group.rss_kb),
            plural(group.count, "process")
        ));
    }
    if !snapshot.sessions.is_empty() {
        lines.push("By tmux session (worktree):".into());
        for session in &snapshot.sessions {
            lines.push(format!(
                "  {}: {}, {}, {}: {}",
                session.name,
                percent(session.cpu),
                format_memory(session.rss_kb),
                plural(session.count, "process"),
                session.summary
            ));
        }
    }
    lines.push("Heaviest processes downstream of wt:".into());
    lines.extend(snapshot.top_downstream.iter().map(report_process));
    lines.push("Heaviest processes not downstream of wt:".into());
    lines.extend(snapshot.top_other.iter().map(report_process));
    if !snapshot.orphans.is_empty() {
        lines.push(format!(
            "LEAKED: {} headless wt instance(s):",
            snapshot.orphans.len()
        ));
        lines.push(
            "  launchd-reparented wt processes not claimed by a com.wt.* job; verify identity before running:".into(),
        );
        lines.push(format!(
            "  kill {}",
            snapshot
                .orphans
                .iter()
                .map(|row| row.pid.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        ));
        lines.extend(snapshot.orphans.iter().map(report_process));
    }
    if !snapshot.orphan_probe_available {
        lines.push(
            "Leaked-instance check unavailable: this platform cannot tell a lost terminal from a supervised daemon.".into(),
        );
    }
    if !snapshot.tmux_probe_available {
        lines.push("tmux session attribution unavailable.".into());
    }
    lines
}

fn report_process(row: &ProcessRow) -> String {
    let session = row
        .session
        .as_deref()
        .map(|name| format!(" [{name}]"))
        .unwrap_or_default();
    format!(
        "  {} {} pid {} [{}]{} up {}: {}",
        percent(row.cpu),
        format_memory(row.rss_kb),
        row.pid,
        category_label(row.category),
        session,
        row.elapsed,
        clip(&short_command(&row.command), 100)
    )
}

/// At most `max` characters, marked with an ellipsis when cut.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max.saturating_sub(1)) {
        Some((at, _)) if text.chars().count() > max => format!("{}…", &text[..at]),
        _ => text.to_owned(),
    }
}

/// Downstream categories, heaviest first.
fn category_groups(snapshot: &Snapshot) -> Vec<PerfGroupView> {
    let mut groups = snapshot
        .category_totals
        .iter()
        .map(|(category, (cpu, rss_kb))| PerfGroupView {
            label: category_label(category).into(),
            cpu: *cpu,
            rss_kb: *rss_kb,
            count: snapshot
                .categories
                .get(category)
                .copied()
                .unwrap_or_default(),
            summary: String::new(),
        })
        .collect::<Vec<_>>();
    groups.sort_by(|a, b| {
        b.cpu
            .total_cmp(&a.cpu)
            .then_with(|| b.rss_kb.cmp(&a.rss_kb))
    });
    groups
}

/// Typed overlay model of a snapshot. Off the render thread: it formats the
/// local sample time.
pub fn view(snapshot: &Snapshot) -> PerfView {
    let process = |row: &ProcessRow| PerfProcessView {
        pid: row.pid,
        cpu: row.cpu,
        rss_kb: row.rss_kb,
        elapsed: row.elapsed.clone(),
        command: short_command(&row.command),
        session: row.session.clone(),
    };
    let label = sampled_at_label(snapshot.sampled_at_ms);
    PerfView {
        sampled_at: label.rsplit(' ').next().unwrap_or(&label).to_owned(),
        verdict: snapshot.verdict.clone(),
        verdict_tone: verdict(snapshot.system_cpu, snapshot.wt_cpu, snapshot.cores).0,
        cores: u32::try_from(snapshot.cores).unwrap_or(u32::MAX),
        system_cpu: snapshot.system_cpu,
        wt_cpu: snapshot.wt_cpu,
        wt_rss_kb: snapshot.wt_rss_kb,
        downstream_count: snapshot.downstream_count,
        load_average: snapshot.load_average,
        memory_used_bytes: snapshot.memory.as_ref().map(|memory| memory.used_bytes),
        memory_total_bytes: snapshot.memory.as_ref().map(|memory| memory.total_bytes),
        categories: category_groups(snapshot),
        sessions: snapshot
            .sessions
            .iter()
            .map(|session| PerfGroupView {
                label: session.name.clone(),
                cpu: session.cpu,
                rss_kb: session.rss_kb,
                count: session.count,
                summary: session.summary.clone(),
            })
            .collect(),
        top_downstream: snapshot.top_downstream.iter().map(process).collect(),
        top_other: snapshot.top_other.iter().map(process).collect(),
        orphans: snapshot.orphans.iter().map(process).collect(),
        orphan_probe_available: snapshot.orphan_probe_available,
        tmux_probe_available: snapshot.tmux_probe_available,
        error: None,
    }
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
            .any(|word| word.rsplit('/').next() == Some(*name))
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

fn classify_process_tree(
    rows: &mut [ProcessRow],
    panes: &HashMap<u32, String>,
    server_pid: Option<u32>,
    executable: &Option<PathBuf>,
    source: Option<&Path>,
    root_at_wt_instances: bool,
    own_pid: u32,
) {
    let children = child_index(rows);
    for (pane, session) in panes {
        for pid in descendants(&[*pane], &children) {
            if let Some(row) = rows.iter_mut().find(|row| row.pid == pid) {
                row.session.get_or_insert_with(|| session.clone());
            }
        }
    }
    let mut roots = vec![own_pid];
    if let Some(server_pid) = server_pid {
        roots.push(server_pid);
    }
    if root_at_wt_instances {
        // A TUI launched as bare `wt` through PATH has no absolute argv[0]
        // to prove its identity; rooting at a lookalike only widens "us".
        roots.extend(
            rows.iter()
                .filter(|row| {
                    is_wt_identity(row, executable, source)
                        || (row.ppid != 1 && row.category == "wt")
                })
                .map(|row| row.pid),
        );
    }
    let downstream = descendants(&roots, &children);
    for row in rows {
        row.downstream = downstream.contains(&row.pid);
        if row.pid == own_pid {
            row.category = "wt";
        }
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
                *categories.entry(category_label(row.category)).or_default() += 1;
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
    #[cfg(not(target_os = "macos"))]
    let _ = ctx;
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
    #[cfg(not(target_os = "macos"))]
    let _ = ctx;
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
            999_999,
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
            10,
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

    #[test]
    fn own_process_counts_as_wt_and_the_sampling_ps_is_dropped() {
        let mut rows = vec![
            parse_process("10 1 3.0 49152 00:01 wt").unwrap(),
            parse_process("11 10 0.0 900 00:00 ps -Ao pid=,ppid=,pcpu=,rss=,etime=,args=").unwrap(),
            parse_process("12 1 0.0 900 00:00 ps aux").unwrap(),
            parse_process("13 10 0.0 900 00:00 /usr/libexec/mystery").unwrap(),
        ];
        drop_own_probe(&mut rows, 10);
        assert_eq!(
            rows.iter().map(|row| row.pid).collect::<Vec<_>>(),
            [10, 12, 13]
        );
        classify_process_tree(&mut rows, &HashMap::new(), None, &None, None, false, 13);
        assert_eq!(rows[2].category, "wt");
        assert!(rows[2].downstream);
    }

    #[test]
    fn bare_wt_tui_roots_the_cli_tree_but_orphans_do_not() {
        let mut rows = [
            parse_process("10 5 3.0 100 00:01 wt").unwrap(),
            parse_process("11 10 50.0 200 00:01 codex").unwrap(),
            parse_process("20 1 1.0 100 00:01 wt").unwrap(),
            parse_process("21 20 9.0 100 00:01 vitest").unwrap(),
        ];
        classify_process_tree(&mut rows, &HashMap::new(), None, &None, None, true, 999_999);
        assert!(rows[0].downstream && rows[1].downstream);
        assert!(!rows[2].downstream && !rows[3].downstream);
    }

    #[test]
    fn memory_units_follow_magnitude() {
        assert_eq!(format_memory(48 * 1024), "48 MiB");
        assert_eq!(format_memory(0), "0 MiB");
        assert_eq!(format_memory(999 * 1024), "999 MiB");
        assert_eq!(format_memory(2_411_725), "2.3 GiB");
    }

    #[test]
    fn verdict_names_calm_machines_then_whose_load_it_is() {
        let (tone, text) = verdict(200.0, 150.0, 12);
        assert_eq!(tone, PerfTone::Calm);
        assert_eq!(
            text,
            "machine is not busy: 200% of 1200% in use, so whatever feels slow is probably not CPU"
        );
        let (tone, text) = verdict(800.0, 600.0, 12);
        assert_eq!(tone, PerfTone::Ours);
        assert_eq!(
            text,
            "wt is most of the load: 600% of the 800% in use (75%)"
        );
        let (tone, text) = verdict(800.0, 80.0, 12);
        assert_eq!(tone, PerfTone::Elsewhere);
        assert_eq!(
            text,
            "wt is not most of the load: 80% of the 800% in use (10%)"
        );
        assert_eq!(verdict(50.0, 0.0, 0).0, PerfTone::Elsewhere);
    }

    #[test]
    fn categories_cover_harness_runners_and_shell_wrappers() {
        for (command, category) in [
            ("/bin/zsh -c /x/.wt/src/main.ts", "shell"),
            ("-zsh", "shell"),
            ("/usr/local/bin/wt", "wt"),
            ("node /x/claude --resume", "harness"),
            ("pnpm test:unit", "test"),
            ("cargo build --release", "build"),
            ("pnpm dev", "dev"),
            ("tmux -L wt new-session", "tmux"),
            ("/usr/sbin/mDNSResponder", "other"),
        ] {
            assert_eq!(classify_command(command), category, "{command}");
        }
        assert_eq!(category_label("harness"), "agents");
        assert_eq!(category_label("nonsense"), "other");
    }

    #[test]
    fn short_command_drops_only_an_absolute_argv0_directory() {
        assert_eq!(
            short_command("/opt/homebrew/bin/node a/b.js"),
            "node a/b.js"
        );
        assert_eq!(short_command("node /a/b.js"), "node /a/b.js");
        assert_eq!(short_command("/usr/bin/top"), "top");
        assert_eq!(
            short_command("/Applications/A B.app/Contents/MacOS/A B --type=gpu"),
            "A B --type=gpu"
        );
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abcd", 4), "abcd");
    }

    #[test]
    fn report_and_view_share_units_labels_and_verdict() {
        let mut snapshot = Snapshot {
            sampled_at_ms: 0,
            cpu_note: CPU_NOTE,
            cores: 4,
            verdict: verdict(10.0, 4.0, 4).1,
            system_cpu: 10.0,
            wt_cpu: 4.0,
            wt_rss_kb: 48 * 1024,
            load_average: Some([1.0, 2.0, 3.0]),
            memory: None,
            process_count: 3,
            downstream_count: 2,
            other_count: 1,
            categories: BTreeMap::from([("harness", 1), ("wt", 1)]),
            category_totals: BTreeMap::from([("harness", (1.0, 1024)), ("wt", (3.0, 2048))]),
            sessions: Vec::new(),
            orphans: Vec::new(),
            orphan_probe_available: true,
            tmux_probe_available: true,
            top_downstream: vec![parse_process("10 1 3.0 2048 00:01 /opt/wt").unwrap()],
            top_other: Vec::new(),
        };
        snapshot.top_downstream[0].category = "wt";
        let report = report(&snapshot).join("\n");
        assert!(report.contains("Verdict: machine is not busy"), "{report}");
        assert!(report.contains("2 processes, 48 MiB RSS"), "{report}");
        assert!(
            report.contains("  wt itself: 3%, 2 MiB, 1 process"),
            "{report}"
        );
        assert!(
            report.contains("pid 10 [wt itself] up 00:01: wt"),
            "{report}"
        );
        assert!(!report.contains("UTC") && !report.contains('\u{2014}'));
        let view = view(&snapshot);
        assert_eq!(view.verdict_tone, PerfTone::Calm);
        assert_eq!(
            view.categories
                .iter()
                .map(|group| group.label.as_str())
                .collect::<Vec<_>>(),
            ["wt itself", "agents"]
        );
        assert_eq!(view.top_downstream[0].command, "wt");
        assert_eq!(view.sampled_at.len(), 8, "{}", view.sampled_at);
    }
}
