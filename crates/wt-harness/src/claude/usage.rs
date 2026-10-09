use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsagePeriod {
    pub utilization: f64,
    pub resets_at: Option<String>,
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeUsage {
    pub five_hour: Option<UsagePeriod>,
    pub seven_day: Option<UsagePeriod>,
    pub seven_day_scoped: Vec<UsagePeriod>,
    pub cached_at_ms: i64,
}

pub fn parse_claude_usage(
    value: &Value,
) -> Option<(Option<UsagePeriod>, Option<UsagePeriod>, Vec<UsagePeriod>)> {
    let limits = |group: &str, scoped: bool| -> Vec<UsagePeriod> {
        value
            .get("limits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                if row.get("group").and_then(Value::as_str) != Some(group) {
                    return None;
                }
                let utilization = row.get("percent")?.as_f64()?;
                let label = row
                    .pointer("/scope/model/display_name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                if scoped != label.is_some() {
                    return None;
                }
                Some(UsagePeriod {
                    utilization,
                    resets_at: row
                        .get("resets_at")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    label,
                })
            })
            .collect()
    };
    let flat = |fields: &[(&str, Option<&str>)]| -> Vec<UsagePeriod> {
        fields
            .iter()
            .filter_map(|(key, label)| {
                let row = value.get(*key)?;
                let utilization = row.get("utilization")?.as_f64()?;
                Some(UsagePeriod {
                    utilization,
                    resets_at: row
                        .get("resets_at")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    label: label.map(str::to_owned),
                })
            })
            .collect()
    };
    let pick = |mut rows: Vec<UsagePeriod>| {
        rows.drain(..)
            .max_by(|a, b| a.utilization.total_cmp(&b.utilization))
    };
    let session_limits = limits("session", false);
    let weekly_limits = limits("weekly", false);
    let mut scoped = limits("weekly", true);
    let five = pick(if session_limits.is_empty() {
        flat(&[("five_hour", None)])
    } else {
        session_limits
    });
    let week = pick(if weekly_limits.is_empty() {
        flat(&[("seven_day", None)])
    } else {
        weekly_limits
    });
    if scoped.is_empty() {
        scoped = flat(&[
            ("seven_day_opus", Some("Opus")),
            ("seven_day_sonnet", Some("Sonnet")),
            ("seven_day_fable", Some("Fable")),
        ]);
    }
    scoped.sort_by(|a, b| b.utilization.total_cmp(&a.utilization));
    if five.is_none() && week.is_none() && scoped.is_empty() {
        None
    } else {
        Some((five, week, scoped))
    }
}

pub fn read_claude_usage(path: &Path) -> Option<ClaudeUsage> {
    let metadata = fs::metadata(path).ok()?;
    let bytes = fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let (five_hour, seven_day, seven_day_scoped) = parse_claude_usage(&value)?;
    let cached_at_ms = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()?;
    Some(ClaudeUsage {
        five_hour,
        seven_day,
        seven_day_scoped,
        cached_at_ms,
    })
}

pub fn usage_window_key(period: &UsagePeriod, weekly: bool) -> String {
    if weekly {
        match period.label.as_deref() {
            Some(label) => format!(
                "7{}",
                label
                    .split_whitespace()
                    .last()
                    .unwrap_or(label)
                    .chars()
                    .next()
                    .unwrap_or(' ')
                    .to_ascii_lowercase()
            ),
            None => "7d".to_owned(),
        }
    } else {
        "5h".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mixed_api_shapes_resolve_each_limit_category_independently() {
        let data = json!({
            "five_hour":{"utilization":55},
            "seven_day":{"utilization":17},
            "limits":[
                {"group":"session","percent":7},
                {"group":"weekly","percent":21,"scope":{"model":{"display_name":"Fable"}}}
            ]
        });
        let (five, weekly, scoped) = parse_claude_usage(&data).unwrap();
        assert_eq!(five.unwrap().utilization, 7.0);
        assert_eq!(weekly.unwrap().utilization, 17.0);
        assert_eq!(scoped[0].label.as_deref(), Some("Fable"));
    }
}
