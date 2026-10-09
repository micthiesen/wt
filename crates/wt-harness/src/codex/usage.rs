use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::UsagePeriod;

#[derive(Clone, Debug, PartialEq)]
pub struct CodexUsage {
    pub five_hour: Option<UsagePeriod>,
    pub seven_day: Option<UsagePeriod>,
    pub plan_type: Option<String>,
    pub cached_at_ms: i64,
}

pub fn read_codex_usage(sessions_dir: &Path) -> Option<CodexUsage> {
    let latest = latest_rollout(sessions_dir)?;
    let metadata = fs::metadata(&latest).ok()?;
    if metadata.len() == 0 {
        return None;
    }
    let size = metadata.len();
    let mut file = fs::File::open(&latest).ok()?;
    file.seek(SeekFrom::Start(size.saturating_sub(96 * 1024)))
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines().rev() {
        if !line.contains("rate_limits") {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event["type"] != "event_msg" || event["payload"]["type"] != "token_count" {
            continue;
        }
        let limits = &event["payload"]["rate_limits"];
        if !limits.is_object() {
            continue;
        }
        let primary = period(&limits["primary"]);
        let secondary = period(&limits["secondary"]);
        let primary_minutes = limits["primary"]["window_minutes"].as_i64();
        let secondary_minutes = limits["secondary"]["window_minutes"].as_i64();
        let five_hour = if primary_minutes == Some(300) {
            primary.clone()
        } else if secondary_minutes == Some(300) {
            secondary.clone()
        } else if primary_minutes.is_none() {
            primary.clone()
        } else {
            None
        };
        let seven_day = if primary_minutes == Some(10_080) {
            primary
        } else if secondary_minutes == Some(10_080) || secondary_minutes.is_none() {
            secondary
        } else {
            None
        };
        if five_hour.is_none() && seven_day.is_none() {
            continue;
        }
        let cached_at_ms = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis()
            .min(i64::MAX as u128) as i64;
        return Some(CodexUsage {
            five_hour,
            seven_day,
            plan_type: limits["plan_type"].as_str().map(str::to_owned),
            cached_at_ms,
        });
    }
    None
}

fn period(value: &Value) -> Option<UsagePeriod> {
    let utilization = value.get("used_percent")?.as_f64()?;
    let resets_at = value
        .get("resets_at")
        .and_then(Value::as_i64)
        .and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok())
        .and_then(|time| time.format(&Rfc3339).ok());
    Some(UsagePeriod {
        utilization,
        resets_at,
        label: None,
    })
}

fn latest_rollout(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut latest: Option<(i64, PathBuf)> = None;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ty) = entry.file_type() else {
                continue;
            };
            if ty.is_dir() {
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = metadata.modified().and_then(|time| {
                time.duration_since(std::time::UNIX_EPOCH)
                    .map_err(std::io::Error::other)
            }) else {
                continue;
            };
            let modified = modified.as_millis().min(i64::MAX as u128) as i64;
            if latest.as_ref().is_none_or(|(time, _)| modified > *time) {
                latest = Some((modified, path));
            }
        }
    }
    latest.map(|(_, path)| path)
}
