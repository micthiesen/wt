use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;
use wt_config::{NamingConfig, NamingHarness, NamingReasoningEffort};
use wt_core::HarnessId;

use crate::AiSummary;

/// Injectable executable names keep command construction testable without
/// invoking a model or depending on the user's installed harnesses.
#[derive(Clone, Debug)]
pub struct HarnessPrograms {
    pub claude: OsString,
    pub codex: OsString,
    pub opencode: OsString,
}

impl Default for HarnessPrograms {
    fn default() -> Self {
        Self {
            claude: "claude".into(),
            codex: "codex".into(),
            opencode: "opencode".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompletionSpec {
    pub harness_id: HarnessId,
    pub program: OsString,
    pub args: Vec<OsString>,
    pub input: Option<Vec<u8>>,
    pub cwd: PathBuf,
    pub timeout: Duration,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HarnessCompletionError {
    #[error("naming harness {0:?} is not supported")]
    UnsupportedHarness(NamingHarness),
    #[error("naming configuration field {field} must be finite and between {min} and {max}")]
    InvalidNumber {
        field: &'static str,
        min: u64,
        max: u64,
    },
}

/// Purely builds the safe one-shot invocation. This function never reads
/// process environment or probes executable availability.
pub fn build_completion_spec(
    naming: &NamingConfig,
    primary: HarnessId,
    prompt: &str,
    cwd: impl AsRef<Path>,
    programs: &HarnessPrograms,
) -> Result<CompletionSpec, HarnessCompletionError> {
    let harness_id = match naming.harness {
        NamingHarness::Primary => primary,
        NamingHarness::Claude => HarnessId::Claude,
        NamingHarness::Codex => HarnessId::Codex,
        NamingHarness::Opencode => HarnessId::Opencode,
    };
    let model = naming.models.get(harness_id.as_str());
    let mut args: Vec<OsString> = Vec::new();
    let mut input = None;
    let program = match harness_id {
        HarnessId::Claude => {
            args.extend(
                [
                    "-p",
                    "--safe-mode",
                    "--permission-mode",
                    "dontAsk",
                    "--tools",
                    "",
                    "--disable-slash-commands",
                    "--no-session-persistence",
                    "--output-format",
                    "text",
                ]
                .into_iter()
                .map(Into::into),
            );
            if let Some(model) = model.filter(|m| !m.is_empty()) {
                args.extend([OsString::from("--model"), model.into()]);
            }
            args.extend([
                OsString::from("--effort"),
                claude_effort(naming.reasoning_effort).into(),
            ]);
            input = Some(prompt.as_bytes().to_vec());
            programs.claude.clone()
        }
        HarnessId::Codex => {
            args.extend(
                [
                    "exec",
                    "--ephemeral",
                    "--sandbox",
                    "read-only",
                    "--ignore-rules",
                    "--skip-git-repo-check",
                    "--color",
                    "never",
                    "-C",
                ]
                .into_iter()
                .map(Into::into),
            );
            args.push(cwd.as_ref().as_os_str().to_owned());
            if let Some(model) = model.filter(|m| !m.is_empty()) {
                args.extend([OsString::from("--model"), model.into()]);
            }
            args.extend([
                OsString::from("--config"),
                format!(
                    "model_reasoning_effort=\"{}\"",
                    effort_name(naming.reasoning_effort)
                )
                .into(),
                OsString::from("-"),
            ]);
            input = Some(prompt.as_bytes().to_vec());
            programs.codex.clone()
        }
        HarnessId::Opencode => {
            args.extend(
                ["run", "--pure", "--format", "default", "--dir"]
                    .into_iter()
                    .map(Into::into),
            );
            args.push(cwd.as_ref().as_os_str().to_owned());
            if let Some(model) = model.filter(|m| !m.is_empty()) {
                args.extend([OsString::from("--model"), model.into()]);
            }
            args.extend([
                OsString::from("--variant"),
                effort_name(naming.reasoning_effort).into(),
                OsString::from("--"),
                prompt.into(),
            ]);
            programs.opencode.clone()
        }
    };
    let timeout_ms = bounded_number("timeout_ms", naming.timeout_ms, 1, 600_000)?;
    bounded_number("max_input_tokens", naming.max_input_tokens, 1, 100_000)?;
    Ok(CompletionSpec {
        harness_id,
        program,
        args,
        input,
        cwd: cwd.as_ref().to_path_buf(),
        timeout: Duration::from_millis(timeout_ms),
    })
}

fn bounded_number(
    field: &'static str,
    value: f64,
    min: u64,
    max: u64,
) -> Result<u64, HarnessCompletionError> {
    if !value.is_finite() || value < min as f64 || value > max as f64 {
        return Err(HarnessCompletionError::InvalidNumber { field, min, max });
    }
    Ok(value.round() as u64)
}

fn effort_name(effort: NamingReasoningEffort) -> &'static str {
    match effort {
        NamingReasoningEffort::Minimal => "minimal",
        NamingReasoningEffort::Low => "low",
        NamingReasoningEffort::Medium => "medium",
        NamingReasoningEffort::High => "high",
        NamingReasoningEffort::Xhigh => "xhigh",
        NamingReasoningEffort::Max => "max",
    }
}

fn claude_effort(effort: NamingReasoningEffort) -> &'static str {
    match effort {
        NamingReasoningEffort::Minimal => "low",
        other => effort_name(other),
    }
}

/// Parse a naming response with the legacy marker/fallback rules.
pub fn parse_title_description(text: &str) -> AiSummary {
    let trimmed = text.trim();
    let title_match = marker_line(trimmed, "TITLE:");
    let brief_match = marker_line(trimmed, "BRIEF:");
    let desc_marker = marker_line(trimmed, "DESCRIPTION:");
    let title = title_match
        .as_ref()
        .map(|(_, value)| clean_inline(value))
        .filter(|s| !s.is_empty());
    let description = if let Some((end, value)) = desc_marker {
        let tail = trimmed[end..].trim();
        if value.is_empty() {
            tail.to_string()
        } else if tail.is_empty() {
            value.to_string()
        } else {
            format!("{}\n{tail}", value.trim())
        }
    } else {
        let end = [
            title_match.as_ref().map(|(end, _)| *end),
            brief_match.as_ref().map(|(end, _)| *end),
        ]
        .into_iter()
        .flatten()
        .max();
        end.map(|end| trimmed[end..].trim().to_string())
            .unwrap_or_else(|| trimmed.to_string())
    };
    AiSummary { title, description }
}

fn marker_line<'a>(text: &'a str, marker: &str) -> Option<(usize, &'a str)> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let without_newline = line.trim_end_matches(['\n', '\r']);
        if let Some(value) = without_newline.strip_prefix(marker) {
            let end = offset + line.len();
            return Some((end.min(text.len()), value.trim()));
        }
        offset += line.len();
    }
    None
}

