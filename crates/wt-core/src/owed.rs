//! What a worktree still owes: one list, derived here and nowhere else.
//!
//! An item is a gate (what blocks merging), a check (a post-merge
//! verification owed in a stage) or a step (anything else the worktree comes
//! back for). It is waiting or due. The board, the details pane, cleanup,
//! removal hazards and command output all read [`owed_items`]; nothing else
//! decides what a row owes.
//!
//! A wake that is delivered after its worktree's own work landed does not
//! vanish: it becomes a due step, kept as an [`OwedRecord`] until it is
//! closed with evidence. Watches that only tell the person are not owed by
//! the row and never appear here.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    Landing, VerifyStage, WorkStatusRecord, check_standings,
    watch::{self, Condition, LocalClock, RowState, Watch, WatchCondition},
};

/// Due steps one worktree may hold. Past it the oldest is dropped.
pub const OWED_PER_SLUG_MAX: usize = 20;

/// How every removal hazard about something owed starts.
pub const OWED_HAZARD: &str = "still owes";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OwedKind {
    /// The merge gate. Cleared by wt when its conditions hold, or by a
    /// person.
    Gate,
    /// A post-merge verification owed in a stage.
    Check,
    /// Anything else the worktree comes back for.
    Step,
}

impl OwedKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gate => "gate",
            Self::Check => "check",
            Self::Step => "step",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OwedState {
    Waiting,
    Due,
}

impl OwedState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Due => "due",
        }
    }
}

/// Where a landed row stands with what it owes. Derived, never asserted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OwedPhase {
    /// Something is due.
    Due,
    /// Something is open and nothing is due.
    Waiting,
    /// Nothing is open: the row can be cleaned up.
    Done,
}

impl OwedPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Due => "due",
            Self::Waiting => "waiting",
            Self::Done => "done",
        }
    }
}

/// Whether a wake's worktree had landed its own work when the wake was
/// delivered. Decided when the fire is evaluated and handed to the store by
/// both the delivery path and the heal path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerLanding {
    Landed,
    /// The wake is coordination before landing (rebase when X merges, test
    /// at 5pm): delivering it closes it.
    NotLanded,
    /// A PR exists, the hosting data is stale and nothing local proves
    /// either way. Counts as landed: the step must be closed.
    Unknown,
}

impl OwnerLanding {
    /// Whether a wake delivered under this landing becomes a due step.
    pub const fn owes(self) -> bool {
        !matches!(self, Self::NotLanded)
    }

    /// From what is known of the base landing; `None` is unknown.
    pub const fn from_base(landed: Option<bool>) -> Self {
        match landed {
            Some(true) => Self::Landed,
            Some(false) => Self::NotLanded,
            None => Self::Unknown,
        }
    }
}

/// A step that came due: a wake delivered to a worktree whose work had
/// landed. Stored in the state's `"owed"` array until it is closed, and on
/// the removal record (`removed[].owed`) once its worktree is removed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwedRecord {
    /// The id of the wake watch it came from; what `wt done <id>` takes.
    pub id: String,
    pub slug: String,
    /// The owner's `createdAt` when the wake was set; see
    /// [`Watch::incarnation`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incarnation: Option<String>,
    /// Who set the wake.
    pub by: String,
    /// The wake's prompt.
    pub what: String,
    /// The conditions that held, in words.
    pub when_text: String,
    pub due_at_ms: i64,
    /// The setter's local UTC offset, for a wake set again from this step.
    #[serde(default)]
    pub utc_offset_minutes: i32,
    /// The wake was sent without proof it reached the session.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unconfirmed: bool,
    /// It must happen before the next production release.
    #[serde(default, skip_serializing_if = "is_false")]
    pub before_release: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl OwedRecord {
    /// The due step a delivered wake becomes. `None` when the watch owes no
    /// wake.
    pub fn from_wake(watch: &Watch, due_at_ms: i64, unconfirmed: bool) -> Option<Self> {
        Some(Self {
            id: watch.id.clone(),
            slug: watch.slug.clone(),
            incarnation: watch.incarnation.clone(),
            by: watch.by.clone(),
            what: watch.then.wake.clone()?,
            when_text: watch.summary_met(),
            due_at_ms,
            utc_offset_minutes: watch.utc_offset_minutes,
            unconfirmed,
            before_release: watch.before_release(),
            extra: Map::new(),
        })
    }

    /// Whether `created_at` names the worktree this step belongs to; see
    /// [`Watch::owned_by`].
    pub fn owned_by(&self, created_at: Option<&str>) -> bool {
        self.incarnation.as_deref() == created_at
    }
}

