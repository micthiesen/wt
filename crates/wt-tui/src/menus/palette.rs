//! `!`, `M`, and the slot palettes, built from the board. The owning host's
//! action catalog comes from its configuration ([`ActionCatalog`] in its
//! `DisplayPolicy`), its action source publishes what changes while it runs
//! ([`ActionState`], each row's [`ActionFacts`]), and [`blocker`] is the one
//! rule both this builder and the host's commit check apply.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{LocalMenu, Menu};
use crate::{
    ActionSurface, Board, BoardRow, ConfirmAction, DisplayPolicy, PickerAction, PickerOption, Seen,
    SessionTarget, TextAction, UiModal,
};

/// The freeform prompt every palette ends with.
pub const CUSTOM_ACTION_ID: &str = "__custom__";
/// Slot palette `z`: open the slot's checkout in the editor.
pub const OPEN_EDITOR_ACTION_ID: &str = "__open_editor__";
/// Row palette `l`: show the dev server's output.
pub const DEV_LOGS_ACTION_ID: &str = "__dev_logs__";

/// A host's action definitions as its palettes list them. Configuration
/// only: the prompt and command bodies stay on the host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionCatalog {
    pub row: Vec<ActionEntry>,
    pub manager: Vec<ActionEntry>,
    pub slot: Vec<ActionEntry>,
    /// `[ui] action_groups_last`: groups listed after the others, in order.
    pub groups_last: Vec<String>,
    /// A dev server is configured, so row palettes offer its logs.
    pub dev_server: bool,
}

impl ActionCatalog {
    pub fn entries(&self, surface: &ActionSurface) -> &[ActionEntry] {
        match surface {
            ActionSurface::Row { .. } => &self.row,
            ActionSurface::Manager { .. } => &self.manager,
            ActionSurface::Slot { .. } => &self.slot,
        }
    }

    pub fn entry(&self, surface: &ActionSurface, id: &str) -> Option<&ActionEntry> {
        self.entries(surface).iter().find(|entry| entry.id == id)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionEntry {
    pub id: String,
    pub name: String,
    pub group: Option<String>,
    /// The configured chord, before reserved and taken letters are resolved.
    pub key: Option<String>,
    /// A shell command; otherwise a prompt for an agent.
    pub shell: bool,
    pub target: ActionDelivery,
    pub requires: Vec<ActionRequirement>,
    /// Label of the value the action asks for before it runs.
    pub arg_prompt: Option<String>,
    /// Runs as chosen, without the extra-instructions prompt.
    pub direct: bool,
    /// Needs no selected row on the manager palette.
    pub fleet: bool,
}

impl ActionEntry {
    /// Its run names the row's pull request.
    pub fn needs_pr(&self) -> bool {
        self.requires.iter().any(|requirement| {
            matches!(
                requirement,
                ActionRequirement::Pr | ActionRequirement::PrReady
            )
        })
    }
}

/// Where an action's prompt or command goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionDelivery {
    #[default]
    Headless,
    Session,
    Manager,
    Slot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionRequirement {
    Pr,
    PrReady,
    Deployed,
    IssueTracker,
}

/// What a row's actions depend on, as its host would resolve them for a
/// dispatch right now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionFacts {
    pub pr: PrFact,
    /// A stage of the configured namespace is deployed from the checkout.
    pub deployed: bool,
    /// The row has an issue id the configured tracker accepts.
    pub tracker_id: bool,
    /// The tracked action running in the checkout, which the palette offers
    /// to stop instead of listing actions.
    pub active_run: Option<ActiveRunView>,
}

/// What a live GitHub read recent enough for a write says about the row's
/// branch. Restored data, an old read and a read that did not cover the
/// branch are all `Unknown`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrFact {
    #[default]
    Unknown,
    Absent,
    Present {
        open: bool,
        draft: bool,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActiveRunView {
    /// The action slot the run holds on its host.
    pub action_key: String,
    pub run_id: String,
    pub action_name: String,
}

/// A value an action was run with before.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArgValue {
    /// Sent back unchanged when chosen.
    pub value: String,
    /// The value as the terminal shows it.
    pub shown: String,
    /// What an earlier run called the value.
    pub label: Option<String>,
}

/// What a host's action source publishes for its palettes beyond the rows'
/// own facts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionState {
    /// Recent values by action id, newest first.
    pub recent_args: BTreeMap<String, Vec<ArgValue>>,
    /// Active tracked runs by action slot.
    pub runs: BTreeMap<String, ActiveRunView>,
    /// Why a slot's checkout cannot run anything, by slot name.
    pub slots_blocked: BTreeMap<String, String>,
}

