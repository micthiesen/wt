use crate::time::parse_iso_millis;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const WORK_STATES: [WorkState; 8] = [
    WorkState::Todo,
    WorkState::Working,
    WorkState::Review,
    WorkState::NeedsTesting,
    WorkState::NeedsHuman,
    WorkState::Ready,
    WorkState::Verified,
    WorkState::Dropped,
];
pub const WORK_RISKS: [WorkRisk; 3] = [WorkRisk::Low, WorkRisk::Medium, WorkRisk::High];
pub const GATED_TODO_RANK: u8 = 8;
pub const BLOCKED_RANK: u8 = 5;
pub const NO_STATUS_RANK: u8 = 6;
pub const LANDED_RANK: u8 = 10;
pub const VERIFY_OVERDUE_DAYS: i64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkState {
    Todo,
    Working,
    Review,
    NeedsTesting,
    NeedsHuman,
    Ready,
    Verified,
    Dropped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkRisk {
    Low,
    Medium,
    High,
}

/// Persisted work status. Unknown keys are retained for forward-compatible
/// read/modify/write operations; timestamps and author do not change claim identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkStatusRecord {
    pub state: WorkState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk: Option<WorkRisk>,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_on: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_after_merge: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl WorkStatusRecord {
    pub fn new(state: WorkState, at: impl Into<String>) -> Self {
        Self {
            state,
            note: None,
            risk: None,
            at: at.into(),
            sha: None,
            by: None,
            blocked_on: None,
            verify_after_merge: None,
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DerivedState {
    Working,
    Asking,
    Polling,
    Unknown,
    Waiting,
    Abandoned,
    Idle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveWorkState {
    pub state: WorkState,
    pub derived: bool,
    pub blocked: bool,
}

pub fn resolve_work_state(input: &str) -> Option<WorkState> {
    let query = input.trim().to_lowercase();
    if query.is_empty() {
        return None;
    }
    let alias = match query.as_str() {
        "nh" | "human" => Some(WorkState::NeedsHuman),
        "nt" | "testing" => Some(WorkState::NeedsTesting),
        _ => None,
    };
    if alias.is_some() {
        return alias;
    }
    let mut matches = WORK_STATES
        .into_iter()
        .filter(|state| state.as_str().starts_with(&query));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

impl WorkState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Working => "working",
            Self::Review => "review",
            Self::NeedsTesting => "needs-testing",
            Self::NeedsHuman => "needs-human",
            Self::Ready => "ready",
            Self::Verified => "verified",
            Self::Dropped => "dropped",
        }
    }
}
impl WorkRisk {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

pub fn same_work_claim(a: &WorkStatusRecord, b: &WorkStatusRecord) -> bool {
    a.state == b.state
        && a.note == b.note
        && a.risk == b.risk
        && a.sha == b.sha
        && a.blocked_on == b.blocked_on
        && a.verify_after_merge == b.verify_after_merge
}

pub fn work_state_rank(state: Option<WorkState>) -> u8 {
    match state {
        Some(WorkState::Ready) => 0,
        Some(WorkState::NeedsHuman) => 1,
        Some(WorkState::NeedsTesting) => 2,
        Some(WorkState::Review) => 3,
        Some(WorkState::Working) => 4,
        Some(WorkState::Todo) => 7,
        Some(WorkState::Verified | WorkState::Dropped) => 9,
        None => NO_STATUS_RANK,
    }
}

pub fn is_gated(record: Option<&WorkStatusRecord>) -> bool {
    record.is_some_and(|record| {
        record.blocked_on.as_deref().is_some_and(|s| !s.is_empty())
            && matches!(record.state, WorkState::Ready | WorkState::Todo)
    })
}

pub fn work_record_rank(record: Option<&WorkStatusRecord>) -> u8 {
    let Some(record) = record else {
        return NO_STATUS_RANK;
    };
    if !is_gated(Some(record)) {
        return work_state_rank(Some(record.state));
    }
    if record.state == WorkState::Todo {
        GATED_TODO_RANK
    } else {
        BLOCKED_RANK
    }
}

pub fn owes_post_merge_verification(record: Option<&WorkStatusRecord>, landed: bool) -> bool {
    record.is_some_and(|record| {
        landed
            && record
                .verify_after_merge
                .as_deref()
                .is_some_and(|steps| !steps.is_empty())
            && !matches!(record.state, WorkState::Verified | WorkState::Dropped)
    })
}

pub fn effective_work_state(
    record: Option<&WorkStatusRecord>,
    session_state: Option<DerivedState>,
    landed: bool,
) -> Option<EffectiveWorkState> {
    if session_state == Some(DerivedState::Asking) {
        return Some(EffectiveWorkState {
            state: WorkState::NeedsHuman,
            derived: true,
            blocked: false,
        });
    }
    if owes_post_merge_verification(record, landed) {
        return Some(EffectiveWorkState {
            state: WorkState::NeedsTesting,
            derived: true,
            blocked: false,
        });
    }
    record.map(|record| EffectiveWorkState {
        state: record.state,
        derived: false,
        blocked: is_gated(Some(record)),
    })
}

pub fn verification_overdue(record: Option<&WorkStatusRecord>, landed: bool, now_ms: i64) -> bool {
    verification_overdue_with_days(record, landed, now_ms, VERIFY_OVERDUE_DAYS as f64)
}

pub fn verification_overdue_with_days(
    record: Option<&WorkStatusRecord>,
    landed: bool,
    now_ms: i64,
    days: f64,
) -> bool {
    if !owes_post_merge_verification(record, landed) {
        return false;
    }
    let Some(record) = record else {
        return false;
    };
    let Some(at) = parse_iso_millis(&record.at) else {
        return true;
    };
    (now_ms as f64 - at as f64) >= days * 86_400_000.0
}

pub fn is_work_status_stale(
    record: Option<&WorkStatusRecord>,
    last_commit_ms: Option<i64>,
) -> bool {
    let (Some(record), Some(last_commit_ms)) = (record, last_commit_ms) else {
        return false;
    };
    parse_iso_millis(&record.at).is_some_and(|asserted| last_commit_ms > asserted)
}

pub fn work_age(at: &str, now_ms: i64) -> Option<String> {
    let seconds = now_ms.saturating_sub(parse_iso_millis(at)?) / 1000;
    let seconds = seconds.max(0);
    Some(if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    })
}

/// Tolerant persisted-record parser. Unknown keys are preserved; invalid
/// optional known fields are ignored just as the TypeScript parser does.
pub fn parse_work_status(raw: &Value) -> Option<WorkStatusRecord> {
    let object = raw.as_object()?;
    let state = match object.get("state")?.as_str()? {
        "todo" => WorkState::Todo,
        "working" => WorkState::Working,
        "review" => WorkState::Review,
        "needs-testing" => WorkState::NeedsTesting,
        "needs-human" => WorkState::NeedsHuman,
        "ready" => WorkState::Ready,
        "verified" => WorkState::Verified,
        "dropped" => WorkState::Dropped,
        _ => return None,
    };
    let at = object.get("at")?.as_str()?;
    if at.trim().is_empty() {
        return None;
    }
    let risk = match object.get("risk").and_then(Value::as_str) {
        Some("low") => Some(WorkRisk::Low),
        Some("medium") => Some(WorkRisk::Medium),
        Some("high") => Some(WorkRisk::High),
        _ => None,
    };
    let clean_optional = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(sanitize_work_note)
            .filter(|s| !s.is_empty())
    };
    let sha = object
        .get("sha")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned);
    let by = object
        .get("by")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_owned());
    const KNOWN: &[&str] = &[
        "state",
        "note",
        "risk",
        "at",
        "sha",
        "by",
        "blockedOn",
        "verifyAfterMerge",
    ];
    let extra = object
        .iter()
        .filter(|(key, _)| !KNOWN.contains(&key.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Some(WorkStatusRecord {
        state,
        note: clean_optional("note"),
        risk,
        at: at.into(),
        sha,
        by,
        blocked_on: clean_optional("blockedOn"),
        verify_after_merge: clean_optional("verifyAfterMerge"),
        extra,
    })
}

/// Remove terminal-control sequences and control bytes, then collapse all
/// whitespace to single spaces.
pub fn sanitize_work_note(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut cleaned = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\x1b' {
            if chars.get(i + 1) == Some(&']') {
                i += 2;
                while i < chars.len() {
                    if chars[i] == '\x07' {
                        i += 1;
                        break;
                    }
                    if chars[i] == '\x1b' && chars.get(i + 1) == Some(&'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            } else if chars.get(i + 1) == Some(&'[') {
                i += 2;
                while i < chars.len()
                    && (chars[i].is_ascii_digit() || matches!(chars[i], ';' | '?'))
                {
                    i += 1;
                }
                while i < chars.len() && (' '..='/').contains(&chars[i]) {
                    i += 1;
                }
                if i < chars.len() && ('@'..='~').contains(&chars[i]) {
                    i += 1;
                }
            } else if matches!(chars.get(i + 1), Some('(' | ')'))
                && chars.get(i + 2).is_some_and(char::is_ascii_alphanumeric)
            {
                i += 3;
            } else {
                i = (i + 2).min(chars.len());
            }
            continue;
        }
        let ch = chars[i];
        if ch.is_control() || ('\u{80}'..='\u{9f}').contains(&ch) {
            cleaned.push(' ');
        } else {
            cleaned.push(ch);
        }
        i += 1;
    }
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sanitization for untrusted text rendered in terminal-facing UI surfaces.
/// Currently shares the work-status note contract: remove ANSI/control
/// sequences and collapse multiline input into one safe line.
pub fn sanitize_terminal_text(input: &str) -> String {
    sanitize_work_note(input)
}

pub fn work_status_suffix(record: &WorkStatusRecord) -> String {
    let risk = record
        .risk
        .map(|risk| format!(" (risk: {})", risk.as_str()))
        .unwrap_or_default();
    let gate = record
        .blocked_on
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!(" [blocked on: {}]", clamp_for_line(s, 120, 60)))
        .unwrap_or_default();
    let verify = record
        .verify_after_merge
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!(" [verify after merge: {}]", clamp_for_line(s, 120, 35)))
        .unwrap_or_default();
    let note = record
        .note
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!(" — {}", clamp_for_line(s, 400, 200)))
        .unwrap_or_default();
    format!("{risk}{gate}{verify}{note}")
}

pub fn verify_steps_headline(steps: &str) -> String {
    clamp_for_line(steps, 120, 35)
}

fn clamp_for_line(text: &str, max: usize, sentence_floor: usize) -> String {
    if text.encode_utf16().count() <= max {
        return text.to_owned();
    }
    let mut prefix = String::new();
    for ch in text.chars() {
        if prefix.encode_utf16().count() + ch.len_utf16() > max {
            break;
        }
        prefix.push(ch);
    }
    let chars: Vec<char> = prefix.chars().collect();
    let mut sentence_cut = None;
    for i in 0..chars.len().saturating_sub(1) {
        if matches!(chars[i], '.' | '!' | '?') && chars[i + 1].is_whitespace() {
            let length: usize = chars[..=i].iter().map(|c| c.len_utf16()).sum();
            if length >= sentence_floor {
                sentence_cut = Some(i + 1);
            }
        }
    }
    let cut = if let Some(end) = sentence_cut {
        chars[..end].iter().collect::<String>()
    } else if let Some(last_space) = chars.iter().rposition(|ch| ch.is_whitespace()) {
        chars[..last_space]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_owned()
    } else {
        prefix.trim_end().to_owned()
    };
    let cut = cut.strip_suffix('.').unwrap_or(&cut);
    format!("{cut}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const AT: &str = "2026-08-20T12:00:00.000Z";
    fn record(state: WorkState) -> WorkStatusRecord {
        WorkStatusRecord::new(state, AT)
    }

    #[test]
    fn state_prefixes_aliases_and_ambiguity_match_cli_rules() {
        assert_eq!(resolve_work_state(" NH "), Some(WorkState::NeedsHuman));
        assert_eq!(resolve_work_state("testing"), Some(WorkState::NeedsTesting));
        assert_eq!(resolve_work_state("rev"), Some(WorkState::Review));
        assert_eq!(resolve_work_state("r"), None);
        assert_eq!(resolve_work_state("needs-"), None);
        assert_eq!(resolve_work_state(""), None);
    }

    #[test]
    fn gate_ranks_and_effective_state_keep_gate_separate_from_work_state() {
        let mut ready = record(WorkState::Ready);
        ready.blocked_on = Some("mobile release".into());
        assert!(is_gated(Some(&ready)));
        assert_eq!(work_record_rank(Some(&ready)), BLOCKED_RANK);
        assert_eq!(
            effective_work_state(Some(&ready), Some(DerivedState::Asking), false),
            Some(EffectiveWorkState {
                state: WorkState::NeedsHuman,
                derived: true,
                blocked: false
            })
        );
        let mut todo = record(WorkState::Todo);
        todo.blocked_on = Some("secrets land".into());
        assert_eq!(work_record_rank(Some(&todo)), GATED_TODO_RANK);
        for state in [
            WorkState::Working,
            WorkState::Review,
            WorkState::NeedsHuman,
            WorkState::Verified,
            WorkState::Dropped,
        ] {
            assert!(!is_gated(Some(&WorkStatusRecord {
                blocked_on: Some("x".into()),
                ..record(state)
            })));
        }
    }

    #[test]
    fn post_merge_verification_is_dormant_until_landing_then_ages_out() {
        let mut ready = record(WorkState::Ready);
        ready.verify_after_merge = Some("run deployed flow".into());
        assert!(!owes_post_merge_verification(Some(&ready), false));
        assert!(owes_post_merge_verification(Some(&ready), true));
        assert_eq!(
            effective_work_state(Some(&ready), None, true)
                .unwrap()
                .state,
            WorkState::NeedsTesting
        );
        let now = parse_iso_millis("2026-08-22T12:00:00Z").unwrap();
        assert!(verification_overdue_with_days(Some(&ready), true, now, 2.0));
        ready.state = WorkState::Verified;
        assert!(!verification_overdue(Some(&ready), true, now));
        ready.state = WorkState::Dropped;
        assert!(!owes_post_merge_verification(Some(&ready), true));
        ready.state = WorkState::Ready;
        ready.at = "invalid".into();
        assert!(verification_overdue(Some(&ready), true, now));
    }

    #[test]
    fn parse_sanitizes_known_fields_and_retains_unknown_json() {
        let parsed = parse_work_status(&json!({"state":"ready", "at":AT, "risk":"yolo", "note":"\u{001b}[31mred\u{001b}[0m\ntext", "blockedOn":"\u{001b}]0;title\u{0007}release", "future":{"x":1}})).unwrap();
        assert_eq!(parsed.note.as_deref(), Some("red text"));
        assert_eq!(parsed.blocked_on.as_deref(), Some("release"));
        assert_eq!(parsed.risk, None);
        assert_eq!(serde_json::to_value(parsed).unwrap()["future"]["x"], 1);
        assert!(parse_work_status(&json!({"state":"shipped", "at":AT})).is_none());
        assert!(parse_work_status(&json!({"state":"ready", "at":"  "})).is_none());
        let invalid_optional = parse_work_status(
            &json!({"state":"ready", "at":AT, "blockedOn":7, "verifyAfterMerge":false}),
        )
        .unwrap();
        assert_eq!(invalid_optional.blocked_on, None);
        assert_eq!(invalid_optional.verify_after_merge, None);
    }

    #[test]
    fn claim_equality_ignores_timestamp_author_but_tracks_gate_and_verify() {
        let a = record(WorkState::Ready);
        let mut b = a.clone();
        b.at = "different".into();
        b.by = Some("other".into());
        assert!(same_work_claim(&a, &b));
        b.blocked_on = Some("gate".into());
        assert!(!same_work_claim(&a, &b));
        b = a.clone();
        b.verify_after_merge = Some("steps".into());
        assert!(!same_work_claim(&a, &b));
        b = a.clone();
        b.extra
            .insert("futureField".into(), serde_json::json!("new value"));
        assert!(same_work_claim(&a, &b));
    }

    #[test]
    fn age_staleness_and_suffix_are_bounded() {
        let now = parse_iso_millis("2026-08-20T12:00:30Z").unwrap();
        assert_eq!(work_age(AT, now).as_deref(), Some("30s"));
        let mut rec = record(WorkState::Ready);
        rec.risk = Some(WorkRisk::Low);
        rec.blocked_on = Some("release".into());
        rec.note = Some("note".into());
        assert_eq!(
            work_status_suffix(&rec),
            " (risk: low) [blocked on: release] — note"
        );
        let long_note = WorkStatusRecord {
            note: Some("x".repeat(900)),
            ..rec.clone()
        };
        let suffix = work_status_suffix(&long_note);
        assert!(suffix.ends_with("..."));
        assert!(suffix.encode_utf16().count() < 450);
        let short_sentence = format!("Yes. {}", "word ".repeat(60));
        assert!(verify_steps_headline(&short_sentence).starts_with("Yes. word word"));
        assert_eq!(
            verify_steps_headline(&format!(
                "Only a real callback proves this. {}",
                "x".repeat(300)
            )),
            "Only a real callback proves this..."
        );
        assert!(is_work_status_stale(Some(&rec), Some(now)));
        assert!(!is_work_status_stale(Some(&rec), None));
        assert!(verify_steps_headline(&"A complete sentence. ".repeat(10)).ends_with("..."));
    }
}
