use serde_json::{Map, Number, Value};

pub const CURRENT_WT_STATE_VERSION: u64 = 17;

#[derive(Clone, Debug, PartialEq)]
pub struct MigrationOutcome {
    pub value: Value,
    pub from: u64,
    pub to: u64,
}

/// Missing, malformed, fractional, or negative versions are legacy version 0.
pub fn raw_wt_state_version(raw: &Value) -> u64 {
    raw.get("version")
        .and_then(Value::as_number)
        .and_then(|n| {
            n.as_u64().or_else(|| {
                let f = n.as_f64()?;
                (f.is_finite() && f >= 0.0 && f.fract() == 0.0 && f <= u64::MAX as f64)
                    .then_some(f as u64)
            })
        })
        .unwrap_or(0)
}

/// Run the current forward-only migration chain, retaining all unknown fields.
/// A payload from newer code is returned unchanged and is never down-stamped.
pub fn migrate_wt_state(raw: Value) -> MigrationOutcome {
    let from = raw_wt_state_version(&raw);
    if from > CURRENT_WT_STATE_VERSION {
        return MigrationOutcome {
            value: raw,
            from,
            to: from,
        };
    }

    let mut value = as_object_or_empty(raw);
    for to in (from + 1)..=CURRENT_WT_STATE_VERSION {
        match to {
            2 => {
                value
                    .entry("attentionSeenTs")
                    .or_insert(Value::Number(Number::from(0)));
            }
            3 => {
                value
                    .entry("edges")
                    .or_insert_with(|| Value::Array(Vec::new()));
            }
            15 => {
                value
                    .entry("remoteLayouts")
                    .or_insert_with(|| Value::Object(Map::new()));
            }
            16 => {
                value
                    .entry("reviewRequestDismissals")
                    .or_insert_with(|| Value::Array(Vec::new()));
            }
            // v4-v14 and v17 are compatibility boundaries with no data
            // transform. Their version steps still matter for downgrade
            // detection, just as they do in the original TypeScript chain.
            _ => {}
        }
    }
    value.insert(
        "version".to_owned(),
        Value::Number(Number::from(CURRENT_WT_STATE_VERSION)),
    );
    MigrationOutcome {
        value: Value::Object(value),
        from,
        to: CURRENT_WT_STATE_VERSION,
    }
}

fn as_object_or_empty(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(object) => object,
        _ => Map::new(),
    }
}