/// What an item waits for, as far as display needs it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "on")]
pub enum OwedWhen {
    /// A gate with no watch: a person clears it.
    Person,
    /// A watch's conditions, all of which must hold.
    Conditions { when: Vec<WatchCondition> },
    /// A check: the change reaching its stage, and not before a time when
    /// it was put off.
    Stage {
        stage: VerifyStage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        not_before_ms: Option<i64>,
    },
    /// A due step: the conditions that held, in the words kept of them.
    Held { what: String },
}

/// One thing a worktree owes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwedItem {
    /// A step's id (its wake watch's), or a gate's watch id. `None` for a
    /// check, which is named by its stage, and for a gate nothing watches.
    pub id: Option<String>,
    pub kind: OwedKind,
    pub state: OwedState,
    /// A check's stage, or the stage of the row's own a step waits for.
    pub stage: Option<VerifyStage>,
    /// The gate text, the check's steps or the step's prompt, in full.
    pub what: String,
    pub when: OwedWhen,
    /// Who set a step. `None` for a check or a gate, which the status
    /// record's author set.
    pub by: Option<String>,
    pub before_release: bool,
    /// A check that has been due for days.
    pub overdue: bool,
    /// A check landed in Git on a probed stage that is not running the
    /// change yet.
    pub awaiting_deploy: bool,
    /// A due step whose wake was sent without proof it arrived.
    pub unconfirmed: bool,
    /// When a step came due.
    pub due_at_ms: Option<i64>,
}

impl OwedItem {
    /// One line naming the item: the first line of a gate or a step, and
    /// `check: <first line of the steps>`.
    pub fn headline(&self) -> String {
        match self.kind {
            OwedKind::Check => format!("check: {}", watch::headline(&self.what)),
            OwedKind::Gate | OwedKind::Step => watch::headline(&self.what),
        }
    }

    /// When the item is due or cleared, in words for a reader now:
    /// `due since 11:36`, `at 17:00`, `when #3806 in production`,
    /// `due in staging`, `awaiting production deploy`, `once in
    /// production, not before Nov 27 18:12`, `until a person clears it`.
    pub fn when_words(&self, clock: LocalClock) -> String {
        match &self.when {
            OwedWhen::Person => "until a person clears it".into(),
            OwedWhen::Conditions { when } if self.kind == OwedKind::Gate => {
                format!("clears {}", watch::when_described(when, clock))
            }
            OwedWhen::Conditions { when } => watch::when_described(when, clock),
            OwedWhen::Stage {
                stage,
                not_before_ms,
            } => {
                let stage = stage.as_str();
                if self.state == OwedState::Due {
                    return if self.overdue {
                        format!("overdue in {stage}")
                    } else {
                        format!("due in {stage}")
                    };
                }
                let waits = if self.awaiting_deploy {
                    format!("awaiting {stage} deploy")
                } else {
                    format!("once in {stage}")
                };
                match not_before_ms.filter(|at| *at > clock.now_ms) {
                    Some(at) => {
                        format!("{waits}, not before {}", watch::describe_instant(at, clock))
                    }
                    None => waits,
                }
            }
            OwedWhen::Held { .. } => match self.due_at_ms {
                Some(at) => format!("due since {}", watch::describe_instant(at, clock)),
                None => "due".into(),
            },
        }
    }

    /// What `wt done` takes to close the item: a step's id or a check's
    /// stage. `None` for a gate and for a step that is still waiting.
    pub fn done_target(&self) -> Option<String> {
        match self.kind {
            OwedKind::Check => self.stage.map(|stage| stage.as_str().to_owned()),
            OwedKind::Step if self.state == OwedState::Due => self.id.clone(),
            OwedKind::Step | OwedKind::Gate => None,
        }
    }
}

