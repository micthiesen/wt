//! Watches: things a worktree is waiting for, and what happens when they
//! hold. A watch names conditions (all must hold) and the actions still owed
//! once they do: clear the merge gate, tell the person, wake the session.
//!
//! Everything here is pure. The caller supplies the clock, the local UTC
//! offset and the facts; unknown facts never make a condition hold.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::{
    VerifyStage,
    time::{days_from_civil, days_in_month},
};

/// Watches one worktree may hold.
pub const WATCHES_PER_SLUG_MAX: usize = 20;
/// Watches one repository may hold.
pub const WATCHES_MAX: usize = 200;

const SECOND_MS: i64 = 1_000;
const MINUTE_MS: i64 = 60 * SECOND_MS;
const HOUR_MS: i64 = 60 * MINUTE_MS;
const DAY_MS: i64 = 24 * HOUR_MS;
const DELAY_MAX_MS: i64 = 366 * DAY_MS;

/// The caller's wall clock: the instant, and how far local time is ahead of
/// UTC at that instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalClock {
    pub now_ms: i64,
    pub utc_offset_minutes: i32,
}

/// A state a worktree's row can be in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RowState {
    /// Landed on the base branch.
    Merged,
    Staging,
    Production,
    /// Work status ready with no merge gate.
    Ready,
    /// An open PR whose checks pass on its current head.
    Green,
    Verified,
    /// PR closed without merging.
    Closed,
    /// An open PR whose checks finished on its current head, passed or
    /// failed.
    Settled,
    /// An open PR whose review bot finished reviewing its current head.
    Reviewed,
    /// An open PR whose armed merge is gone: dropped from the merge queue,
    /// or auto-merge switched off.
    Evicted,
}

impl RowState {
    pub const ALL: [Self; 10] = [
        Self::Merged,
        Self::Staging,
        Self::Production,
        Self::Ready,
        Self::Green,
        Self::Verified,
        Self::Closed,
        Self::Settled,
        Self::Reviewed,
        Self::Evicted,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Staging => "staging",
            Self::Production => "production",
            Self::Ready => "ready",
            Self::Green => "green",
            Self::Verified => "verified",
            Self::Closed => "closed",
            Self::Settled => "settled",
            Self::Reviewed => "reviewed",
            Self::Evicted => "evicted",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == value)
    }

    /// Whether the state can stop holding once it has held.
    const fn revocable(self) -> bool {
        matches!(
            self,
            Self::Ready | Self::Green | Self::Settled | Self::Reviewed | Self::Evicted
        )
    }

    fn phrase(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Staging => "on staging",
            Self::Production => "in production",
            Self::Ready => "ready",
            Self::Green => "checks green",
            Self::Verified => "verified",
            Self::Closed => "closed",
            Self::Settled => "checks settled",
            Self::Reviewed => "bot review done",
            Self::Evicted => "merge dropped",
        }
    }
}

/// What a watch waits for on a pull request it names by number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrGoal {
    Merged,
    Reached(VerifyStage),
    Closed,
}

impl PrGoal {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Reached(stage) => stage.as_str(),
            Self::Closed => "closed",
        }
    }
}

/// One thing a watch waits for. Stored as its `Display` text, which
/// [`parse`] reads back.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Condition {
    /// An instant, in Unix milliseconds.
    At {
        at_ms: i64,
    },
    /// The watch's own worktree.
    Own(RowState),
    Pr {
        number: u64,
        goal: PrGoal,
    },
    /// Another worktree.
    Worktree {
        slug: String,
        state: RowState,
    },
    /// No live worktree has this slug.
    Gone {
        slug: String,
    },
    /// The next deployment of a stage after the condition's baseline.
    Release(VerifyStage),
    /// A probe from configuration.
    Check {
        id: String,
        arg: Option<String>,
    },
    /// A commit reached a stage. Written by wt in place of a condition on a
    /// row or PR that no longer needs watching once its landing commit is
    /// known.
    Commit {
        oid: String,
        stage: VerifyStage,
    },
    /// Text a newer build wrote. It never holds and is kept verbatim.
    Unknown(String),
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::At { at_ms } => write!(f, "at:{at_ms}"),
            Self::Own(state) => f.write_str(state.as_str()),
            Self::Pr { number, goal } => write!(f, "pr:{number}:{}", goal.as_str()),
            // The prefix keeps a worktree named like a word of the grammar
            // (`release`, `pr`, `wt`) apart from that word.
            Self::Worktree { slug, state } => write!(f, "wt:{slug}:{}", state.as_str()),
            Self::Gone { slug } => write!(f, "wt:{slug}:gone"),
            Self::Release(stage) => write!(f, "release:{}", stage.as_str()),
            Self::Check { id, arg: None } => write!(f, "check:{id}"),
            Self::Check { id, arg: Some(arg) } => write!(f, "check:{id}={arg}"),
            Self::Commit { oid, stage } => write!(f, "commit:{oid}:{}", stage.as_str()),
            Self::Unknown(text) => f.write_str(text),
        }
    }
}

impl Condition {
    /// The text a record keeps. A pull request keeps the `#` spelling,
    /// which builds from before `pr:` read too; a record is never typed
    /// into a shell.
    fn stored(&self) -> String {
        match self {
            Self::Pr { number, goal } => format!("#{number}:{}", goal.as_str()),
            other => other.to_string(),
        }
    }

    /// Reads every spelling a build has stored, including a worktree
    /// without its `wt:` prefix, which typed input no longer always may be.
    fn from_stored(text: &str) -> Self {
        absolute_ms(text)
            .map(|at_ms| Self::At { at_ms })
            .or_else(|| parse_untimed(text, Spelling::Stored).ok())
            .unwrap_or_else(|| Self::Unknown(text.to_owned()))
    }

    fn revocable(&self) -> bool {
        match self {
            Self::Own(state) | Self::Worktree { state, .. } => state.revocable(),
            Self::Check { .. } => true,
            _ => false,
        }
    }

    fn phrase(&self, local: Local) -> String {
        match self {
            Self::At { at_ms } => local.describe(*at_ms),
            Self::Own(RowState::Green) => "PR checks green".into(),
            Self::Own(RowState::Closed) => "PR closed".into(),
            Self::Own(RowState::Settled) => "PR checks settled".into(),
            Self::Own(RowState::Reviewed) => "bot review of the current head done".into(),
            Self::Own(state) => state.phrase().into(),
            Self::Pr { number, goal } => match goal {
                PrGoal::Merged => format!("#{number} merged"),
                PrGoal::Reached(VerifyStage::Staging) => format!("#{number} on staging"),
                PrGoal::Reached(VerifyStage::Production) => format!("#{number} in production"),
                PrGoal::Closed => format!("#{number} closed"),
            },
            Self::Worktree { slug, state } => format!("{slug} {}", state.phrase()),
            Self::Gone { slug } => format!("{slug} removed"),
            Self::Release(stage) => format!("next {} release", stage.as_str()),
            Self::Check { .. } => self.to_string().replacen(':', " ", 1),
            Self::Commit { oid, stage } => match stage {
                VerifyStage::Staging => format!("{} on staging", short(oid)),
                VerifyStage::Production => format!("{} in production", short(oid)),
            },
            Self::Unknown(text) => text.clone(),
        }
    }
}