/// What the palette showed for the commit's target. The host refuses the
/// commit as stale when its own state differs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionSeen {
    /// The checkout behind the surface's row. `None` without a row.
    pub row: Option<ActionRowSeen>,
    /// The pull request shown for the row, for an action that requires one.
    pub pr: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionRowSeen {
    pub path: String,
    pub branch: String,
}

/// Why an action cannot run for a surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocker {
    NoRow,
    /// No live GitHub read covers the branch yet; a read can lift this.
    PrUnconfirmed,
    NoPr,
    PrDraft,
    PrNotOpen,
    NotDeployed,
    NoTrackerId,
}

impl Blocker {
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoRow => "no row selected",
            Self::PrUnconfirmed => "GitHub data is not confirmed",
            Self::NoPr => "no PR",
            Self::PrDraft => "PR is draft",
            Self::PrNotOpen => "PR not open",
            Self::NotDeployed => "no stage deployed",
            Self::NoTrackerId => "no tracker id (set one with `#`)",
        }
    }
}

/// The first reason `entry` cannot run on `surface`, given the facts of the
/// surface's row. The palette lists the entry with this reason and the host
/// refuses its commit for the same one.
pub fn blocker(
    entry: &ActionEntry,
    surface: &ActionSurface,
    facts: &ActionFacts,
) -> Option<Blocker> {
    if matches!(surface, ActionSurface::Manager { key: None })
        && !entry.fleet
        && entry.id != CUSTOM_ACTION_ID
    {
        return Some(Blocker::NoRow);
    }
    entry
        .requires
        .iter()
        .find_map(|requirement| match requirement {
            ActionRequirement::Pr => match facts.pr {
                PrFact::Unknown => Some(Blocker::PrUnconfirmed),
                PrFact::Absent => Some(Blocker::NoPr),
                PrFact::Present { .. } => None,
            },
            ActionRequirement::PrReady => match facts.pr {
                PrFact::Unknown => Some(Blocker::PrUnconfirmed),
                PrFact::Absent => Some(Blocker::NoPr),
                PrFact::Present { draft: true, .. } => Some(Blocker::PrDraft),
                PrFact::Present { open: false, .. } => Some(Blocker::PrNotOpen),
                PrFact::Present { .. } => None,
            },
            ActionRequirement::Deployed => (!facts.deployed).then_some(Blocker::NotDeployed),
            ActionRequirement::IssueTracker => (!facts.tracker_id).then_some(Blocker::NoTrackerId),
        })
}

/// The name a slot's palette and action slot go by.
pub fn slot_name(target: SessionTarget) -> &'static str {
    match target {
        SessionTarget::WtSource => "wt",
        SessionTarget::Main => "main",
        SessionTarget::Manager => "manager",
        SessionTarget::Dotfiles => "dotfiles",
        _ => "slot",
    }
}

/// Everything one surface's palette is built from.
pub(crate) struct Scope<'a> {
    pub display: &'a DisplayPolicy,
    pub state: &'a ActionState,
    /// The row a row or keyed manager surface names.
    pub row: Option<&'a BoardRow>,
}

impl<'a> Scope<'a> {
    /// `None` when the surface names a row the board does not list.
    pub(crate) fn of(board: &'a Board, surface: &ActionSurface) -> Option<Self> {
        let key = match surface {
            ActionSurface::Row { key } => Some(key),
            ActionSurface::Manager { key } => key.as_ref(),
            ActionSurface::Slot { .. } => None,
        };
        let row = match key {
            Some(key) => Some(board.rows.iter().find(|row| &row.key == key)?),
            None => None,
        };
        // A remote row's palette lists its own host's actions.
        let (display, state) = match row.and_then(|row| row.host.as_ref()) {
            Some(host) => (
                board.host_display.get(host).unwrap_or(&EMPTY_DISPLAY),
                board.host_actions.get(host).unwrap_or(&EMPTY_STATE),
            ),
            None => (&board.display, &board.actions),
        };
        Some(Self {
            display,
            state,
            row,
        })
    }

    pub(crate) fn entries(&self, surface: &ActionSurface) -> &'a [ActionEntry] {
        self.display.actions.entries(surface)
    }

    pub(crate) fn facts(&self) -> &'a ActionFacts {
        self.row.map_or(&NO_FACTS, |row| &row.action_facts)
    }

    /// Why nothing can run on a slot surface whose checkout is unavailable.
    pub(crate) fn slot_blocked(&self, surface: &ActionSurface) -> Option<&'a str> {
        let ActionSurface::Slot { target } = surface else {
            return None;
        };
        self.state
            .slots_blocked
            .get(slot_name(*target))
            .map(String::as_str)
    }

    /// The tracked run active in the surface's action slot.
    pub(crate) fn active_run(&self, surface: &ActionSurface) -> Option<&'a ActiveRunView> {
        match (surface, self.row) {
            (_, Some(row)) => row.action_facts.active_run.as_ref(),
            (ActionSurface::Manager { .. }, None) => self.state.runs.get("manager"),
            (ActionSurface::Slot { target }, None) => self.state.runs.get(slot_name(*target)),
            (ActionSurface::Row { .. }, None) => None,
        }
    }

    /// What a commit for this surface says it was shown.
    pub(crate) fn seen(&self, entry: &ActionEntry) -> Seen<ActionSeen> {
        let needs_pr = entry.needs_pr();
        Seen(ActionSeen {
            row: self.row.map(|row| ActionRowSeen {
                path: row.path.clone(),
                branch: row.branch.clone(),
            }),
            pr: self
                .row
                .filter(|_| needs_pr)
                .and_then(|row| row.pr.as_ref())
                .and_then(|pr| pr.number),
        })
    }
}