/// The watches in `watches` that belong to the worktree `slug` created at
/// `created_at` and still follow it.
pub fn row_watches(watches: &[Watch], slug: &str, created_at: Option<&str>) -> Vec<Watch> {
    watches
        .iter()
        .filter(|watch| watch.slug == slug && !watch.owner_gone && watch.owned_by(created_at))
        .cloned()
        .collect()
}

/// The due steps in `due` that belong to the worktree `slug` created at
/// `created_at`.
pub fn row_due(due: &[OwedRecord], slug: &str, created_at: Option<&str>) -> Vec<OwedRecord> {
    due.iter()
        .filter(|step| step.slug == slug && step.owned_by(created_at))
        .cloned()
        .collect()
}

/// The stage of the row's own a watch waits for, the later one when it
/// names both.
fn own_stage(watch: &Watch) -> Option<VerifyStage> {
    watch
        .when
        .iter()
        .filter_map(|cond| match cond.cond {
            Condition::Own(RowState::Staging) => Some(VerifyStage::Staging),
            Condition::Own(RowState::Production) => Some(VerifyStage::Production),
            _ => None,
        })
        .max_by_key(|stage| *stage == VerifyStage::Production)
}

/// Everything one worktree owes. `watches` and `due` must be this
/// worktree's own (see [`row_watches`] and [`row_due`]); watches that only
/// tell the person are left out.
///
/// Order: what is due first (checks by stage, then steps, oldest first),
/// then the gate, then what waits: for staging, for production, for
/// anything else, and last for a time, soonest first.
pub fn owed_items(
    record: Option<&WorkStatusRecord>,
    landing: &Landing,
    watches: &[Watch],
    due: &[OwedRecord],
    now_ms: i64,
) -> Vec<OwedItem> {
    let item = |kind, state, what: &str, when| OwedItem {
        id: None,
        kind,
        state,
        stage: None,
        what: what.to_owned(),
        when,
        by: None,
        before_release: false,
        overdue: false,
        awaiting_deploy: false,
        unconfirmed: false,
        due_at_ms: None,
    };
    let mut items = Vec::new();
    // Each waiting item with what it sorts by: what it waits for, then when.
    let mut waiting: Vec<((u8, i64), OwedItem)> = Vec::new();
    let stage_rank = |stage: Option<VerifyStage>| match stage {
        Some(VerifyStage::Staging) => 0,
        Some(VerifyStage::Production) => 1,
        None => 2,
    };
    for (check, standing) in check_standings(record, *landing, now_ms) {
        let mut owed = item(
            OwedKind::Check,
            if standing.due {
                OwedState::Due
            } else {
                OwedState::Waiting
            },
            &check.steps,
            OwedWhen::Stage {
                stage: standing.stage,
                not_before_ms: check.not_before_ms,
            },
        );
        owed.stage = Some(standing.stage);
        owed.overdue = standing.overdue;
        owed.awaiting_deploy = standing.awaiting_deploy;
        if standing.due {
            items.push(owed);
        } else if let Some(at) = check
            .not_before_ms
            .filter(|_| standing.deferred && landing.base && landing.reached(standing.stage))
        {
            // Its stage has the change: only the time holds it back.
            waiting.push(((3, at), owed));
        } else {
            waiting.push(((stage_rank(owed.stage), 0), owed));
        }
    }
    let mut steps: Vec<&OwedRecord> = due.iter().collect();
    steps.sort_by_key(|step| step.due_at_ms);
    for step in steps {
        let mut owed = item(
            OwedKind::Step,
            OwedState::Due,
            &step.what,
            OwedWhen::Held {
                what: step.when_text.clone(),
            },
        );
        owed.id = Some(step.id.clone());
        owed.by = Some(step.by.clone());
        owed.before_release = step.before_release;
        owed.unconfirmed = step.unconfirmed;
        owed.due_at_ms = Some(step.due_at_ms);
        items.push(owed);
    }
    let gate = record
        .filter(|record| !record.is_terminal())
        .and_then(|record| record.blocked_on.as_deref())
        .filter(|gate| !gate.is_empty());
    let clears_gate = |watch: &Watch| {
        gate.is_some() && watch.then.unblock.as_deref() == gate && !watch.owner_gone
    };
    if let Some(gate) = gate {
        let watch = watches.iter().find(|watch| clears_gate(watch));
        let mut owed = item(
            OwedKind::Gate,
            OwedState::Waiting,
            gate,
            match watch {
                Some(watch) => OwedWhen::Conditions {
                    when: watch.when.clone(),
                },
                None => OwedWhen::Person,
            },
        );
        owed.id = watch.map(|watch| watch.id.clone());
        items.push(owed);
    }
    for watch in watches {
        let Some(prompt) = watch.then.wake.as_deref().filter(|_| watch.wakes_owner()) else {
            continue;
        };
        // The wake that follows a gate is part of the gate's item.
        if clears_gate(watch) {
            continue;
        }
        let mut owed = item(
            OwedKind::Step,
            OwedState::Waiting,
            prompt,
            OwedWhen::Conditions {
                when: watch.when.clone(),
            },
        );
        owed.id = Some(watch.id.clone());
        owed.stage = own_stage(watch);
        owed.by = Some(watch.by.clone());
        owed.before_release = watch.before_release();
        let times = watch.when.iter().map(|cond| match cond.cond {
            Condition::At { at_ms } => Some(at_ms),
            _ => None,
        });
        let rank = match times.collect::<Option<Vec<i64>>>() {
            Some(times) if owed.stage.is_none() => (3, times.into_iter().max().unwrap_or(0)),
            _ => (stage_rank(owed.stage), 0),
        };
        waiting.push((rank, owed));
    }
    waiting.sort_by_key(|(rank, _)| *rank);
    items.extend(waiting.into_iter().map(|(_, owed)| owed));
    items
}

