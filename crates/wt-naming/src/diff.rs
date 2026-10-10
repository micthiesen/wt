use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessError, ProcessRunner};

use crate::cache_key::short_hash;

const EXCLUDES: &[&str] = &[
    ":!package-lock.json",
    ":!yarn.lock",
    ":!pnpm-lock.yaml",
    ":!bun.lock",
    ":!bun.lockb",
    ":!Cargo.lock",
    ":!*.min.js",
    ":!*.min.css",
    ":!*.map",
    ":!*.snap",
    ":!dist/*",
    ":!build/*",
    ":!*.generated.*",
    ":!*.g.dart",
    ":!*_generated.go",
    ":!*.pb.go",
    ":!*.pb.ts",
    ":!*.d.ts",
    ":!migrations/*.sql",
];
const CHARS_PER_TOKEN: f64 = 3.5;
const SCAFFOLD_CHARS: usize = 500;
const GIT_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileMode {
    Full,
    Tight,
    Hunks,
    Dropped,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeCounts {
    pub full: usize,
    pub tight: usize,
    pub hunks: usize,
    pub dropped: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffContext {
    pub hash: String,
    pub base: String,
    pub prompt: String,
    pub counts: ModeCounts,
    pub files_total: usize,
}

#[derive(Debug, Error)]
pub enum DiffContextError {
    #[error("diff context base is invalid: {0}")]
    InvalidBase(String),
    #[error("max_input_tokens must be finite and between 1 and 100000, got {0}")]
    InvalidBudget(f64),
    #[error("{operation}: {source}")]
    Process {
        operation: &'static str,
        #[source]
        source: ProcessError,
    },
    #[error("{operation}: git returned an error (exit {code:?}): {stderr}")]
    Git {
        operation: &'static str,
        code: Option<i32>,
        stderr: String,
    },
}

/// Collect committed-only changes against `base` and reduce them to a
/// deterministic prompt under the configured token estimate. Uncommitted
/// edits are intentionally excluded, matching the historical AI summary.
pub async fn build_diff_context(
    repo: impl AsRef<Path>,
    base: &str,
    max_input_tokens: f64,
    runner: &ProcessRunner,
    cancellation: &CancellationToken,
) -> Result<Option<DiffContext>, DiffContextError> {
    if base.is_empty() || base.starts_with('-') || base.contains('\0') || base.contains('\n') {
        return Err(DiffContextError::InvalidBase(base.to_string()));
    }
    if !max_input_tokens.is_finite() || !(1.0..=100_000.0).contains(&max_input_tokens) {
        return Err(DiffContextError::InvalidBudget(max_input_tokens));
    }
    let repo = repo.as_ref();
    let stat = run_git(
        runner,
        cancellation,
        repo,
        "diff stat",
        vec![
            "-c",
            "core.quotePath=true",
            "diff",
            "--stat",
            &format!("{base}...HEAD"),
            "--",
        ]
        .into_iter()
        .chain(EXCLUDES.iter().copied())
        .map(str::to_owned)
        .collect(),
        Duration::from_secs(10),
    )
    .await?;
    let stat = stat.trim().to_string();
    if stat.is_empty() {
        return Ok(None);
    }
    let log = run_git(
        runner,
        cancellation,
        repo,
        "commit log",
        vec!["log", "--reverse", "--format=%s", &format!("{base}..HEAD")]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        Duration::from_secs(5),
    )
    .await?;
    let log = log.trim().to_string();
    let raw_diff = run_git(
        runner,
        cancellation,
        repo,
        "full diff",
        vec![
            "-c",
            "core.quotePath=true",
            "diff",
            "-U3",
            "-W",
            "--diff-algorithm=patience",
            "--ignore-space-change",
            "--ignore-blank-lines",
            &format!("{base}...HEAD"),
            "--",
        ]
        .into_iter()
        .chain(EXCLUDES.iter().copied())
        .map(str::to_owned)
        .collect(),
        Duration::from_secs(15),
    )
    .await?;
    if stat.is_empty() && log.is_empty() && raw_diff.trim().is_empty() {
        return Ok(None);
    }

    let hash = short_hash(format!("{base}\n{stat}\n{raw_diff}").as_bytes());
    let mut parts = parse_diff_parts(&raw_diff);
    let files_total = parts.len();
    let total_budget = (max_input_tokens * CHARS_PER_TOKEN).floor().max(1.0) as usize;
    let header_chars = utf16_len(&stat)
        .saturating_add(utf16_len(&log))
        .saturating_add(SCAFFOLD_CHARS);
    let file_budget = 1000usize.max(total_budget.saturating_sub(header_chars));
    let counts = compact_diff(&mut parts, file_budget);
    let rendered = parts.iter().filter_map(render).collect::<String>();
    let mut sections = vec![format!("File summary:\n{stat}")];
    if !log.is_empty() {
        sections.push(format!("Commit messages (oldest first):\n{log}"));
    }
    if !rendered.is_empty() {
        sections.push(format!("Detailed changes:\n{rendered}"));
    }
    let note = format_compaction(counts);
    if !note.is_empty() {
        sections.push(format!("(compaction: {note})"));
    }
    let prompt = sections.join("\n\n");
    let prompt = if utf16_len(&prompt) > total_budget {
        format!(
            "{}\n\n(prompt truncated)",
            truncate_utf16(&prompt, total_budget)
        )
    } else {
        prompt
    };
    Ok(Some(DiffContext {
        hash,
        base: base.to_string(),
        prompt,
        counts,
        files_total,
    }))
}

async fn run_git(
    runner: &ProcessRunner,
    cancellation: &CancellationToken,
    repo: &Path,
    operation: &'static str,
    args: Vec<String>,
    timeout: Duration,
) -> Result<String, DiffContextError> {
    let mut spec = CommandSpec::new("git").args(args).cwd(PathBuf::from(repo));
    spec.timeout = timeout;
    spec.output_limit = GIT_OUTPUT_LIMIT;
    let output = runner
        .run(spec, cancellation)
        .await
        .map_err(|source| DiffContextError::Process { operation, source })?;
    if !output.status.success() {
        return Err(DiffContextError::Git {
            operation,
            code: output.status.code(),
            stderr: output.stderr_text(),
        });
    }
    Ok(output.stdout_text())
}

pub fn parse_diff_parts(diff: &str) -> Vec<(String, String, usize, usize, FileMode)> {
    let mut parts = Vec::new();
    let mut start = None;
    for (offset, line) in line_offsets(diff) {
        if line.starts_with("diff --git ")
            && let Some(prev) = start.replace(offset)
        {
            push_part(diff, prev, offset, &mut parts);
        }
    }
    if let Some(start) = start {
        push_part(diff, start, diff.len(), &mut parts);
    }
    parts
}

fn push_part(
    diff: &str,
    start: usize,
    end: usize,
    out: &mut Vec<(String, String, usize, usize, FileMode)>,
) {
    let raw = &diff[start..end];
    let Some(path) = parse_diff_path(raw.lines().next().unwrap_or_default()) else {
        return;
    };
    let mut adds = 0;
    let mut dels = 0;
    let mut in_hunk = false;
    for line in raw.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line.starts_with('+') {
            adds += 1;
        } else if line.starts_with('-') {
            dels += 1;
        }
    }
    out.push((path, raw.to_string(), adds, dels, FileMode::Full));
}

fn line_offsets(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    text.split_inclusive('\n').map(move |line| {
        let at = offset;
        offset += line.len();
        (at, line.trim_end_matches(['\n', '\r']))
    })
}

fn parse_diff_path(header: &str) -> Option<String> {
    let body = header.strip_prefix("diff --git ")?;
    let new = if body.starts_with('"') {
        let separator = body.find("\" \"")? + 2;
        &body[separator..]
    } else {
        let separator = body.rfind(" b/")? + 1;
        &body[separator..]
    };
    let decoded = decode_git_path(new)?;
    Some(decoded.strip_prefix("b/").unwrap_or(&decoded).to_string())
}

fn decode_git_path(input: &str) -> Option<String> {
    if !input.starts_with('"') {
        return Some(input.to_string());
    }
    let bytes = input.as_bytes();
    if bytes.len() < 2 || *bytes.last()? != b'"' {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i + 1 < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        let esc = *bytes.get(i)?;
        match esc {
            b't' => out.push(b'\t'),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b'\\' => out.push(b'\\'),
            b'"' => out.push(b'"'),
            b'0'..=b'7' => {
                let mut value = 0u16;
                let mut count = 0;
                while count < 3
                    && i + count < bytes.len() - 1
                    && (b'0'..=b'7').contains(&bytes[i + count])
                {
                    value = value * 8 + (bytes[i + count] - b'0') as u16;
                    count += 1;
                }
                out.push(value as u8);
                i += count - 1;
            }
            _ => return None,
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

pub fn compact_diff(
    parts: &mut [(String, String, usize, usize, FileMode)],
    budget: usize,
) -> ModeCounts {
    loop {
        let total: usize = parts.iter().map(|p| utf16_len(&render_tuple(p))).sum();
        if total <= budget {
            break;
        }
        let mut target: Option<(usize, usize, usize)> = None;
        for (i, part) in parts.iter().enumerate() {
            let tier = priority(&part.0);
            let size = utf16_len(&render_tuple(part));
            if part.4 == FileMode::Dropped {
                continue;
            }
            if target.is_none_or(|(_, old_tier, old_size)| {
                tier > old_tier || (tier == old_tier && size > old_size)
            }) {
                target = Some((i, tier, size));
            }
        }
        let Some((i, _, _)) = target else {
            break;
        };
        parts[i].4 = match parts[i].4 {
            FileMode::Full => FileMode::Tight,
            FileMode::Tight => FileMode::Hunks,
            FileMode::Hunks | FileMode::Dropped => FileMode::Dropped,
        };
    }
    let mut counts = ModeCounts::default();
    for part in parts {
        match part.4 {
            FileMode::Full => counts.full += 1,
            FileMode::Tight => counts.tight += 1,
            FileMode::Hunks => counts.hunks += 1,
            FileMode::Dropped => counts.dropped += 1,
        }
    }
    counts
}

fn render_tuple(part: &(String, String, usize, usize, FileMode)) -> String {
    if part.4 == FileMode::Dropped {
        return String::new();
    }
    let lines = part.1.lines();
    let mut out = Vec::new();
    for line in lines {
        match part.4 {
            FileMode::Full => out.push(line),
            FileMode::Tight if !line.starts_with(' ') => out.push(line),
            FileMode::Hunks
                if line.starts_with("diff --git")
                    || line.starts_with("index ")
                    || line.starts_with("--- ")
                    || line.starts_with("+++ ")
                    || line.starts_with("@@")
                    || line.starts_with("similarity index")
                    || line.starts_with("rename ")
                    || line.starts_with("new file")
                    || line.starts_with("deleted file")
                    || line.starts_with("copy from")
                    || line.starts_with("copy to")
                    || line.starts_with("Binary files") =>
            {
                out.push(line)
            }
            _ => {}
        }
    }
    let mut rendered = out.join("\n");
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
}

fn render(part: &(String, String, usize, usize, FileMode)) -> Option<String> {
    (part.4 != FileMode::Dropped).then(|| render_tuple(part))
}

fn priority(path: &str) -> usize {
    let lower = path.to_ascii_lowercase();
    if [".test.", ".spec.", ".stories.", ".e2e."]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return 2;
    }
    if [
        ".github/",
        ".circleci/",
        ".gitlab/",
        ".husky/",
        ".vscode/",
        ".idea/",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
    {
        return 3;
    }
    if [
        ".json",
        ".yaml",
        ".yml",
        ".toml",
        ".env",
        ".conf",
        ".cfg",
        ".ini",
        ".lock",
        ".properties",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
    {
        return 3;
    }
    if [
        ".sql", ".graphql", ".gql", ".prisma", ".proto", ".xsd", ".wsdl", ".md", ".mdx", ".rst",
        ".txt", ".adoc",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
    {
        return 2;
    }
    if [
        ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".py", ".go", ".rs", ".java", ".kt", ".kts",
        ".swift", ".c", ".cc", ".cpp", ".h", ".hh", ".hpp", ".hxx", ".rb", ".php", ".cs", ".fs",
        ".fsx", ".scala", ".elm", ".ex", ".exs", ".erl", ".clj", ".cljs", ".m", ".mm", ".vue",
        ".svelte", ".astro", ".sh", ".bash", ".zsh", ".fish", ".lua", ".dart", ".nim", ".zig",
        ".hs", ".ml", ".mli", ".r", ".jl",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
    {
        return 1;
    }
    2
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn truncate_utf16(s: &str, max: usize) -> String {
    let mut units = 0;
    s.chars()
        .take_while(|ch| {
            units += ch.len_utf16();
            units <= max
        })
        .collect()
}

fn format_compaction(counts: ModeCounts) -> String {
    let total = counts.full + counts.tight + counts.hunks + counts.dropped;
    if total == 0 || counts.full == total {
        return String::new();
    }
    [
        (counts.full, "full"),
        (counts.tight, "tight"),
        (counts.hunks, "hunks-only"),
        (counts.dropped, "dropped"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, name)| format!("{n} {name}"))
    .collect::<Vec<_>>()
    .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn parses_quoted_paths_and_counts_only_hunk_lines() {
        let diff = "diff --git \"a/a\\040space.rs\" \"b/a\\040space.rs\"\nindex 1..2 100644\n--- \"a/a\\040space.rs\"\n+++ \"b/a\\040space.rs\"\n@@ -1 +1 @@\n-old\n+new\n";
        let parts = parse_diff_parts(diff);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].0, "a space.rs");
        assert_eq!((parts[0].2, parts[0].3), (1, 1));
    }

    #[test]
    fn compaction_sacrifices_config_before_source_and_counts_modes() {
        let mut parts = vec![
            (
                "src/main.rs".into(),
                format!("diff --git a/a b/a\n@@ x @@\n{}\n", "+source\n".repeat(30)),
                30,
                0,
                FileMode::Full,
            ),
            (
                ".github/workflows/a.yml".into(),
                format!("diff --git a/b b/b\n@@ x @@\n{}\n", "+config\n".repeat(30)),
                30,
                0,
                FileMode::Full,
            ),
        ];
        let counts = compact_diff(&mut parts, 400);
        assert_eq!(parts[0].4, FileMode::Full);
        assert!(parts[1].4 != FileMode::Full);
        assert_eq!(
            counts.full + counts.tight + counts.hunks + counts.dropped,
            2
        );
    }

    #[tokio::test]
    async fn collects_committed_changes_against_explicit_base_and_ignores_uncommitted_edits() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("base.rs"), "fn base() {}\n").unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-m", "initial"]);
        git(repo, &["checkout", "-b", "topic"]);
        std::fs::write(
            repo.join("space name.rs"),
            "fn changed() {\n    let answer = 42;\n}\n",
        )
        .unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-m", "Add named function"]);
        std::fs::write(repo.join("uncommitted.rs"), "must not enter summary\n").unwrap();
        let runner = ProcessRunner::new(NonZeroUsize::new(2).unwrap());
        let result = build_diff_context(repo, "main", 8000.0, &runner, &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.files_total, 1);
        assert!(result.prompt.contains("space name.rs"), "{}", result.prompt);
        assert!(result.prompt.contains("let answer = 42"));
        assert!(result.prompt.contains("Add named function"));
        assert!(!result.prompt.contains("uncommitted.rs"));
        assert_eq!(result.hash.len(), 16);
    }
}