static EMPTY_DISPLAY: std::sync::LazyLock<DisplayPolicy> =
    std::sync::LazyLock::new(DisplayPolicy::default);
static EMPTY_STATE: std::sync::LazyLock<ActionState> =
    std::sync::LazyLock::new(ActionState::default);
static NO_FACTS: std::sync::LazyLock<ActionFacts> = std::sync::LazyLock::new(ActionFacts::default);

pub(super) fn menu(board: &Board, surface: &ActionSurface) -> Menu {
    let Some(scope) = Scope::of(board, surface) else {
        return Menu::Local(LocalMenu::Notice(
            "worktree is no longer on the board".into(),
        ));
    };
    let entries = scope.entries(surface);
    if entries.is_empty() {
        // Every host lists at least its custom prompt once its board is read.
        return Menu::Local(LocalMenu::Notice("actions are still loading".into()));
    }
    let blocked = scope.slot_blocked(surface);
    let slug = match (surface, scope.row) {
        (ActionSurface::Slot { target }, _) => slot_name(*target),
        (_, Some(row)) => row.slug.as_str(),
        (_, None) => "manager",
    };
    if blocked.is_none()
        && let Some(run) = scope.active_run(surface)
    {
        // A row routes by its key, which its host reads as this action slot.
        let action_key = scope
            .row
            .map_or_else(|| run.action_key.clone(), |row| row.key.clone());
        return Menu::Local(LocalMenu::modal(UiModal::Confirm {
            action: ConfirmAction::KillAction {
                action_key,
                run_id: run.run_id.clone(),
            },
            title: format!("Stop {}?", run.action_name),
            lines: vec![
                format!("{} is active on {slug}.", run.action_name),
                "Stopping it terminates its worker and descendants. Partial changes remain in the worktree.".into(),
            ],
            cancel_key: Some('!'),
        }));
    }
    let keys = assign_keys(entries, reserved_keys(surface));
    let facts = scope.facts();
    let option = |value: &str, label: String, chord: Option<char>, detail: String| PickerOption {
        value: Some(value.into()),
        label,
        chord,
        note: None,
        verify_after_merge: None,
        verify_in: None,
        detail: Some(detail),
    };
    let unavailable = |reason: &str| format!("({reason})");
    let mut groups: Vec<(&str, Vec<PickerOption>)> = Vec::new();
    for entry in entries.iter().filter(|entry| entry.id != CUSTOM_ACTION_ID) {
        let reason = blocked.or_else(|| blocker(entry, surface, facts).map(Blocker::reason));
        let group = entry.group.as_deref().unwrap_or_default();
        let label = if group.is_empty() {
            entry.name.clone()
        } else {
            format!("{group}: {}", entry.name)
        };
        let detail = reason.map_or_else(
            || action_detail(entry, &scope.display.primary_harness),
            unavailable,
        );
        push(
            &mut groups,
            group,
            option(&entry.id, label, keys.get(&entry.id).copied(), detail),
        );
    }
    if let (ActionSurface::Row { .. }, Some(row)) = (surface, scope.row)
        && scope.display.actions.dev_server
    {
        let detail = if row.environment_live {
            "live · scrollable".into()
        } else {
            unavailable("dev server is not running")
        };
        push(
            &mut groups,
            "dev server",
            option(
                DEV_LOGS_ACTION_ID,
                "dev server: Open dev server logs".into(),
                Some('l'),
                detail,
            ),
        );
    }
    // Stable: groups keep first appearance, except the configured tail.
    groups.sort_by_key(|(group, _)| {
        scope
            .display
            .actions
            .groups_last
            .iter()
            .position(|name| name == group)
            .map_or(0, |index| index + 1)
    });
    let mut options: Vec<_> = groups
        .into_iter()
        .flat_map(|(_, options)| options)
        .collect();
    if let (ActionSurface::Row { .. }, Some(row)) = (surface, scope.row) {
        let has_pr = row.pr.as_ref().is_some_and(|pr| pr.number.is_some());
        options.push(option(
            crate::AUTO_MERGE_ACTION_ID,
            "github: Toggle merge when ready".into(),
            Some('m'),
            if has_pr {
                "gh · merge queue aware".into()
            } else {
                unavailable("no PR")
            },
        ));
    }
    if matches!(surface, ActionSurface::Slot { .. }) {
        options.push(option(
            OPEN_EDITOR_ACTION_ID,
            "Open in editor".into(),
            Some('z'),
            blocked.map_or_else(|| "local".into(), unavailable),
        ));
    }
    options.push(option(
        CUSTOM_ACTION_ID,
        // Manager and slot palettes message a live session.
        if matches!(surface, ActionSurface::Row { .. }) {
            "Custom prompt…"
        } else {
            "Custom message…"
        }
        .into(),
        Some('c'),
        blocked.map_or_else(|| "freeform".into(), unavailable),
    ));
    let title = match surface {
        ActionSurface::Row { .. } => format!("{slug} actions"),
        ActionSurface::Manager { .. } => "Manager commands".into(),
        ActionSurface::Slot { .. } => format!("{slug} commands"),
    };
    Menu::Local(LocalMenu::modal(UiModal::Picker {
        action: PickerAction::Actions {
            surface: surface.clone(),
        },
        title,
        options,
        selected: 0,
    }))
}

