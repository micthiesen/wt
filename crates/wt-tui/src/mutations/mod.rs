//! What each user action does to the board before a fetch confirms it.
//! `effect` maps every `UiAction` to its change; each family module owns the
//! projection, confirmation and deadlines of its kinds.
mod effect;
mod github;
mod layout;
mod lifecycle;
mod row;
#[cfg(test)]
mod tests;

use wt_core::Domain;
use wt_runtime::{Deadlines, Mutation, Target};

use crate::{
    Board, BoardRow, BoardSection, UiReply,
    model::{ARCHIVED_SECTION, INBOX_SECTION},
};

pub use effect::Effect;
pub use github::GithubChange;
pub use layout::LayoutChange;
pub use lifecycle::LifecycleChange;
pub use row::RowChange;

/// The palette entry the controller offers for merge-when-ready. The model
/// resolves it into an explicit enable/disable before sending.
pub const AUTO_MERGE_ACTION_ID: &str = "__auto_merge__";
/// Domains a pending change may wait on. Each must be published by a leaf
/// source on every host (wt-app's registry test checks that), otherwise a
/// change could only end at its deadline. An empty list settles as soon as
/// the host accepts.
pub const SETTLING_DOMAINS: &[Domain] = &[
    Domain::Git,
    Domain::Metadata,
    Domain::Github,
    Domain::Reviews,
    Domain::Sessions,
    Domain::Actions,
];

#[derive(Clone, Debug, PartialEq)]
pub struct UiMutation {
    pub target: Target,
    pub kind: MutationKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MutationKind {
    Github(GithubChange),
    Layout(LayoutChange),
    Row(RowChange),
    Lifecycle(LifecycleChange),
    /// A write with no field-level projection, such as starting a session or
    /// an action run. It still shows as pending until the host answers and a
    /// fetch of its domains follows.
    Unprojected {
        label: &'static str,
        domains: &'static [Domain],
    },
}

impl From<GithubChange> for MutationKind {
    fn from(change: GithubChange) -> Self {
        Self::Github(change)
    }
}

impl From<LayoutChange> for MutationKind {
    fn from(change: LayoutChange) -> Self {
        Self::Layout(change)
    }
}

impl From<RowChange> for MutationKind {
    fn from(change: RowChange) -> Self {
        Self::Row(change)
    }
}

impl From<LifecycleChange> for MutationKind {
    fn from(change: LifecycleChange) -> Self {
        Self::Lifecycle(change)
    }
}

/// What one family of changes does to the board. `apply` and `observed`
/// receive the change's target: its host and, for a row change, the row key.
trait Family {
    /// The field the change writes; with the target it identifies the change.
    fn field(&self) -> String;
    fn label(&self) -> String;
    /// The domains whose fetch can confirm the change.
    fn domains(&self) -> &'static [Domain];
    /// Show the change on `board`.
    fn apply(&self, target: &Target, board: &mut Board);
    /// Whether confirmed data already shows the change.
    fn observed(&self, target: &Target, board: &Board) -> bool;
    /// Deadlines that differ from the defaults.
    fn deadlines(&self) -> Option<Deadlines> {
        None
    }
}

impl MutationKind {
    fn family(&self) -> Option<&dyn Family> {
        match self {
            Self::Github(change) => Some(change),
            Self::Layout(change) => Some(change),
            Self::Row(change) => Some(change),
            Self::Lifecycle(change) => Some(change),
            Self::Unprojected { .. } => None,
        }
    }
}

impl UiMutation {
    /// Name a creation's placeholder row after its operation id. Returns the
    /// placeholder key, or `None` for any other change.
    pub(crate) fn assign_placeholder(&mut self, id: u64) -> Option<String> {
        let MutationKind::Lifecycle(LifecycleChange::Create { placeholder, .. }) = &mut self.kind
        else {
            return None;
        };
        *placeholder = format!("\0creating:{id}");
        Some(placeholder.clone())
    }