impl Serialize for Condition {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.stored())
    }
}

impl<'de> Deserialize<'de> for Condition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_stored(&String::deserialize(deserializer)?))
    }
}

/// How long a condition must have held without a break.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Delay(i64);

impl Delay {
    /// `None` unless the delay is positive and at most a year.
    pub fn from_ms(ms: i64) -> Option<Self> {
        (1..=DELAY_MAX_MS).contains(&ms).then_some(Self(ms))
    }

    pub const fn as_ms(self) -> i64 {
        self.0
    }

    /// `90m`, `3h`, `2d` or `30s`.
    pub fn parse(text: &str) -> Option<Self> {
        let unit = match text.as_bytes().last()? {
            b's' => SECOND_MS,
            b'm' => MINUTE_MS,
            b'h' => HOUR_MS,
            b'd' => DAY_MS,
            _ => return None,
        };
        Self::from_ms(digits(&text[..text.len() - 1])?.checked_mul(unit)?)
    }
}

impl fmt::Display for Delay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (unit, suffix) = [(DAY_MS, "d"), (HOUR_MS, "h"), (MINUTE_MS, "m")]
            .into_iter()
            .find(|(unit, _)| self.0 % unit == 0)
            .unwrap_or((SECOND_MS, "s"));
        write!(f, "{}{suffix}", self.0 / unit)
    }
}

/// A condition as one watch holds it, with what the watch has seen of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchCondition {
    pub cond: Condition,
    #[serde(default, rename = "delayMs", skip_serializing_if = "Option::is_none")]
    pub delay: Option<Delay>,
    /// What a release condition compares against: the stage's release marker
    /// when the watch was created, or when it was first seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    /// Since when the condition has held without a known break.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_held_at_ms: Option<i64>,
    /// Held for good since `first_held_at_ms`: the facts are no longer
    /// consulted.
    #[serde(default, skip_serializing_if = "is_false")]
    pub latched: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl WatchCondition {
    pub fn new(cond: Condition, delay: Option<Delay>) -> Self {
        Self {
            cond,
            delay,
            baseline: None,
            first_held_at_ms: None,
            latched: false,
            extra: Map::new(),
        }
    }

    /// The condition as [`parse`] reads it back, with a time written as
    /// local wall clock.
    pub fn to_arg(&self, utc_offset_minutes: i32) -> String {
        let cond = match &self.cond {
            Condition::At { at_ms } => Local::absolute(utc_offset_minutes).stamp(*at_ms),
            cond => cond.to_string(),
        };
        match self.delay {
            Some(delay) => format!("{cond}+{delay}"),
            None => cond,
        }
    }

    /// The condition in words, with a time relative to today: `17:00`,
    /// `09:00 tomorrow`, `Nov 26 17:00`, `#3806 in production`, `PR checks
    /// green for 1h`.
    pub fn describe(&self, clock: LocalClock) -> String {
        self.describe_in(Local {
            utc_offset_minutes: clock.utc_offset_minutes,
            now_ms: Some(clock.now_ms),
        })
    }

    /// The condition in as few words as name it, for a narrow column:
    /// `production`, `production +7d`, `#3889 merged`, `Fri 18:12`.
    pub fn label(&self, clock: LocalClock) -> String {
        let stage_of = |stage: VerifyStage| stage.as_str();
        let label = match &self.cond {
            Condition::At { at_ms } => terse_instant(*at_ms, clock),
            Condition::Own(RowState::Green) => "checks green".into(),
            Condition::Own(RowState::Closed) => "PR closed".into(),
            Condition::Own(state) => state.as_str().into(),
            Condition::Pr { number, goal } => format!("#{number} {}", goal.as_str()),
            Condition::Worktree { slug, state } => {
                format!("{} {}", clipped(slug, LABEL_SLUG_CHARS), state.as_str())
            }
            Condition::Gone { slug } => format!("{} removed", clipped(slug, LABEL_SLUG_CHARS)),
            Condition::Release(stage) => format!("{} release", stage_of(*stage)),
            Condition::Check { id, .. } => format!("check {id}"),
            Condition::Commit { oid, stage } => format!("{} {}", short(oid), stage_of(*stage)),
            Condition::Unknown(text) => text.clone(),
        };
        match self.delay {
            Some(delay) => format!("{label} +{delay}"),
            None => label,
        }
    }

    /// The stage this condition waits for a deploy to, while that deploy
    /// has not happened: the row's own stage, another pull request's or
    /// commit's, or the stage's next release.
    pub fn awaits_stage(&self) -> Option<VerifyStage> {
        if self.latched || self.first_held_at_ms.is_some() {
            return None;
        }
        match self.cond {
            Condition::Own(RowState::Staging) => Some(VerifyStage::Staging),
            Condition::Own(RowState::Production) => Some(VerifyStage::Production),
            Condition::Pr {
                goal: PrGoal::Reached(stage),
                ..
            }
            | Condition::Commit { stage, .. }
            | Condition::Release(stage) => Some(stage),
            _ => None,
        }
    }

    /// When the condition is satisfied, where that is a known instant: a
    /// time, or a condition that already holds and is waiting out its
    /// delay (or has none left to wait out).
    pub fn known_at_ms(&self) -> Option<i64> {
        let delay = self.delay.map_or(0, Delay::as_ms);
        match self.cond {
            Condition::At { at_ms } => Some(at_ms.saturating_add(delay)),
            _ => self
                .first_held_at_ms
                .map(|since| since.saturating_add(delay)),
        }
    }

    fn describe_in(&self, local: Local) -> String {
        let phrase = self.cond.phrase(local);
        match self.delay {
            Some(delay) if self.cond.revocable() => format!("{phrase} for {delay}"),
            // The row's own stage reads "in production" or "on staging" on
            // its own; after a delay it is the moment it got there.
            Some(delay) => match phrase
                .strip_prefix("in ")
                .or_else(|| phrase.strip_prefix("on "))
            {
                Some(stage) if matches!(self.cond, Condition::Own(_)) => {
                    format!("{delay} after it reaches {stage}")
                }
                _ => format!("{delay} after {phrase}"),
            },
            None => phrase,
        }
    }
}

/// One thing a met watch does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WatchAction {
    Unblock,
    Notify,
    Wake,
}

impl WatchAction {
    /// In the order a met watch delivers them.
    pub const ALL: [Self; 3] = [Self::Unblock, Self::Notify, Self::Wake];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unblock => "unblock",
            Self::Notify => "notify",
            Self::Wake => "wake",
        }
    }
}

/// The automation fire key of one action of one watch.
pub fn action_key(id: &str, action: WatchAction) -> String {
    format!("wt.watch:{id}:{}", action.as_str())
}