fn push<'g>(groups: &mut Vec<(&'g str, Vec<PickerOption>)>, group: &'g str, option: PickerOption) {
    if let Some((_, options)) = groups.iter_mut().find(|(name, _)| *name == group) {
        options.push(option);
    } else {
        groups.push((group, vec![option]));
    }
}

/// The prompt for the value an action asks for: a text prompt, or a picker
/// of recent values when it has any.
pub(crate) fn arg_modal(
    surface: &ActionSurface,
    entry: &ActionEntry,
    label: &str,
    recent: &[ArgValue],
    seen: Seen<ActionSeen>,
) -> UiModal {
    if recent.is_empty() {
        return UiModal::Text {
            action: TextAction::ActionArg {
                surface: surface.clone(),
                id: entry.id.clone(),
                seen: Some(seen),
            },
            prompt: label.to_owned(),
            initial: String::new(),
            allow_empty: false,
        };
    }
    let mut options = vec![PickerOption {
        value: None,
        label: "Enter a new value…".into(),
        chord: Some('n'),
        note: None,
        verify_after_merge: None,
        verify_in: None,
        detail: None,
    }];
    options.extend(recent.iter().map(|recent| PickerOption {
        value: Some(recent.value.clone()),
        label: match &recent.label {
            Some(label) => format!("{label}  {}", recent.shown),
            None => recent.shown.clone(),
        },
        chord: None,
        note: None,
        verify_after_merge: None,
        verify_in: None,
        detail: None,
    }));
    UiModal::Picker {
        action: PickerAction::ActionArg {
            surface: surface.clone(),
            id: entry.id.clone(),
            seen: Some(seen),
        },
        title: label.to_owned(),
        options,
        selected: 0,
    }
}

/// The freeform prompt, or the optional extra instructions of a prompt
/// action.
pub(crate) fn extras_modal(
    surface: &ActionSurface,
    entry: &ActionEntry,
    arg: Option<String>,
    seen: Seen<ActionSeen>,
) -> UiModal {
    let custom = entry.id == CUSTOM_ACTION_ID;
    UiModal::Text {
        action: TextAction::ActionExtras {
            surface: surface.clone(),
            id: entry.id.clone(),
            arg,
            seen: Some(seen),
        },
        prompt: if custom {
            "Prompt".into()
        } else {
            format!("{} · extra instructions (optional)", entry.name)
        },
        initial: String::new(),
        allow_empty: !custom,
    }
}

/// The trailing hint: `$ id` for shell commands, the agent glyph and id for
/// prompts, with `↪` when it goes to the live session.
fn action_detail(entry: &ActionEntry, primary: &str) -> String {
    let agent = crate::harness_glyph(primary);
    match (entry.shell, entry.target) {
        (true, _) => format!("$ {}", entry.id),
        (_, ActionDelivery::Session) => format!("{agent}  ↪ {}", entry.id),
        _ => format!("{agent}  {}", entry.id),
    }
}

/// Letters a surface keeps for its fixed rows, so configured actions never
/// take them: custom prompt and movement everywhere, then merge and dev logs
/// on rows and open-in-editor on slots.
fn reserved_keys(surface: &ActionSurface) -> &'static [char] {
    match surface {
        ActionSurface::Row { .. } => &['c', 'j', 'k', 'q', 'm', 'l'],
        ActionSurface::Manager { .. } => &['c', 'j', 'k', 'q'],
        ActionSurface::Slot { .. } => &['c', 'j', 'k', 'q', 'z'],
    }
}

