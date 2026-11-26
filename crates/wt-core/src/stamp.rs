//! A review stamp: the mark a worktree keeps once it has been through a
//! round of a stamping skill (`[history] stamps`). It lives on the status
//! record as `stamp`, which every status write carries forward, so it stays
//! when the status is asserted again or the head moves.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::WorkStatusRecord;

/// The status record key the stamp is kept under.
pub const STAMP_KEY: &str = "stamp";
/// The slug entry key of a stamp that is not earned yet: its skill started
/// and no status has been asserted since.
pub const STAMP_PENDING_KEY: &str = "stampPending";
/// The marker of a stamped row whose head is the one that was stamped.
pub const STAMP_CURRENT: &str = "\u{2726}";
/// The marker of a stamped row whose head has moved since.
pub const STAMP_MOVED: &str = "\u{2727}";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stamp {
    /// The skill that earned it, without the leading slash.
    pub skill: String,
    /// The head when it was earned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    pub at: String,
    /// Who earned it: a worktree slug, `manager`, or `None` for the person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Stamp {
    /// The stamp a status record carries.
    pub fn of(record: &WorkStatusRecord) -> Option<Self> {
        Self::parse(record.extra.get(STAMP_KEY)?)
    }

    pub fn parse(raw: &Value) -> Option<Self> {
        serde_json::from_value::<Self>(raw.clone())
            .ok()
            .filter(|stamp| is_stamp_name(&stamp.skill))
    }

    /// Whether `head` is the commit that was stamped. An unknown head, or a
    /// stamp without one, has moved as far as anyone can tell.
    pub fn current(&self, head: Option<&str>) -> bool {
        matches!((self.sha.as_deref(), head), (Some(stamped), Some(head)) if stamped == head)
    }

    pub fn glyph(&self, head: Option<&str>) -> &'static str {
        if self.current(head) {
            STAMP_CURRENT
        } else {
            STAMP_MOVED
        }
    }
}

/// Whether `name` can name a stamp: a skill or slash-command name, which is
/// safe to print as it is.
pub fn is_stamp_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_stamp_rides_the_status_record_and_goes_hollow_when_the_head_moves() {
        let record = crate::parse_work_status(&json!({
            "state": "ready", "at": "2026-08-20T12:00:00.000Z",
            "stamp": {"skill": "fable-review", "sha": "abc", "at": "2026-08-20T12:00:00.000Z", "later": 1},
        }))
        .unwrap();
        let stamp = Stamp::of(&record).unwrap();
        assert_eq!(stamp.skill, "fable-review");
        assert_eq!(stamp.glyph(Some("abc")), STAMP_CURRENT);
        assert_eq!(stamp.glyph(Some("def")), STAMP_MOVED);
        assert_eq!(stamp.glyph(None), STAMP_MOVED);
        // What a newer build added is kept, and the record still writes it.
        assert_eq!(serde_json::to_value(&record).unwrap()["stamp"]["later"], 1);

        let unsafe_name = json!({"skill": "a\u{1b}[31m", "at": "x"});
        assert_eq!(Stamp::parse(&unsafe_name), None);
        assert_eq!(Stamp::parse(&json!("fable-review")), None);
    }
}