/// The actions a watch still owes. A delivered action is removed, so what is
/// here is exactly what remains to do.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchActions {
    /// Clear the merge gate, if it still reads exactly this text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unblock: Option<String>,
    /// Tell the person. Empty text means the default wording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<String>,
    /// Send this prompt to the worktree's session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl WatchActions {
    pub fn get(&self, action: WatchAction) -> Option<&str> {
        match action {
            WatchAction::Unblock => self.unblock.as_deref(),
            WatchAction::Notify => self.notify.as_deref(),
            WatchAction::Wake => self.wake.as_deref(),
        }
    }

    /// Remove an action, returning its text if it was still owed.
    pub fn take(&mut self, action: WatchAction) -> Option<String> {
        match action {
            WatchAction::Unblock => self.unblock.take(),
            WatchAction::Notify => self.notify.take(),
            WatchAction::Wake => self.wake.take(),
        }
    }

    pub fn remaining(&self) -> impl Iterator<Item = WatchAction> + '_ {
        WatchAction::ALL
            .into_iter()
            .filter(|action| self.get(*action).is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.remaining().next().is_none()
    }

    pub fn notify_only(&self) -> bool {
        self.remaining().eq([WatchAction::Notify])
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Watch {
    pub id: String,
    /// The worktree the watch belongs to.
    pub slug: String,
    /// The owner's `createdAt` when the watch was set. A later worktree with
    /// the same slug is another incarnation and does not inherit the watch.
    /// `None` when the owner had no recorded creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<String>,
    pub by: String,
    pub created_at_ms: i64,
    /// The setter's local UTC offset, so text written later about the watch
    /// shows the times its setter meant.
    #[serde(default)]
    pub utc_offset_minutes: i32,
    pub when: Vec<WatchCondition>,
    pub then: WatchActions,
    /// When every condition held. Set once and never cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub met_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The owner's worktree is gone and the watch outlived it; its
    /// conditions no longer refer to the owner's row.
    #[serde(default, skip_serializing_if = "is_false")]
    pub owner_gone: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

// Kept beside the record's other keys rather than as fields, so a build
// that does not know them carries them through a rewrite like any other.
const LANDING_COMMIT: &str = "landingCommit";
const BEFORE_RELEASE: &str = "beforeRelease";
const OWED_ON_DELIVERY: &str = "owedOnDelivery";
const EVALUATED_AT: &str = "evaluatedAtMs";
/// How often an engine records again that it judged a watch. Every stamp
/// rewrites the state and wakes every process that reads it, and the
/// record is only asked whether an engine has judged the watch at all.
pub const EVALUATED_EVERY_MS: i64 = HOUR_MS;

/// What is known about a removed owner's branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerRemoval<'a> {
    /// The removal record says the branch had landed.
    pub landed: bool,
    /// The commit that carried the branch's work onto the base branch.
    pub landing_commit: Option<&'a str>,
}

impl Watch {
    pub fn is_met(&self) -> bool {
        self.met_at_ms.is_some()
    }

    /// The commit that carried the owner's work onto the base branch, once
    /// the engine has seen it: what the watch follows after its worktree is
    /// removed, even by a process that never saw the row landed.
    pub fn landing_commit(&self) -> Option<&str> {
        self.extra.get(LANDING_COMMIT)?.as_str()
    }

    /// When an engine recorded judging the watch, to the minute: the first
    /// time, then about hourly. `None` until one has.
    pub fn evaluated_at_ms(&self) -> Option<i64> {
        self.extra.get(EVALUATED_AT)?.as_i64()
    }

    /// Whether what the watch wakes its session for must happen before the
    /// next production release (`wt wake --before-release`). Listing and
    /// display only.
    pub fn before_release(&self) -> bool {
        self.extra.get(BEFORE_RELEASE).and_then(Value::as_bool) == Some(true)
    }

    pub fn set_before_release(&mut self, before_release: bool) {
        if before_release {
            self.extra
                .insert(BEFORE_RELEASE.to_owned(), Value::Bool(true));
        } else {
            self.extra.remove(BEFORE_RELEASE);
        }
    }

    /// Whether the wake stays owed as a due step once delivered, wherever
    /// the worktree's current work stands. Set on the wake an earlier pull
    /// request's check becomes: that work has landed, whatever the
    /// worktree is on now.
    pub fn owed_on_delivery(&self) -> bool {
        self.extra.get(OWED_ON_DELIVERY).and_then(Value::as_bool) == Some(true)
    }

    pub fn set_owed_on_delivery(&mut self, owed: bool) {
        if owed {
            self.extra
                .insert(OWED_ON_DELIVERY.to_owned(), Value::Bool(true));
        } else {
            self.extra.remove(OWED_ON_DELIVERY);
        }
    }

    /// Whether the watch still owes its owner's session a wake. A watch
    /// that outlived its owner wakes nobody.
    pub fn wakes_owner(&self) -> bool {
        self.then.wake.is_some() && !self.owner_gone
    }

    /// Whether any condition is about the owner's own row reaching a stage.
    pub fn follows_own_landing(&self) -> bool {
        self.when.iter().any(|cond| {
            matches!(
                cond.cond,
                Condition::Own(RowState::Staging | RowState::Production)
            )
        })
    }

    /// Whether `created_at` names the worktree this watch was set on.
    /// A watch set on a worktree with no recorded creation belongs only to
    /// one that still has none: any worktree created later has a stamp.
    pub fn owned_by(&self, created_at: Option<&str>) -> bool {
        self.incarnation.as_deref() == created_at
    }

    /// The conditions in words for a reader now.
    pub fn describe(&self, clock: LocalClock) -> String {
        self.join(|cond| cond.describe(clock))
    }

    /// The conditions in words for text that is kept, with times in the
    /// setter's local time and relative to the day the watch was set.
    pub fn summary(&self) -> String {
        self.summary_on(self.created_at_ms)
    }

    /// [`Self::summary`] for text about what the watch did once it was met,
    /// with times relative to that day.
    pub fn summary_met(&self) -> String {
        self.summary_on(self.met_at_ms.unwrap_or(self.created_at_ms))
    }

    fn summary_on(&self, day_ms: i64) -> String {
        let local = Local {
            utc_offset_minutes: self.utc_offset_minutes,
            now_ms: Some(day_ms),
        };
        self.join(|cond| cond.describe_in(local))
    }

    /// Whether the watch waits for one instant and nothing else, which
    /// reads "at 17:00" where anything else reads "when merged".
    pub fn timed(&self) -> bool {
        matches!(self.when.as_slice(), [only] if matches!(only.cond, Condition::At { .. }))
    }

    /// [`Self::summary`] led by its connective: `at 17:00`, `when merged`.
    pub fn when_summary(&self) -> String {
        self.connected(self.summary())
    }

    /// [`Self::describe`] led by its connective.
    pub fn when_described(&self, clock: LocalClock) -> String {
        self.connected(self.describe(clock))
    }

    fn connected(&self, conditions: String) -> String {
        if self.timed() {
            format!("at {conditions}")
        } else {
            format!("when {conditions}")
        }
    }

    fn join(&self, describe: impl Fn(&WatchCondition) -> String) -> String {
        self.when
            .iter()
            .map(describe)
            .collect::<Vec<_>>()
            .join(" and ")
    }

    /// Mark own-row `merged` conditions held for good. Returns whether any
    /// changed.
    pub fn latch_own_merged(&mut self, now_ms: i64) -> bool {
        let mut changed = false;
        for cond in &mut self.when {
            if cond.cond == Condition::Own(RowState::Merged) && !cond.latched {
                cond.latched = true;
                cond.first_held_at_ms.get_or_insert(now_ms);
                changed = true;
            }
        }
        changed
    }

    /// The watch as it goes on once its owner's worktree is gone, or why it
    /// cannot. Only a watch that just tells the person outlives a landed
    /// owner; its own-row conditions become conditions on the landing commit.
    pub fn without_owner(&self, removal: OwnerRemoval<'_>, now_ms: i64) -> Result<Self, String> {
        if self.owner_gone {
            return Ok(self.clone());
        }
        if !self.then.notify_only() {
            return Err("its worktree was removed".into());
        }
        if !removal.landed {
            return Err("its worktree was removed before landing".into());
        }
        let mut next = self.clone();
        next.owner_gone = true;
        next.latch_own_merged(now_ms);
        for cond in &mut next.when {
            let stage = match cond.cond {
                Condition::Own(RowState::Staging) => VerifyStage::Staging,
                Condition::Own(RowState::Production) => VerifyStage::Production,
                Condition::Own(state) if !cond.latched => {
                    return Err(format!(
                        "its worktree was removed and can no longer become {}",
                        state.as_str()
                    ));
                }
                _ => continue,
            };
            let Some(oid) = removal.landing_commit.or(self.landing_commit()) else {
                return Err("its worktree was removed and no landing commit is known".into());
            };
            cond.cond = Condition::Commit {
                oid: oid.to_owned(),
                stage,
            };
        }
        Ok(next)
    }
}

/// Why a condition argument was refused. Its text teaches the grammar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrammarError {
    pub arg: String,
    pub problem: String,
}