fn assign_keys(entries: &[ActionEntry], reserved: &[char]) -> BTreeMap<String, char> {
    let mut taken: BTreeSet<_> = reserved.iter().copied().collect();
    let mut keys = BTreeMap::new();
    let listed = || entries.iter().filter(|entry| entry.id != CUSTOM_ACTION_ID);
    for entry in listed() {
        if let Some(key) = entry
            .key
            .as_deref()
            .filter(|key| key.len() == 1)
            .and_then(|key| key.chars().next())
            .map(|key| key.to_ascii_lowercase())
            && key.is_ascii_alphanumeric()
            && taken.insert(key)
        {
            keys.insert(entry.id.clone(), key);
        }
    }
    for entry in listed() {
        if keys.contains_key(&entry.id) {
            continue;
        }
        if let Some(key) = entry
            .name
            .to_lowercase()
            .chars()
            .chain('a'..='z')
            .find(|key| key.is_ascii_lowercase() && !taken.contains(key))
        {
            taken.insert(key);
            keys.insert(entry.id.clone(), key);
        }
    }
    keys
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{BoardRow, PrPresentation};

    fn entry(id: &str, name: &str) -> ActionEntry {
        ActionEntry {
            id: id.into(),
            name: name.into(),
            ..Default::default()
        }
    }

    fn custom(target: ActionDelivery) -> ActionEntry {
        ActionEntry {
            key: Some("c".into()),
            target,
            fleet: true,
            ..entry(CUSTOM_ACTION_ID, "Custom prompt…")
        }
    }

    /// A catalog with one entry of every shape the palettes treat apart.
    pub(crate) fn catalog() -> ActionCatalog {
        ActionCatalog {
            row: vec![
                ActionEntry {
                    group: Some("agent".into()),
                    key: Some("g".into()),
                    target: ActionDelivery::Session,
                    ..entry("agent-continue", "Agent: continue work")
                },
                ActionEntry {
                    group: Some("stage".into()),
                    shell: true,
                    requires: vec![ActionRequirement::Deployed],
                    ..entry("teardown", "Tear down stage")
                },
                ActionEntry {
                    shell: true,
                    requires: vec![ActionRequirement::Pr],
                    ..entry("rerun-review", "Re-run review")
                },
                ActionEntry {
                    requires: vec![ActionRequirement::PrReady],
                    arg_prompt: Some("Reviewer".into()),
                    ..entry("ask-review", "Ask for review")
                },
                ActionEntry {
                    requires: vec![ActionRequirement::IssueTracker],
                    ..entry("sync-issue", "Sync issue")
                },
                custom(ActionDelivery::Headless),
            ],
            manager: vec![
                ActionEntry {
                    group: Some("manager".into()),
                    key: Some("d".into()),
                    target: ActionDelivery::Manager,
                    fleet: true,
                    ..entry("manager-digest", "Digest: what needs me")
                },
                ActionEntry {
                    group: Some("manager".into()),
                    key: Some("r".into()),
                    target: ActionDelivery::Manager,
                    ..entry("manager-ask-row", "Ask about selected row")
                },
                ActionEntry {
                    group: Some("manager".into()),
                    key: Some("m".into()),
                    target: ActionDelivery::Manager,
                    direct: true,
                    fleet: true,
                    ..entry("manager-compact", "Compact manager context")
                },
                custom(ActionDelivery::Manager),
            ],
            slot: vec![
                ActionEntry {
                    key: Some("g".into()),
                    target: ActionDelivery::Slot,
                    fleet: true,
                    ..entry("slot-continue", "Continue current work")
                },
                ActionEntry {
                    key: Some("m".into()),
                    target: ActionDelivery::Slot,
                    direct: true,
                    fleet: true,
                    ..entry("slot-compact", "Compact context")
                },
                custom(ActionDelivery::Slot),
            ],
            groups_last: vec!["agent".into()],
            dev_server: true,
        }
    }

    pub(crate) fn board() -> Board {
        Board {
            display: DisplayPolicy {
                actions: catalog(),
                primary_harness: "claude".into(),
                ..Default::default()
            },
            rows: vec![BoardRow {
                key: "one".into(),
                slug: "one".into(),
                title: "one".into(),
                branch: "feature/one".into(),
                path: "/work/one".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn row_surface() -> ActionSurface {
        ActionSurface::Row { key: "one".into() }
    }

    fn picker(board: &Board, surface: &ActionSurface) -> (String, Vec<PickerOption>) {
        match menu(board, surface) {
            Menu::Local(LocalMenu::Modal { modal, warm, host }) => {
                assert!(warm.is_none() && host.is_none());
                match *modal {
                    UiModal::Picker {
                        action,
                        title,
                        options,
                        selected,
                    } => {
                        assert_eq!(
                            action,
                            PickerAction::Actions {
                                surface: surface.clone()
                            }
                        );
                        assert_eq!(selected, 0);
                        (title, options)
                    }
                    other => panic!("expected a picker, got {other:?}"),
                }
            }
            other => panic!("expected a board menu, got {other:?}"),
        }
    }

    fn detail<'a>(options: &'a [PickerOption], id: &str) -> &'a str {
        options
            .iter()
            .find(|option| option.value.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("{id} is not listed"))
            .detail
            .as_deref()
            .unwrap()
    }

    #[test]
    fn every_surface_is_answered_from_an_empty_and_a_partial_board() {
        let surfaces = [
            row_surface(),
            ActionSurface::Manager { key: None },
            ActionSurface::Manager {
                key: Some("one".into()),
            },
            ActionSurface::Slot {
                target: SessionTarget::Main,
            },
        ];
        // Nothing is read yet: a notice, never a host request or a panic.
        for surface in &surfaces {
            assert!(matches!(
                menu(&Board::default(), surface),
                Menu::Local(LocalMenu::Notice(_))
            ));
        }
        // The catalog without a single prepared fact: everything that needs
        // a fact reads as unavailable, with why.
        let board = board();
        for surface in &surfaces {
            picker(&board, surface);
        }
        let (_, options) = picker(&board, &row_surface());
        assert_eq!(
            detail(&options, "rerun-review"),
            "(GitHub data is not confirmed)"
        );
        assert_eq!(
            detail(&options, "ask-review"),
            "(GitHub data is not confirmed)"
        );
        assert_eq!(detail(&options, "teardown"), "(no stage deployed)");
        assert_eq!(
            detail(&options, "sync-issue"),
            "(no tracker id (set one with `#`))"
        );
        // A row that left the board has no palette to open.
        assert_eq!(
            menu(&board, &ActionSurface::Row { key: "gone".into() }),
            Menu::Local(LocalMenu::Notice(
                "worktree is no longer on the board".into()
            ))
        );
    }

    #[test]
    fn row_palette_lists_groups_fixed_rows_and_reasons_as_the_host_did() {
        let mut board = board();
        board.rows[0].action_facts = ActionFacts {
            pr: PrFact::Present {
                open: true,
                draft: true,
            },
            deployed: true,
            tracker_id: true,
            active_run: None,
        };
        board.rows[0].pr = Some(PrPresentation {
            number: Some(7),
            ..Default::default()
        });
        let (title, options) = picker(&board, &row_surface());
        assert_eq!(title, "one actions");
        let labels: Vec<_> = options.iter().map(|option| option.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "stage: Tear down stage",
                "Re-run review",
                "Ask for review",
                "Sync issue",
                "dev server: Open dev server logs",
                // The configured tail group, then the fixed rows.
                "agent: Agent: continue work",
                "github: Toggle merge when ready",
                "Custom prompt…",
            ]
        );
        let agent = crate::harness_glyph("claude");
        assert_eq!(
            detail(&options, "agent-continue"),
            format!("{agent}  ↪ agent-continue")
        );
        assert_eq!(detail(&options, "teardown"), "$ teardown");
        assert_eq!(detail(&options, "rerun-review"), "$ rerun-review");
        assert_eq!(detail(&options, "ask-review"), "(PR is draft)");
        assert_eq!(
            detail(&options, "sync-issue"),
            format!("{agent}  sync-issue")
        );
        assert_eq!(
            detail(&options, DEV_LOGS_ACTION_ID),
            "(dev server is not running)"
        );
        assert_eq!(
            detail(&options, crate::AUTO_MERGE_ACTION_ID),
            "gh · merge queue aware"
        );
        assert_eq!(detail(&options, CUSTOM_ACTION_ID), "freeform");
        let chord = |id: &str| {
            options
                .iter()
                .find(|option| option.value.as_deref() == Some(id))
                .unwrap()
                .chord
        };
        assert_eq!(chord("agent-continue"), Some('g'));
        assert_eq!(chord(DEV_LOGS_ACTION_ID), Some('l'));
        assert_eq!(chord(crate::AUTO_MERGE_ACTION_ID), Some('m'));
        assert_eq!(chord(CUSTOM_ACTION_ID), Some('c'));

        board.rows[0].environment_live = true;
        board.rows[0].pr = None;
        board.rows[0].action_facts.pr = PrFact::Absent;
        let (_, options) = picker(&board, &row_surface());
        assert_eq!(detail(&options, DEV_LOGS_ACTION_ID), "live · scrollable");
        assert_eq!(detail(&options, crate::AUTO_MERGE_ACTION_ID), "(no PR)");
        assert_eq!(detail(&options, "rerun-review"), "(no PR)");
    }

    #[test]
    fn manager_palette_without_a_row_offers_only_fleet_entries() {
        let board = board();
        let (title, options) = picker(&board, &ActionSurface::Manager { key: None });
        assert_eq!(title, "Manager commands");
        assert_eq!(detail(&options, "manager-ask-row"), "(no row selected)");
        assert!(!detail(&options, "manager-digest").starts_with('('));
        assert_eq!(options.last().unwrap().label, "Custom message…");
        assert_eq!(detail(&options, CUSTOM_ACTION_ID), "freeform");
        let (_, options) = picker(
            &board,
            &ActionSurface::Manager {
                key: Some("one".into()),
            },
        );
        assert!(!detail(&options, "manager-ask-row").starts_with('('));
    }

    #[test]
    fn an_unavailable_slot_lists_every_entry_with_the_reason() {
        let mut board = board();
        board
            .actions
            .slots_blocked
            .insert("wt".into(), "paths.wt_source not set".into());
        // A run in a slot that cannot run anything is not offered for stopping.
        board.actions.runs.insert(
            "wt".into(),
            ActiveRunView {
                action_key: "wt".into(),
                run_id: "r-1".into(),
                action_name: "Lint".into(),
            },
        );
        let (title, options) = picker(
            &board,
            &ActionSurface::Slot {
                target: SessionTarget::WtSource,
            },
        );
        assert_eq!(title, "wt commands");
        assert!(options.len() > 2);
        assert!(
            options
                .iter()
                .all(|option| option.detail.as_deref() == Some("(paths.wt_source not set)"))
        );
        assert_eq!(options.last().unwrap().label, "Custom message…");
        let (title, options) = picker(
            &board,
            &ActionSurface::Slot {
                target: SessionTarget::Main,
            },
        );
        assert_eq!(title, "main commands");
        assert_eq!(detail(&options, OPEN_EDITOR_ACTION_ID), "local");
        assert_eq!(
            options
                .iter()
                .find(|option| option.value.as_deref() == Some(OPEN_EDITOR_ACTION_ID))
                .unwrap()
                .chord,
            Some('z')
        );
    }

    #[test]
    fn an_active_run_is_offered_for_stopping_by_the_id_the_board_shows() {
        let run = |slot: &str, id: &str| ActiveRunView {
            action_key: slot.into(),
            run_id: id.into(),
            action_name: "Lint".into(),
        };
        let mut board = board();
        board.rows[0].action_facts.active_run = Some(run("one", "run-9"));
        board
            .actions
            .runs
            .insert("main".into(), run("main", "run-3"));
        for surface in [
            row_surface(),
            ActionSurface::Manager {
                key: Some("one".into()),
            },
        ] {
            assert_eq!(
                menu(&board, &surface),
                Menu::Local(LocalMenu::modal(UiModal::Confirm {
                    action: ConfirmAction::KillAction {
                        action_key: "one".into(),
                        run_id: "run-9".into(),
                    },
                    title: "Stop Lint?".into(),
                    lines: vec![
                        "Lint is active on one.".into(),
                        "Stopping it terminates its worker and descendants. Partial changes remain in the worktree.".into(),
                    ],
                    cancel_key: Some('!'),
                }))
            );
        }
        let Menu::Local(LocalMenu::Modal { modal, .. }) = menu(
            &board,
            &ActionSurface::Slot {
                target: SessionTarget::Main,
            },
        ) else {
            panic!("expected the stop confirmation")
        };
        assert!(matches!(
            *modal,
            UiModal::Confirm {
                action: ConfirmAction::KillAction { ref action_key, ref run_id },
                ..
            } if action_key == "main" && run_id == "run-3"
        ));
        // The manager's own slot has no run: its list opens.
        picker(&board, &ActionSurface::Manager { key: None });
    }

    #[test]
    fn a_remote_row_builds_locally_from_its_own_hosts_catalog() {
        let mut board = board();
        let key = wt_core::remote_worktree_ledger_key("studio", "far");
        board.rows.push(BoardRow {
            key: key.clone(),
            host: Some("studio".into()),
            slug: "far".into(),
            branch: "feature/far".into(),
            action_facts: ActionFacts {
                pr: PrFact::Present {
                    open: true,
                    draft: false,
                },
                ..Default::default()
            },
            ..Default::default()
        });
        let surface = ActionSurface::Row { key: key.clone() };
        // Until that host's catalog arrives there is nothing to list, and
        // this machine's actions are never offered in its place.
        assert_eq!(
            menu(&board, &surface),
            Menu::Local(LocalMenu::Notice("actions are still loading".into()))
        );
        board.host_display.insert(
            "studio".into(),
            DisplayPolicy {
                primary_harness: "codex".into(),
                actions: ActionCatalog {
                    row: vec![
                        ActionEntry {
                            requires: vec![ActionRequirement::Pr],
                            ..entry("studio-only", "Studio only")
                        },
                        custom(ActionDelivery::Headless),
                    ],
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let (title, options) = picker(&board, &surface);
        assert_eq!(title, "far actions");
        let ids: Vec<_> = options.iter().filter_map(|o| o.value.as_deref()).collect();
        assert_eq!(
            ids,
            ["studio-only", crate::AUTO_MERGE_ACTION_ID, CUSTOM_ACTION_ID]
        );
        assert_eq!(
            detail(&options, "studio-only"),
            format!("{}  studio-only", crate::harness_glyph("codex"))
        );
        // Its stop request routes by the row's key, which names that host.
        board.rows[1].action_facts.active_run = Some(ActiveRunView {
            action_key: "far".into(),
            run_id: "run-1".into(),
            action_name: "Build".into(),
        });
        let Menu::Local(LocalMenu::Modal { modal, .. }) = menu(&board, &surface) else {
            panic!("expected the stop confirmation")
        };
        assert!(matches!(
            *modal,
            UiModal::Confirm {
                action: ConfirmAction::KillAction { ref action_key, ref run_id },
                ..
            } if *action_key == key && run_id == "run-1"
        ));
    }

    #[test]
    fn a_commit_says_which_checkout_and_pull_request_it_was_shown() {
        let mut board = board();
        board.rows[0].pr = Some(PrPresentation {
            number: Some(7),
            ..Default::default()
        });
        let surface = row_surface();
        let scope = Scope::of(&board, &surface).unwrap();
        let entries = scope.entries(&surface);
        let seen = |id: &str| {
            scope
                .seen(entries.iter().find(|entry| entry.id == id).unwrap())
                .0
        };
        assert_eq!(
            seen("rerun-review"),
            ActionSeen {
                row: Some(ActionRowSeen {
                    path: "/work/one".into(),
                    branch: "feature/one".into(),
                }),
                pr: Some(7),
            }
        );
        // An action that needs no pull request does not pin one.
        assert_eq!(seen("agent-continue").pr, None);
        let slot = ActionSurface::Slot {
            target: SessionTarget::Main,
        };
        let scope = Scope::of(&board, &slot).unwrap();
        assert_eq!(
            scope.seen(&scope.entries(&slot)[0]).0,
            ActionSeen::default()
        );
    }

    #[test]
    fn explicit_keys_win_before_derived_keys_and_reserved_keys_remain_free() {
        let entries = vec![
            entry("derived", "Alpha"),
            ActionEntry {
                key: Some("a".into()),
                ..entry("explicit", "Beta")
            },
            ActionEntry {
                key: Some("c".into()),
                ..entry("reserved", "Continue")
            },
        ];
        let keys = assign_keys(
            &entries,
            reserved_keys(&ActionSurface::Manager { key: None }),
        );
        assert_eq!(keys["explicit"], 'a');
        assert_ne!(keys["derived"], 'a');
        assert!(!keys.values().any(|key| ['c', 'j', 'k', 'q'].contains(key)));
    }

    #[test]
    fn row_palette_reserves_merge_and_logs_letters() {
        let entries = vec![
            ActionEntry {
                key: Some("l".into()),
                ..entry("lint", "Lint")
            },
            entry("test", "Test"),
        ];
        let keys = assign_keys(&entries, reserved_keys(&row_surface()));
        assert!(!keys.values().any(|key| ['m', 'l'].contains(key)));
        // `t` was the retired AI rename; actions may claim it again.
        assert_eq!(keys.get("test"), Some(&'t'));
        let slot = reserved_keys(&ActionSurface::Slot {
            target: SessionTarget::Main,
        });
        assert!(slot.contains(&'z'));
    }

    /// The cost of opening a palette: one pure build, with no request, no
    /// subprocess and no file read. Printed for the before/after record.
    #[test]
    fn opening_a_palette_is_one_pure_build() {
        let mut board = board();
        let template = board.rows[0].clone();
        board.rows = (0..50)
            .map(|index| BoardRow {
                key: format!("row-{index}"),
                slug: format!("row-{index}"),
                ..template.clone()
            })
            .collect();
        for index in 0..40 {
            board.display.actions.row.insert(
                0,
                ActionEntry {
                    group: Some(format!("group-{}", index % 5)),
                    requires: vec![ActionRequirement::Pr],
                    ..entry(&format!("action-{index}"), &format!("Action {index}"))
                },
            );
        }
        let surface = ActionSurface::Row {
            key: "row-49".into(),
        };
        let started = std::time::Instant::now();
        const BUILDS: u32 = 1000;
        for _ in 0..BUILDS {
            let (_, options) = picker(&board, &surface);
            assert_eq!(options.len(), 48);
        }
        let each = started.elapsed() / BUILDS;
        eprintln!("palette build (50 rows, 45 actions): {each:?} each");
        assert!(
            each < std::time::Duration::from_millis(5),
            "building a palette took {each:?}"
        );
    }
}