    /// Record what the host's accepting reply decided: the key of a created
    /// row, or the harness a cycle switched to.
    pub(crate) fn on_accepted(&mut self, reply: &UiReply) {
        match &mut self.kind {
            MutationKind::Lifecycle(LifecycleChange::Create { resolved, .. }) => {
                *resolved = reply.select_when_visible.clone();
            }
            MutationKind::Row(RowChange::Primary { harness }) => {
                *harness = reply.primary_harness.clone();
            }
            _ => {}
        }
    }
}

impl Mutation<Board> for UiMutation {
    fn key(&self) -> String {
        let target = format!(
            "{}/{}",
            self.target.host.as_deref().unwrap_or(""),
            self.target.key.as_deref().unwrap_or("")
        );
        let field = match &self.kind {
            MutationKind::Unprojected { label, .. } => format!("unprojected:{label}"),
            kind => kind.family().map(Family::field).unwrap_or_default(),
        };
        format!("{target}#{field}")
    }

    fn target(&self) -> Target {
        self.target.clone()
    }

    fn label(&self) -> String {
        match &self.kind {
            MutationKind::Unprojected { label, .. } => (*label).into(),
            kind => kind.family().map(Family::label).unwrap_or_default(),
        }
    }

    fn domains(&self) -> &'static [Domain] {
        match &self.kind {
            MutationKind::Unprojected { domains, .. } => domains,
            kind => kind.family().map_or(&[], Family::domains),
        }
    }

    fn apply(&self, board: &mut Board) {
        if let Some(family) = self.kind.family() {
            family.apply(&self.target, board);
        }
    }

    fn deadlines(&self) -> Option<Deadlines> {
        self.kind.family().and_then(Family::deadlines)
    }

    fn observed(&self, board: &Board) -> bool {
        self.kind
            .family()
            .is_none_or(|family| family.observed(&self.target, board))
    }
}

fn row<'a>(board: &'a Board, key: &str) -> Option<&'a BoardRow> {
    board.rows.iter().find(|row| row.key == key)
}

fn row_mut<'a>(board: &'a mut Board, key: &str) -> Option<&'a mut BoardRow> {
    board.rows.iter_mut().find(|row| row.key == key)
}

/// The targeted row as `board` shows it.
fn target_row<'a>(board: &'a Board, target: &Target) -> Option<&'a BoardRow> {
    target.key.as_deref().and_then(|key| row(board, key))
}

/// The targeted row, for an edit.
fn target_row_mut<'a>(board: &'a mut Board, target: &Target) -> Option<&'a mut BoardRow> {
    row_mut(board, target.key.as_deref().unwrap_or_default())
}

/// Position of the targeted row.
fn target_index(board: &Board, target: &Target) -> Option<usize> {
    let key = target.key.as_deref().unwrap_or_default();
    board.rows.iter().position(|row| row.key == key)
}

fn section_index(board: &mut Board, key: &str, title: &str) -> usize {
    if let Some(index) = board.sections.iter().position(|section| section.key == key) {
        return index;
    }
    board.sections.push(BoardSection {
        key: key.into(),
        title: title.into(),
        folded: key == ARCHIVED_SECTION,
        ..Default::default()
    });
    board.sections.len() - 1
}

fn find_section(board: &Board, name: &str) -> Option<usize> {
    board
        .sections
        .iter()
        .position(|s| s.key.eq_ignore_ascii_case(name) || s.title.eq_ignore_ascii_case(name))
}

fn row_section(board: &Board, index: usize) -> Option<usize> {
    board
        .sections
        .iter()
        .position(|section| section.rows.contains(&index))
}

/// Move a row between sections in the presentation only.
fn place(board: &mut Board, index: usize, key: &str, title: &str) {
    for section in &mut board.sections {
        section.rows.retain(|row| *row != index);
    }
    let target = section_index(board, key, title);
    board.sections[target].rows.push(index);
}

const INBOX_TITLE: &str = "Inbox";
const ARCHIVED_TITLE: &str = "Archived";