const GRAMMAR: &str = "A condition is one of:
  17:00 | 5pm | 5:30pm | 3h | 90m | 2d | \"tomorrow 09:00\" | 2030-01-15T09:00    a local time
  merged | staging | production | ready | green | verified | closed    this worktree
  settled | reviewed | evicted    its open PR: checks finished (passed or failed),
      the review bot finished the current head, the armed merge was dropped
  pr:3806 | pr:3806:merged|staging|production|closed    a pull request
  wt:<slug>:merged|staging|production|ready|green|verified|closed|settled|reviewed|evicted|gone
      another worktree
      (<slug>:<state> reads the same unless the slug is a word of this grammar)
  release:staging | release:production    the next release there
  check:<id> | check:<id>=<arg>    a configured check
Anything but a time may end in +<duration>: it must have held that long.
A pull request also reads as #3806, but an unquoted # starts a shell comment
and the rest of the command is lost: write pr:3806.
Examples: pr:3806:production   release:production+24h   17:00";

impl fmt::Display for GrammarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot read the condition `{}`: {}\n{GRAMMAR}",
            self.arg, self.problem
        )
    }
}

impl std::error::Error for GrammarError {}

/// Read one condition argument. Times are local wall clock at the caller's
/// offset; `17:00` is its next occurrence, and a time that has passed is
/// refused.
pub fn parse(arg: &str, clock: LocalClock) -> Result<WatchCondition, GrammarError> {
    let fail = |problem: String| GrammarError {
        arg: arg.to_owned(),
        problem,
    };
    let text = arg.trim();
    if text.is_empty() {
        return Err(fail("it is empty".into()));
    }
    if let Some(at_ms) = parse_time(text, clock).map_err(&fail)? {
        if at_ms <= clock.now_ms {
            return Err(fail("that time has already passed".into()));
        }
        return Ok(WatchCondition::new(Condition::At { at_ms }, None));
    }
    let (body, delay) = split_delay(text).map_err(&fail)?;
    if delay.is_some() && !matches!(parse_time(body, clock), Ok(None)) {
        return Err(fail("a time takes no delay; write the later time".into()));
    }
    let cond = parse_untimed(body, Spelling::Typed).map_err(|problem| {
        fail(if looks_like_time(text) {
            TIME_FORMS.to_owned()
        } else {
            problem
        })
    })?;
    Ok(WatchCondition::new(cond, delay))
}

fn absolute_ms(text: &str) -> Option<i64> {
    digits(text.strip_prefix("at:")?)
}

const TIME_FORMS: &str = "a time is a clock time (17:00, 5pm, 5:30pm), a time from now (3h, 90m, 2d), \"tomorrow 09:00\" as one quoted argument, or a date and time (2030-01-15T09:00)";

/// Whether text that is no condition was probably meant as a time, so its
/// refusal should say how a time is written.
fn looks_like_time(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    // A slug may start with a digit; `<slug>:<state>` has no space.
    (lower.starts_with(|first: char| first.is_ascii_digit())
        && (!lower.contains(':') || lower.contains(' ')))
        || [
            "in ", "at ", "today", "tomorrow", "tonight", "next ", "noon", "midnight",
        ]
        .iter()
        .any(|word| lower.starts_with(word))
        || lower.contains("o'clock")
}

/// `17:00`, `5pm` or `5:30pm` as milliseconds into the day. `None` when the
/// text is not a clock time at all.
fn clock_of_day(text: &str) -> Result<Option<i64>, String> {
    let lower = text.to_ascii_lowercase();
    let (body, half) = match (lower.strip_suffix("am"), lower.strip_suffix("pm")) {
        (Some(body), _) => (body.trim_end(), Some(0)),
        (_, Some(body)) => (body.trim_end(), Some(12)),
        _ => (lower.as_str(), None),
    };
    let (hour, minute) = match body.split_once(':') {
        Some((hour, minute)) if minute.len() == 2 => (hour, digits(minute)),
        Some(_) => return Ok(None),
        // An hour alone is a time only with am or pm.
        None if half.is_some() => (body, Some(0)),
        None => return Ok(None),
    };
    let (Some(hour), Some(minute)) = (
        Some(hour)
            .filter(|hour| (1..=2).contains(&hour.len()))
            .and_then(digits),
        minute,
    ) else {
        return Ok(None);
    };
    let hour = match half {
        None if hour > 23 => return Err("a clock time runs from 00:00 to 23:59".into()),
        None => hour,
        Some(_) if !(1..=12).contains(&hour) => {
            return Err("a 12-hour time runs from 12am to 11:59pm; 17:00 works too".into());
        }
        Some(half) => hour % 12 + half,
    };
    if minute > 59 {
        return Err("a clock time runs from 00:00 to 23:59".into());
    }
    Ok(Some(hour * HOUR_MS + minute * MINUTE_MS))
}