fn clean_inline(value: &str) -> String {
    value
        .trim()
        .trim_matches(['"', '\'', '`'])
        .trim()
        .trim_end_matches('.')
        .trim()
        .to_string()
}

const META_WORDS: &[&str] = &[
    "tui",
    "stack",
    "stacks",
    "branch",
    "branches",
    "section",
    "sections",
    "header",
    "headers",
    "group",
    "groups",
    "grouping",
    "developer",
    "tool",
    "tools",
    "feature",
    "features",
    "subsystem",
    "subsystems",
    "area",
    "areas",
];

pub fn is_stack_title_meta_only(title: &str) -> bool {
    let words: Vec<String> = title
        .to_lowercase()
        .split_whitespace()
        .map(|word| word.chars().filter(char::is_ascii_alphabetic).collect())
        .filter(|word: &String| !word.is_empty())
        .collect();
    !words.is_empty() && words.iter().all(|word| META_WORDS.contains(&word.as_str()))
}

/// Extracts, cleans and caps the title from a stack response. Caller decides
/// whether a meta-only title should trigger the single retry.
pub fn parse_stack_title(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let raw = marker_line(trimmed, "TITLE:")
        .map(|(_, value)| value)
        .unwrap_or(trimmed);
    let words: Vec<_> = clean_inline(raw)
        .split_whitespace()
        .take(6)
        .map(str::to_owned)
        .collect();
    let title = words.join(" ");
    (!title.is_empty()).then_some(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(harness: NamingHarness) -> NamingConfig {
        NamingConfig {
            harness,
            ..NamingConfig::default()
        }
    }

    #[test]
    fn builds_isolated_harness_invocations() {
        let programs = HarnessPrograms {
            claude: "fake-claude".into(),
            codex: "fake-codex".into(),
            opencode: "fake-opencode".into(),
        };
        let cwd = Path::new("/repo with spaces");
        let claude = build_completion_spec(
            &config(NamingHarness::Claude),
            HarnessId::Codex,
            "p",
            cwd,
            &programs,
        )
        .unwrap();
        assert_eq!(claude.program, "fake-claude");
        assert!(
            claude
                .args
                .iter()
                .any(|arg| arg == "--no-session-persistence")
        );
        assert_eq!(claude.input.as_deref(), Some(b"p".as_slice()));
        let codex = build_completion_spec(
            &config(NamingHarness::Codex),
            HarnessId::Claude,
            "p",
            cwd,
            &programs,
        )
        .unwrap();
        assert_eq!(
            codex.args.iter().position(|arg| arg == "/repo with spaces"),
            Some(9)
        );
        assert!(codex.args.iter().any(|arg| arg == "--sandbox"));
        let open = build_completion_spec(
            &config(NamingHarness::Opencode),
            HarnessId::Claude,
            "safe prompt",
            cwd,
            &programs,
        )
        .unwrap();
        assert_eq!(open.args.last().unwrap(), "safe prompt");
        assert!(open.args.iter().any(|arg| arg == "--pure"));
    }

    #[test]
    fn parses_markers_fallback_and_stack_titles() {
        let parsed = parse_title_description(
            "TITLE: \"Fix concise summary.\"\nBRIEF: ignored\nDESCRIPTION:\nDetails",
        );
        assert_eq!(parsed.title.as_deref(), Some("Fix concise summary"));
        assert_eq!(parsed.description, "Details");
        assert_eq!(
            parse_title_description("plain response").description,
            "plain response"
        );
        assert_eq!(
            parse_stack_title("TITLE: `Improve board filters.`"),
            Some("Improve board filters".into())
        );
        assert_eq!(
            parse_stack_title("Too many words in this stack title"),
            Some("Too many words in this stack".into())
        );
        assert!(is_stack_title_meta_only("Header Stack Section"));
        assert!(!is_stack_title_meta_only("Header Stamp"));
    }

    #[test]
    fn rejects_invalid_numeric_configuration() {
        let mut cfg = config(NamingHarness::Claude);
        cfg.timeout_ms = f64::NAN;
        assert!(matches!(
            build_completion_spec(
                &cfg,
                HarnessId::Claude,
                "",
                ".",
                &HarnessPrograms::default()
            ),
            Err(HarnessCompletionError::InvalidNumber {
                field: "timeout_ms",
                ..
            })
        ));
        cfg.timeout_ms = 1.0;
        cfg.max_input_tokens = 0.0;
        assert!(matches!(
            build_completion_spec(
                &cfg,
                HarnessId::Claude,
                "",
                ".",
                &HarnessPrograms::default()
            ),
            Err(HarnessCompletionError::InvalidNumber {
                field: "max_input_tokens",
                ..
            })
        ));
    }
}