/// Where a row stands with what it owes. `None` while its own work has not
/// landed: until then its asserted status says where it stands.
pub fn owed_phase(items: &[OwedItem], landed: bool) -> Option<OwedPhase> {
    if !landed {
        return None;
    }
    Some(if items.iter().any(|item| item.state == OwedState::Due) {
        OwedPhase::Due
    } else if items.is_empty() {
        OwedPhase::Done
    } else {
        OwedPhase::Waiting
    })
}

/// Whether removing the worktree would lose something it owes: a check not
/// yet discharged (due or not), a wake still owed to its session, or a step
/// that came due and was not closed. This is the one cleanup predicate. A
/// merge gate alone is not owed after landing and holds nothing.
/// `watches` and `due` must be this worktree's own.
pub fn owes_anything(
    record: Option<&WorkStatusRecord>,
    watches: &[Watch],
    due: &[OwedRecord],
) -> bool {
    record.is_some_and(|record| !record.open_checks().is_empty())
        || watches.iter().any(Watch::wakes_owner)
        || !due.is_empty()
}

/// One removal hazard per thing [`owes_anything`] counts, each reading
/// `still owes: <headline> (<when in words>)`. Empty exactly when
/// [`owes_anything`] is false. The only source of that text.
///
/// A hazard says what removal would lose, not how urgent it is: a check
/// reads `in staging` wherever its landing stands, so the text a removal
/// was confirmed against does not change when the branch lands.
pub fn owed_hazards(
    record: Option<&WorkStatusRecord>,
    watches: &[Watch],
    due: &[OwedRecord],
    clock: LocalClock,
) -> Vec<String> {
    let wakes = |id: Option<&str>| {
        watches
            .iter()
            .any(|watch| Some(watch.id.as_str()) == id && watch.wakes_owner())
    };
    owed_items(record, &Landing::NONE, watches, due, clock.now_ms)
        .iter()
        .filter(|item| item.kind != OwedKind::Gate || wakes(item.id.as_deref()))
        .map(|item| {
            let when = match (&item.when, item.stage) {
                (OwedWhen::Stage { .. }, Some(stage)) => item.when_words(clock).replacen(
                    &format!("once in {}", stage.as_str()),
                    &format!("in {}", stage.as_str()),
                    1,
                ),
                _ => item.when_words(clock),
            };
            format!("{OWED_HAZARD}: {} ({when})", item.headline())
        })
        .collect()
}

#[cfg(test)]
mod tests;