fn parse_time(text: &str, clock: LocalClock) -> Result<Option<i64>, String> {
    let offset_ms = i64::from(clock.utc_offset_minutes) * MINUTE_MS;
    // `3h` is `+3h`: nothing else in the grammar is digits and a unit.
    let from_now = text.strip_prefix('+').or_else(|| {
        Some(text).filter(|text| {
            text.starts_with(|first: char| first.is_ascii_digit()) && Delay::parse(text).is_some()
        })
    });
    if let Some(duration) = from_now {
        let delay = Delay::parse(duration)
            .ok_or("a time from now looks like +90m, +3h or +2d, up to a year")?;
        // Whole seconds, so the time reads back exactly as it prints.
        let at = clock.now_ms + delay.as_ms() + SECOND_MS - 1;
        return Ok(Some(at - at.rem_euclid(SECOND_MS)));
    }
    if let Some(at_ms) = absolute_ms(text) {
        return Ok(Some(at_ms));
    }
    let local_now = clock.now_ms + offset_ms;
    let midnight = local_now - local_now.rem_euclid(DAY_MS);
    let lower = text.to_ascii_lowercase();
    for (word, day) in [("today", 0), ("tomorrow", DAY_MS)] {
        if let Some(rest) = lower.strip_prefix(word)
            && rest.starts_with(' ')
        {
            let of_day = clock_of_day(rest.trim())?.ok_or_else(|| {
                format!("after `{word}` comes a clock time: \"{word} 09:00\"; any other day is a date and time, 2030-01-15T09:00")
            })?;
            return Ok(Some(midnight + day + of_day - offset_ms));
        }
    }
    if let Some(of_day) = clock_of_day(text)? {
        let mut at = midnight + of_day;
        if at <= local_now {
            at += DAY_MS;
        }
        return Ok(Some(at - offset_ms));
    }
    let bytes = text.as_bytes();
    let dated = bytes.len() >= 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && (bytes.len() == 10 || matches!(bytes[10], b'T' | b't' | b' '));
    if !dated {
        return Ok(None);
    }
    let shape = "a date and time looks like 2030-01-15T09:00";
    let mut clock_parts = text.get(11..).ok_or(shape)?.split(':');
    let (Some(hour), Some(minute), second, None) = (
        clock_parts.next(),
        clock_parts.next(),
        clock_parts.next(),
        clock_parts.next(),
    ) else {
        return Err(shape.into());
    };
    let field = |text: Option<&str>| text.filter(|text| text.len() == 2).and_then(digits);
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        text.get(..4).and_then(digits),
        field(text.get(5..7)),
        field(text.get(8..10)),
        field(Some(hour)),
        field(Some(minute)),
        second.map_or(Some(0), |second| field(Some(second))),
    ) else {
        return Err(shape.into());
    };
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err("that date or time does not exist".into());
    }
    Ok(Some(
        days_from_civil(year, month, day) * DAY_MS
            + hour * HOUR_MS
            + minute * MINUTE_MS
            + second * SECOND_MS
            - offset_ms,
    ))
}

fn digits(text: &str) -> Option<i64> {
    (!text.is_empty() && text.len() <= 18 && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn split_delay(text: &str) -> Result<(&str, Option<Delay>), String> {
    let Some((body, duration)) = text.rsplit_once('+') else {
        return Ok((text, None));
    };
    match Delay::parse(duration) {
        Some(delay) if !body.is_empty() => Ok((body, Some(delay))),
        // A check's argument is free text and may contain a plus sign.
        _ if text.starts_with("check:") => Ok((text, None)),
        _ => Err("a delay looks like +90m, +3h or +2d, up to a year".into()),
    }
}

fn parse_stage(text: &str) -> Option<VerifyStage> {
    match text {
        "staging" => Some(VerifyStage::Staging),
        "production" => Some(VerifyStage::Production),
        _ => None,
    }
}

/// Where a condition's text came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Spelling {
    /// An argument someone typed: a worktree whose slug is a word of the
    /// grammar must be named with `wt:`.
    Typed,
    /// A record: whatever a build has stored reads back.
    Stored,
}

/// Slugs that, written bare before `:<state>`, read as another form or
/// could be taken for one.
fn grammar_word(slug: &str) -> bool {
    ["release", "check", "commit", "pr", "wt", "at"].contains(&slug)
        || slug.starts_with(['#', '+'])
        || slug.contains('+')
        || Delay::parse(slug).is_some()
        || !matches!(clock_of_day(slug), Ok(None))
}

fn parse_untimed(text: &str, spelling: Spelling) -> Result<Condition, String> {
    let named = |form: &str, rest: &str| {
        let state = rest.rsplit_once(':').map_or(rest, |(_, state)| state);
        if state == "gone" || RowState::parse(state).is_some() {
            format!("; a worktree named {form} is wt:{form}:{rest}")
        } else {
            String::new()
        }
    };
    // `pr:` before a digit is a pull request; before anything else it is a
    // worktree named `pr`.
    let pr = text.strip_prefix('#').or_else(|| {
        text.strip_prefix("pr:")
            .filter(|rest| rest.starts_with(|first: char| first.is_ascii_digit()))
    });
    if let Some(pr) = pr {
        let (number, goal) = match pr.split_once(':') {
            Some((number, goal)) => (number, Some(goal)),
            None => (pr, None),
        };
        let number = digits(number)
            .and_then(|number| u64::try_from(number).ok())
            .filter(|number| *number > 0)
            .ok_or_else(|| format!("`{number}` is not a pull request number"))?;
        let goal = match goal {
            None | Some("merged") => PrGoal::Merged,
            Some("closed") => PrGoal::Closed,
            Some(other) => PrGoal::Reached(parse_stage(other).ok_or_else(|| {
                format!(
                    "a pull request can be merged, staging, production or closed, not `{other}`"
                )
            })?),
        };
        return Ok(Condition::Pr { number, goal });
    }
    if let Some(stage) = text.strip_prefix("release:") {
        return parse_stage(stage).map(Condition::Release).ok_or_else(|| {
            format!(
                "a release is release:staging or release:production, not `{stage}`{}",
                named("release", stage)
            )
        });
    }
    if let Some(check) = text.strip_prefix("check:") {
        let (id, arg) = match check.split_once('=') {
            Some((id, arg)) => (id, Some(arg)),
            None => (check, None),
        };
        if id.is_empty() || arg == Some("") {
            return Err(
                "a check is check:<id> or check:<id>=<arg>, with neither part empty".into(),
            );
        }
        // The argument becomes one argv entry of the check's command, which
        // must not read it as an option of its own.
        if spelling == Spelling::Typed
            && let Some(arg) = arg
        {
            if arg.starts_with('-') {
                return Err(format!(
                    "a check's argument cannot start with `-`: the command would read `{arg}` as an option"
                ));
            }
            if arg.chars().any(char::is_control) {
                return Err("a check's argument cannot contain control characters".into());
            }
        }
        return Ok(Condition::Check {
            id: id.to_owned(),
            arg: arg.map(str::to_owned),
        });
    }
    if let Some(commit) = text.strip_prefix("commit:") {
        return commit
            .split_once(':')
            .filter(|(oid, _)| {
                (7..=64).contains(&oid.len()) && oid.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .and_then(|(oid, stage)| {
                Some(Condition::Commit {
                    oid: oid.to_owned(),
                    stage: parse_stage(stage)?,
                })
            })
            .ok_or_else(|| {
                format!(
                    "a commit is commit:<sha>:staging or commit:<sha>:production{}",
                    named("commit", commit)
                )
            });
    }
    let states =
        "merged, staging, production, ready, green, verified, closed, settled, reviewed or evicted";
    // `wt:` before `<slug>:<state>` names a worktree whatever its slug.
    // Before a state alone it is a worktree named `wt`.
    let explicit = text.strip_prefix("wt:").filter(|rest| rest.contains(':'));
    let Some((slug, state)) = explicit.unwrap_or(text).rsplit_once(':') else {
        return match text {
            "gone" => Err("`gone` needs a worktree: <slug>:gone".into()),
            _ => RowState::parse(text)
                .map(Condition::Own)
                .ok_or_else(|| format!("`{text}` is not a state: use {states}")),
        };
    };
    if slug.is_empty() || slug.contains(char::is_whitespace) {
        return Err(format!("`{slug}` is not a worktree"));
    }
    let cond = match state {
        "gone" => Condition::Gone {
            slug: slug.to_owned(),
        },
        _ => RowState::parse(state)
            .map(|state| Condition::Worktree {
                slug: slug.to_owned(),
                state,
            })
            .ok_or_else(|| format!("`{state}` is not a state: use {states}, or gone"))?,
    };
    if explicit.is_none() && spelling == Spelling::Typed && grammar_word(slug) {
        return Err(format!(
            "`{slug}` is also a word of the grammar, so `{text}` could mean two things; a worktree named {slug} is {cond}"
        ));
    }
    Ok(cond)
}

/// How times are written: at an offset, and relative to a day when one is
/// given: the reader's present, or the day a kept text is about.
#[derive(Clone, Copy)]
struct Local {
    utc_offset_minutes: i32,
    now_ms: Option<i64>,
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

impl Local {
    fn absolute(utc_offset_minutes: i32) -> Self {
        Self {
            utc_offset_minutes,
            now_ms: None,
        }
    }

    fn local(self, ms: i64) -> i64 {
        ms + i64::from(self.utc_offset_minutes) * MINUTE_MS
    }

    /// `YYYY-MM-DD HH:MM`, with seconds only when there are some.
    fn stamp(self, at_ms: i64) -> String {
        let local = self.local(at_ms);
        let (year, month, day) = civil_from_days(local.div_euclid(DAY_MS));
        let seconds = local.rem_euclid(MINUTE_MS) / SECOND_MS;
        let date = format!("{year:04}-{month:02}-{day:02} {}", self.clock(at_ms));
        if seconds == 0 {
            date
        } else {
            format!("{date}:{seconds:02}")
        }
    }

    fn clock(self, at_ms: i64) -> String {
        let of_day = self.local(at_ms).rem_euclid(DAY_MS);
        format!(
            "{:02}:{:02}",
            of_day / HOUR_MS,
            of_day % HOUR_MS / MINUTE_MS
        )
    }

    /// `17:00` on the reference day, `09:00 tomorrow`, else `Nov 26 17:00`,
    /// with the year when it is another one. Never seconds: this is read,
    /// not typed back.
    fn describe(self, at_ms: i64) -> String {
        let day = |ms| self.local(ms).div_euclid(DAY_MS);
        let clock = self.clock(at_ms);
        let (year, month, date) = civil_from_days(day(at_ms));
        let dated = format!(
            "{} {date} {clock}",
            MONTHS[(month - 1).clamp(0, 11) as usize]
        );
        match self.now_ms {
            Some(now_ms) if day(at_ms) == day(now_ms) => clock,
            Some(now_ms) if day(at_ms) == day(now_ms) + 1 => format!("{clock} tomorrow"),
            Some(now_ms) if civil_from_days(day(now_ms)).0 == year => dated,
            _ => format!("{dated}, {year}"),
        }
    }
}

/// Most characters of another worktree's slug in a condition's label.
const LABEL_SLUG_CHARS: usize = 12;

fn clipped(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        format!("{}…", text.chars().take(max - 1).collect::<String>())
    } else {
        text.to_owned()
    }
}

/// An instant in the fewest words that place it for a reader now: `22:32`
/// today, `Fri 18:12` within six days either way, else `Dec 2 09:24`.
pub fn terse_instant(at_ms: i64, clock: LocalClock) -> String {
    let local = Local {
        utc_offset_minutes: clock.utc_offset_minutes,
        now_ms: Some(clock.now_ms),
    };
    let day = |ms| local.local(ms).div_euclid(DAY_MS);
    let time = local.clock(at_ms);
    match day(at_ms) - day(clock.now_ms) {
        0 => time,
        -6..=6 => format!(
            "{} {time}",
            // 1970-01-01 was a Thursday.
            ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][day(at_ms).rem_euclid(7) as usize]
        ),
        _ => {
            let (_, month, date) = civil_from_days(day(at_ms));
            format!(
                "{} {date} {time}",
                MONTHS[(month - 1).clamp(0, 11) as usize]
            )
        }
    }
}

/// A countdown in its two coarsest units, one from three days out: `6d`,
/// `1d 21h`, `2h 5m`, `40m`.
pub fn countdown(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    match (days, hours, minutes) {
        (3.., _, _) | (1.., 0, _) => format!("{days}d"),
        (1.., _, _) => format!("{days}d {hours}h"),
        (0, 1.., 0) => format!("{hours}h"),
        (0, 1.., _) => format!("{hours}h {minutes}m"),
        (0, 0, 0) => "under 1m".to_owned(),
        _ => format!("{minutes}m"),
    }
}

/// An instant in words for a reader now: `17:00`, `09:00 tomorrow`,
/// `Nov 26 17:00`.
pub fn describe_instant(at_ms: i64, clock: LocalClock) -> String {
    Local {
        utc_offset_minutes: clock.utc_offset_minutes,
        now_ms: Some(clock.now_ms),
    }
    .describe(at_ms)
}

/// Conditions in words for a reader now, led by their connective:
/// `at 17:00` for one instant, `when merged and #3806 in production` for
/// anything else.
pub fn when_described(when: &[WatchCondition], clock: LocalClock) -> String {
    let words = when
        .iter()
        .map(|cond| cond.describe(clock))
        .collect::<Vec<_>>()
        .join(" and ");
    if matches!(when, [only] if matches!(only.cond, Condition::At { .. })) {
        format!("at {words}")
    } else {
        format!("when {words}")
    }
}

/// Whether kept text names one instant as [`Watch::summary`] writes it:
/// `17:00`, `09:00 tomorrow`, `Nov 26 17:00`, `Jan 2 09:00, 2027`, and
/// `tomorrow 09:00` as earlier builds wrote it. History keeps a watch's
/// words, not its conditions, and still reads "at" for a time.
pub fn is_time_phrase(text: &str) -> bool {
    let clock = |text: &str| {
        let bytes = text.as_bytes();
        bytes.len() == 5
            && bytes[2] == b':'
            && [0, 1, 3, 4].iter().all(|at| bytes[*at].is_ascii_digit())
    };
    let dated = |text: &str| {
        let mut words = text.splitn(3, ' ');
        let (month, day, time) = (words.next(), words.next(), words.next());
        month.is_some_and(|month| MONTHS.contains(&month))
            && day.is_some_and(|day| !day.is_empty() && day.bytes().all(|b| b.is_ascii_digit()))
            && time.is_some_and(|time| match time.split_once(", ") {
                Some((time, year)) => {
                    clock(time) && year.len() == 4 && year.bytes().all(|b| b.is_ascii_digit())
                }
                None => clock(time),
            })
    };
    clock(text)
        || text.strip_suffix(" tomorrow").is_some_and(clock)
        || text.strip_prefix("tomorrow ").is_some_and(clock)
        || dated(text)
}

/// Kept words for what a watch waits for, led by their connective:
/// `at 17:00` for a time, `when merged` for anything else.
pub fn when_phrase(what: &str) -> String {
    if is_time_phrase(what) {
        format!("at {what}")
    } else {
        format!("when {what}")
    }
}

/// The first line with anything on it of a prompt, a notification text or a
/// check's steps, cut to 80 cells. Stored text is often one line with its
/// numbered steps inline after ` STEPS:`; the headline stops before them,
/// so every surface names a check by what it proves.
pub fn headline(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let line = match line.find(" STEPS:") {
        Some(at) if !line[..at].trim().is_empty() => line[..at].trim_end(),
        _ => line,
    };
    if line.chars().count() > 80 {
        format!("{}…", line.chars().take(79).collect::<String>())
    } else {
        line.to_owned()
    }
}

/// The inverse of `days_from_civil` (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let doe = shifted.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let shifted_month = (5 * doy + 2) / 153;
    let day = doy - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

/// What the caller knows about one worktree. `None` is unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowFacts {
    /// A live worktree has this slug. `false` asserts it is gone; a slug
    /// whose presence is unknown has no entry at all.
    pub exists: bool,
    /// The live worktree's `createdAt`.
    pub incarnation: Option<String>,
    pub merged: Option<bool>,
    pub staging: Option<bool>,
    pub production: Option<bool>,
    pub ready: Option<bool>,
    pub green: Option<bool>,
    pub verified: Option<bool>,
    pub closed: Option<bool>,
    pub settled: Option<bool>,
    pub reviewed: Option<bool>,
    pub evicted: Option<bool>,
}

impl RowFacts {
    fn get(&self, state: RowState) -> Option<bool> {
        match state {
            RowState::Merged => self.merged,
            RowState::Staging => self.staging,
            RowState::Production => self.production,
            RowState::Ready => self.ready,
            RowState::Green => self.green,
            RowState::Verified => self.verified,
            RowState::Closed => self.closed,
            RowState::Settled => self.settled,
            RowState::Reviewed => self.reviewed,
            RowState::Evicted => self.evicted,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrLife {
    Open,
    Merged,
    Closed,
}

/// What the caller knows about a pull request named by number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrFacts {
    pub life: Option<PrLife>,
    pub merge_oid: Option<String>,
    pub staging: Option<bool>,
    pub production: Option<bool>,
}

/// A configured check as one worktree runs it. The owner is part of the
/// identity because a check's command may name the worktree.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CheckKey {
    pub slug: String,
    pub id: String,
    pub arg: Option<String>,
}

/// Everything a caller knows that watches may depend on. A missing entry is
/// unknown, and unknown never holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchFacts {
    pub rows: BTreeMap<String, RowFacts>,
    pub prs: BTreeMap<u64, PrFacts>,
    /// Whether a commit has reached a stage.
    pub commits: HashMap<(String, VerifyStage), bool>,
    /// What identifies each stage's current release: its deployed commit, or
    /// its branch tip where nothing reports deployments.
    pub releases: HashMap<VerifyStage, String>,
    /// `Ok(holds)`, or the probe's error.
    pub checks: BTreeMap<CheckKey, Result<bool, String>>,
}

/// The facts pending watches depend on that are not on the board.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    pub prs: BTreeSet<u64>,
    pub commits: Vec<(String, VerifyStage)>,
    pub releases: Vec<VerifyStage>,
    pub checks: BTreeSet<CheckKey>,
}

impl Needs {
    pub fn is_empty(&self) -> bool {
        self.prs.is_empty()
            && self.commits.is_empty()
            && self.releases.is_empty()
            && self.checks.is_empty()
    }
}

/// What a caller must gather to evaluate these watches, and nothing a met
/// watch or a settled condition no longer needs.
pub fn needs<'a>(watches: impl IntoIterator<Item = &'a Watch>) -> Needs {
    let mut needs = Needs::default();
    let mut commits = BTreeSet::new();
    let mut releases = BTreeSet::new();
    for watch in watches {
        if watch.is_met() {
            continue;
        }
        for cond in watch.when.iter().filter(|cond| !cond.latched) {
            match &cond.cond {
                Condition::Pr { number, .. } => {
                    needs.prs.insert(*number);
                }
                Condition::Commit { oid, stage } => {
                    commits.insert((oid.clone(), stage.as_str()));
                }
                Condition::Release(stage) => {
                    releases.insert(stage.as_str());
                }
                Condition::Check { id, arg } => {
                    needs.checks.insert(CheckKey {
                        slug: watch.slug.clone(),
                        id: id.clone(),
                        arg: arg.clone(),
                    });
                }
                Condition::At { .. }
                | Condition::Own(_)
                | Condition::Worktree { .. }
                | Condition::Gone { .. }
                | Condition::Unknown(_) => {}
            }
        }
    }
    needs.commits = commits
        .into_iter()
        .filter_map(|(oid, stage)| Some((oid, parse_stage(stage)?)))
        .collect();
    needs.releases = releases.into_iter().filter_map(parse_stage).collect();
    needs
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Truth {
    Holds,
    NotYet,
    /// Not known now, and why. Never treated as holding or as not holding.
    Unknown(String),
}

impl Truth {
    fn of(holds: bool) -> Self {
        if holds { Self::Holds } else { Self::NotYet }
    }

    fn known(fact: Option<bool>, unknown: impl FnOnce() -> String) -> Self {
        fact.map_or_else(|| Self::Unknown(unknown()), Self::of)
    }
}

/// One condition's new record, tied to the condition it was computed for.
#[derive(Clone, Debug, PartialEq)]
pub struct ConditionStamp {
    pub index: usize,
    /// The condition evaluated. The stamp applies only while the watch
    /// still holds it at `index`.
    pub was: Condition,
    pub now: WatchCondition,
}

/// What an evaluation changes in a watch's durable record.
#[derive(Clone, Debug, PartialEq)]
pub struct WatchStamps {
    pub id: String,
    pub conditions: Vec<ConditionStamp>,
    /// Every condition held at this instant.
    pub met_at_ms: Option<i64>,
    /// The new `last_error`, when it differs from the stored one.
    pub last_error: Option<Option<String>>,
    /// The owner's landing commit, when the record does not have it yet.
    pub landing_commit: Option<String>,
    /// The minute of this evaluation, when the record shows none or one
    /// more than [`EVALUATED_EVERY_MS`] old.
    pub evaluated_at_ms: Option<i64>,
}

/// What [`WatchStamps::apply`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub changed: bool,
    /// This application is the one that marked the watch met.
    pub newly_met: bool,
}

impl WatchStamps {
    /// Stamps that change nothing yet.
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            conditions: Vec::new(),
            met_at_ms: None,
            last_error: None,
            landing_commit: None,
            evaluated_at_ms: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.conditions.is_empty()
            && self.met_at_ms.is_none()
            && self.last_error.is_none()
            && self.landing_commit.is_none()
            && self.evaluated_at_ms.is_none()
    }

    /// The minute to record an evaluation at `now_ms` under, unless the
    /// watch already shows one from the last [`EVALUATED_EVERY_MS`].
    pub fn evaluation_due(watch: &Watch, now_ms: i64) -> Option<i64> {
        let minute = now_ms - now_ms.rem_euclid(MINUTE_MS);
        watch
            .evaluated_at_ms()
            .is_none_or(|at| at.saturating_add(EVALUATED_EVERY_MS) < minute)
            .then_some(minute)
    }

    /// Write the stamps into `watch`. A watch that is already met keeps its
    /// time, and a condition that was replaced since keeps its record.
    pub fn apply(&self, watch: &mut Watch) -> Applied {
        let mut applied = Applied::default();
        if watch.id != self.id {
            return applied;
        }
        let mut stale = false;
        for stamp in &self.conditions {
            match watch.when.get_mut(stamp.index) {
                Some(cond) if cond.cond == stamp.was => {
                    if *cond != stamp.now {
                        *cond = stamp.now.clone();
                        applied.changed = true;
                    }
                }
                _ => stale = true,
            }
        }
        if let Some(error) = &self.last_error
            && watch.last_error != *error
        {
            watch.last_error.clone_from(error);
            applied.changed = true;
        }
        if let Some(oid) = &self.landing_commit
            && watch.landing_commit() != Some(oid)
        {
            watch
                .extra
                .insert(LANDING_COMMIT.to_owned(), Value::String(oid.clone()));
            applied.changed = true;
        }
        if let Some(at) = self.evaluated_at_ms
            && watch.evaluated_at_ms().is_none_or(|stored| stored < at)
        {
            watch.extra.insert(EVALUATED_AT.to_owned(), at.into());
            applied.changed = true;
        }
        // A stale stamp means "met" was judged on other conditions.
        if let Some(met_at_ms) = self.met_at_ms
            && !stale
            && watch.met_at_ms.is_none()
        {
            watch.met_at_ms = Some(met_at_ms);
            applied.changed = true;
            applied.newly_met = true;
        }
        applied
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Evaluation {
    /// One per condition, in the watch's order.
    pub truths: Vec<Truth>,
    /// The watch is met: it already was, or every condition holds now.
    pub met: bool,
    /// What to store, when anything changed.
    pub stamps: Option<WatchStamps>,
}

/// Judge a watch against what the caller knows. Every condition must hold;
/// an unknown one never does, and a watch once met stays met.
pub fn evaluate(watch: &Watch, facts: &WatchFacts, now_ms: i64) -> Evaluation {
    let mut truths = Vec::with_capacity(watch.when.len());
    let mut conditions = Vec::new();
    let mut error = None;
    for (index, cond) in watch.when.iter().enumerate() {
        let mut next = cond.clone();
        let observed = if cond.latched {
            Truth::Holds
        } else {
            observe(watch, &mut next, facts, now_ms, &mut error)
        };
        let truth = match observed {
            Truth::Holds => {
                let since = *next.first_held_at_ms.get_or_insert(now_ms);
                let delay = next.delay.map_or(0, Delay::as_ms);
                Truth::of(now_ms >= since.saturating_add(delay))
            }
            Truth::NotYet => {
                next.first_held_at_ms = None;
                Truth::NotYet
            }
            unknown @ Truth::Unknown(_) => unknown,
        };
        if next != *cond {
            conditions.push(ConditionStamp {
                index,
                was: cond.cond.clone(),
                now: next,
            });
        }
        truths.push(truth);
    }
    if watch.is_met() {
        return Evaluation {
            truths,
            met: true,
            stamps: None,
        };
    }
    let met = !truths.is_empty() && truths.iter().all(|truth| *truth == Truth::Holds);
    let last_error = (watch.last_error != error).then_some(error);
    let stamps = (met || !conditions.is_empty() || last_error.is_some()).then(|| WatchStamps {
        conditions,
        met_at_ms: met.then_some(now_ms),
        last_error,
        ..WatchStamps::new(&watch.id)
    });
    Evaluation {
        truths,
        met,
        stamps,
    }
}

/// Whether the condition's subject holds now, before any delay. May replace
/// the condition with a commit-based one, record a baseline, or latch.
fn observe(
    watch: &Watch,
    next: &mut WatchCondition,
    facts: &WatchFacts,
    now_ms: i64,
    error: &mut Option<String>,
) -> Truth {
    let commit = |oid: &str, stage: VerifyStage| {
        Truth::known(facts.commits.get(&(oid.to_owned(), stage)).copied(), || {
            format!(
                "not known whether {} reached {}",
                short(oid),
                stage.as_str()
            )
        })
    };
    let no_data =
        |subject: &dyn fmt::Display| Truth::Unknown(format!("no current data for {subject}"));
    match next.cond.clone() {
        Condition::At { at_ms } => Truth::of(now_ms >= at_ms),
        Condition::Own(state) => match facts.rows.get(&watch.slug) {
            None => no_data(&"this worktree"),
            Some(row) if !row.exists || !watch.owned_by(row.incarnation.as_deref()) => {
                Truth::Unknown("its worktree is gone".into())
            }
            Some(row) => Truth::known(row.get(state), || {
                format!("not known whether this worktree is {}", state.as_str())
            }),
        },
        Condition::Worktree { slug, state } => match facts.rows.get(&slug) {
            None => no_data(&slug),
            Some(row) => Truth::known(row.get(state), || {
                format!("not known whether {slug} is {}", state.as_str())
            }),
        },
        Condition::Gone { slug } => match facts.rows.get(&slug) {
            None => no_data(&slug),
            Some(row) => Truth::of(!row.exists),
        },
        Condition::Pr { number, goal } => {
            let subject = format!("#{number}");
            let Some(pr) = facts.prs.get(&number) else {
                return no_data(&subject);
            };
            let Some(life) = pr.life else {
                return no_data(&subject);
            };
            match goal {
                PrGoal::Merged => Truth::of(life == PrLife::Merged),
                PrGoal::Closed => Truth::of(life == PrLife::Closed),
                PrGoal::Reached(_) if life != PrLife::Merged => Truth::NotYet,
                PrGoal::Reached(stage) => {
                    let reached = match stage {
                        VerifyStage::Staging => pr.staging,
                        VerifyStage::Production => pr.production,
                    };
                    let Some(oid) = &pr.merge_oid else {
                        return Truth::known(reached, || {
                            format!("the merge commit of {subject} is not known")
                        });
                    };
                    // The merge commit says everything the PR still could,
                    // so the PR itself is no longer asked.
                    next.cond = Condition::Commit {
                        oid: oid.clone(),
                        stage,
                    };
                    reached.map_or_else(|| commit(oid, stage), Truth::of)
                }
            }
        }
        Condition::Commit { oid, stage } => commit(&oid, stage),
        Condition::Release(stage) => {
            let Some(marker) = facts.releases.get(&stage) else {
                return Truth::Unknown(format!(
                    "the current {} release is not known",
                    stage.as_str()
                ));
            };
            match &next.baseline {
                None => {
                    next.baseline = Some(marker.clone());
                    Truth::NotYet
                }
                Some(baseline) if baseline == marker => Truth::NotYet,
                // A release is an event: once seen it has happened, whatever
                // is deployed later.
                Some(_) => {
                    next.latched = true;
                    Truth::Holds
                }
            }
        }
        Condition::Check { id, arg } => {
            let key = CheckKey {
                slug: watch.slug.clone(),
                id,
                arg,
            };
            match facts.checks.get(&key) {
                None => Truth::Unknown("the check has not run yet".into()),
                Some(Ok(holds)) => Truth::of(*holds),
                Some(Err(problem)) => {
                    error.get_or_insert_with(|| problem.clone());
                    Truth::Unknown(problem.clone())
                }
            }
        }
        Condition::Unknown(text) => {
            Truth::Unknown(format!("this build does not understand `{text}`"))
        }
    }
}

#[cfg(test)]
mod tests;
