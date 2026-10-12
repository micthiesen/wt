//! What each user action does to the board before a fetch confirms it. Every
//! `UiAction` declares its effect here with an exhaustive match, so a new
//! action cannot ship without deciding how it appears while pending.
use wt_runtime::{Domain, Mutation, Target};

use crate::{Board, BoardRow, BoardSection, BusyView, UiAction};

pub(crate) const INBOX: &str = "\0inbox";
pub(crate) const ARCHIVED: &str = "\0archived";
/// The palette entry the controller offers for merge-when-ready. The model
/// resolves it into an explicit enable/disable before sending.
pub const AUTO_MERGE_ACTION_ID: &str = "__auto_merge__";
/// Sentinel from the section picker that opens the "new section" prompt.
const NEW_SECTION: &str = "\0new-section";

/// How an action affects what the board shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Opens a prompt, reads, or acts outside wt's own data.
    ReadOnly,
    /// Changes data the board shows; tracked until a fetch confirms it.
    Mutation(UiMutation),
}

impl Effect {
    /// What the executing host refreshes after the action.
    pub fn domains(&self) -> &'static [Domain] {
        match self {
            Self::ReadOnly => &[],
            Self::Mutation(mutation) => mutation.domains(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UiMutation {
    pub target: Target,
    pub kind: MutationKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MutationKind {
    AutoMerge {
        enable: bool,
    },
    MarkReady,
    Ship,
    Reviewers {
        reviewers: Vec<String>,
    },
    Status {
        state: wt_core::WorkState,
    },
    Archive {
        archived: bool,
    },
    MoveSection {
        section: Option<String>,
    },
    FoldSection {
        section: String,
        folded: bool,
    },
    RenameSection {
        old: String,
        new: String,
    },
    Title {
        title: String,
    },
    Base {
        base: Option<String>,
    },
    Issue {
        issue_id: Option<String>,
    },
    Automations {
        paused: bool,
    },
    AttentionSeen {
        at_ms: u64,
    },
    DismissReview {
        url: String,
    },
    Create {
        placeholder: String,
        input: String,
        resolved: Option<String>,
    },
    Remove {
        keys: Vec<String>,
    },
    /// The controller chooses the next harness; the reply names it.
    Primary {
        harness: Option<String>,
    },
    /// A write with no field-level projection, such as starting a session or
    /// an action run. It still shows as pending until the host answers and a
    /// fetch of its domains follows.
    Other {
        label: &'static str,
        domains: &'static [Domain],
    },
}

impl UiAction {
    /// The effect of this action given the board the user is looking at.
    /// Direction-dependent actions (toggles) resolve against that board, so
    /// the request carries what the user saw rather than what the host's
    /// possibly older data says.
    pub fn effect(&self, board: &Board) -> Effect {
        effect(self, None, board)
    }
}

fn row<'a>(board: &'a Board, key: &str) -> Option<&'a BoardRow> {
    board.rows.iter().find(|row| row.key == key)
}

fn on_row(board: &Board, host: &Option<String>, key: &str, kind: MutationKind) -> Effect {
    let host = row(board, key)
        .map(|row| row.host.clone())
        .unwrap_or_else(|| host.clone());
    Effect::Mutation(UiMutation {
        target: Target::row(host, key),
        kind,
    })
}

fn on_board(host: &Option<String>, kind: MutationKind) -> Effect {
    Effect::Mutation(UiMutation {
        target: Target::board(host.clone()),
        kind,
    })
}

fn other(
    host: &Option<String>,
    key: Option<&str>,
    label: &'static str,
    domains: &'static [Domain],
) -> Effect {
    Effect::Mutation(UiMutation {
        target: Target {
            host: host.clone(),
            key: key.map(str::to_owned),
        },
        kind: MutationKind::Other { label, domains },
    })
}

fn effect(action: &UiAction, host: Option<String>, board: &Board) -> Effect {
    use MutationKind as K;
    let host = &host;
    match action {
        UiAction::OnHost { host, action } => effect(action, host.clone(), board),
        UiAction::PrepareCreate { .. }
        | UiAction::PrepareHardRefresh
        | UiAction::SetHistoryActive { .. }
        | UiAction::SetPerf { .. }
        | UiAction::PrepareRestoreRemoved { .. }
        | UiAction::OpenLink { .. }
        | UiAction::OpenPrLink { .. }
        | UiAction::OpenPrDefault { .. }
        | UiAction::OpenSlotEditor { .. }
        | UiAction::PrepareGithub { .. }
        | UiAction::PrepareReviewers { .. }
        | UiAction::PrepareReviewCheckout { .. }
        | UiAction::PrepareActions { .. }
        | UiAction::PrepareSection { .. }
        | UiAction::GenerateTitle { .. }
        | UiAction::Copy { .. }
        | UiAction::OpenEditor { .. }
        | UiAction::PrepareRemove { .. }
        | UiAction::PrepareCleanup
        | UiAction::PrepareStatus { .. }
        | UiAction::PrepareBase { .. }
        | UiAction::OpenUrl { .. }
        | UiAction::PrepareSessions { .. }
        | UiAction::PrepareHarnesses { .. }
        | UiAction::PrepareStopTerminal { .. } => Effect::ReadOnly,
        // A palette entry only opens its prompt; merge-when-ready is resolved
        // into `GithubSetAutoMerge` before sending (`Model::resolve`).
        UiAction::PrepareAction { .. } => Effect::ReadOnly,
        UiAction::ManagedSignIn { .. } => other(host, None, "signing in", &[Domain::Host]),
        UiAction::HardRefresh => other(host, None, "refreshing", &[]),
        UiAction::SetAttentionSeen { at_ms } => on_board(host, K::AttentionSeen { at_ms: *at_ms }),
        UiAction::ToggleAutomations { key: Some(key) } => {
            let paused = row(board, key).is_some_and(|row| row.automations_paused);
            on_row(board, host, key, K::Automations { paused: !paused })
        }
        UiAction::ToggleAutomations { key: None } => on_board(
            host,
            K::Automations {
                paused: !board.automations_paused,
            },
        ),
        UiAction::CancelAutomations => {
            other(host, None, "cancelling automations", &[Domain::Automations])
        }
        UiAction::RestoreRemoved { key, .. } => other(
            host,
            Some(key),
            "restoring",
            &[Domain::Git, Domain::Metadata, Domain::History],
        ),
        UiAction::ToggleRemovedAutomations { key } => other(
            host,
            Some(key),
            "toggling automations",
            &[Domain::History, Domain::Metadata],
        ),
        UiAction::PerfInvestigate { .. } => {
            other(host, None, "sending investigation", &[Domain::Sessions])
        }
        UiAction::Restack { key } => other(
            host,
            Some(key),
            "restacking",
            &[Domain::Actions, Domain::Git],
        ),
        UiAction::ReviewCheckout { .. } => other(
            host,
            None,
            "checking out review",
            &[Domain::Git, Domain::Metadata, Domain::Reviews],
        ),
        UiAction::DismissReviewRequest { url, .. } => {
            on_board(host, K::DismissReview { url: url.clone() })
        }
        UiAction::SubmitReviewers { key, selected, .. } => on_row(
            board,
            host,
            key,
            K::Reviewers {
                reviewers: selected.clone(),
            },
        ),
        UiAction::GithubMarkReady { key, .. } => on_row(board, host, key, K::MarkReady),
        UiAction::GithubSetAutoMerge { key, enable, .. } => {
            on_row(board, host, key, K::AutoMerge { enable: *enable })
        }
        UiAction::GithubShip { key, .. } => on_row(board, host, key, K::Ship),
        UiAction::GithubFailedChecks { key } => other(
            host,
            Some(key),
            "sending failed checks",
            &[Domain::Sessions],
        ),
        UiAction::KillAction { .. } => other(host, None, "stopping action", &[Domain::Actions]),
        UiAction::RunAction { surface, .. } => {
            let key = match surface {
                crate::ActionSurface::Row { key } => Some(key.as_str()),
                _ => None,
            };
            other(
                host,
                key,
                "starting action",
                &[Domain::Actions, Domain::Sessions],
            )
        }
        UiAction::FoldSection { key, folded } => on_board(
            host,
            K::FoldSection {
                section: key.clone(),
                folded: *folded,
            },
        ),
        UiAction::MoveSection { section, .. } if section.as_deref() == Some(NEW_SECTION) => {
            Effect::ReadOnly
        }
        UiAction::MoveSection { key, section } => on_row(
            board,
            host,
            key,
            K::MoveSection {
                section: section.clone(),
            },
        ),
        UiAction::Reorder { key, .. } => {
            other(host, key.as_deref(), "reordering", &[Domain::Metadata])
        }
        UiAction::RenameSection { old, new } => on_board(
            host,
            K::RenameSection {
                old: old.clone(),
                new: new.clone(),
            },
        ),
        UiAction::CyclePrimary => on_board(host, K::Primary { harness: None }),
        UiAction::SetTitle { key, title } => on_row(
            board,
            host,
            key,
            K::Title {
                title: title.clone(),
            },
        ),
        UiAction::Create { input } => Effect::Mutation(UiMutation {
            target: Target::board(host.clone()),
            kind: K::Create {
                placeholder: String::new(),
                input: input.clone(),
                resolved: None,
            },
        }),
        UiAction::Remove { key, .. } => on_row(
            board,
            host,
            key,
            K::Remove {
                keys: vec![key.clone()],
            },
        ),
        UiAction::Cleanup { revisions } => on_board(
            host,
            K::Remove {
                keys: revisions.iter().map(|r| r.key.clone()).collect(),
            },
        ),
        UiAction::ToggleArchive { key } => {
            let archived = row(board, key).is_some_and(|row| row.archived);
            on_row(
                board,
                host,
                key,
                K::Archive {
                    archived: !archived,
                },
            )
        }
        UiAction::SetStatus { key, state, .. } => {
            match state.as_deref().and_then(wt_core::resolve_work_state) {
                Some(state) => on_row(board, host, key, K::Status { state }),
                None => other(host, Some(key), "updating status", &[Domain::Metadata]),
            }
        }
        UiAction::SetBase { key, base } => on_row(board, host, key, K::Base { base: base.clone() }),
        UiAction::SetIssueOverride { key, issue_id } => on_row(
            board,
            host,
            key,
            K::Issue {
                issue_id: issue_id.clone(),
            },
        ),
        UiAction::Session { key, .. } => {
            other(host, key.as_deref(), "opening session", &[Domain::Sessions])
        }
        UiAction::SelectSession { .. } => other(host, None, "opening session", &[Domain::Sessions]),
        UiAction::KillSession { .. } => other(host, None, "stopping session", &[Domain::Sessions]),
        UiAction::EnterHarness { key, .. } => {
            other(host, Some(key), "opening harness", &[Domain::Sessions])
        }
        UiAction::StopTerminal { key, .. } => {
            other(host, Some(key), "stopping terminal", &[Domain::Sessions])
        }
        UiAction::StopSession { .. } => other(host, None, "stopping session", &[Domain::Sessions]),
    }
}

fn row_mut<'a>(board: &'a mut Board, key: &str) -> Option<&'a mut BoardRow> {
    board.rows.iter_mut().find(|row| row.key == key)
}

fn section_index(board: &mut Board, key: &str, title: &str) -> usize {
    if let Some(index) = board.sections.iter().position(|section| section.key == key) {
        return index;
    }
    board.sections.push(BoardSection {
        key: key.into(),
        title: title.into(),
        folded: key == ARCHIVED,
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

fn busy(label: &str, op: &str) -> Option<BusyView> {
    Some(BusyView {
        op: op.into(),
        label: label.into(),
        ..Default::default()
    })
}

impl UiMutation {
    fn row_key(&self) -> Option<&str> {
        self.target.key.as_deref()
    }

    fn row<'a>(&self, board: &'a Board) -> Option<&'a BoardRow> {
        self.row_key().and_then(|key| row(board, key))
    }

    fn pr(&self, board: &Board) -> Option<crate::PrPresentation> {
        self.row(board).and_then(|row| row.pr.clone())
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
            MutationKind::AutoMerge { .. } | MutationKind::Ship => "merge".to_owned(),
            MutationKind::MarkReady => "draft".to_owned(),
            MutationKind::Reviewers { .. } => "reviewers".to_owned(),
            MutationKind::Status { .. } => "status".to_owned(),
            MutationKind::Archive { .. } => "archive".to_owned(),
            MutationKind::MoveSection { .. } => "section".to_owned(),
            MutationKind::FoldSection { section, .. } => format!("fold:{section}"),
            MutationKind::RenameSection { old, .. } => format!("rename:{old}"),
            MutationKind::Title { .. } => "title".to_owned(),
            MutationKind::Base { .. } => "base".to_owned(),
            MutationKind::Issue { .. } => "issue".to_owned(),
            MutationKind::Automations { .. } => "automations".to_owned(),
            MutationKind::AttentionSeen { .. } => "attention".to_owned(),
            MutationKind::DismissReview { url } => format!("dismiss:{url}"),
            MutationKind::Create { placeholder, .. } => format!("create:{placeholder}"),
            MutationKind::Remove { keys } => format!("remove:{}", keys.join(",")),
            MutationKind::Primary { .. } => "primary".to_owned(),
            MutationKind::Other { label, .. } => format!("other:{label}"),
        };
        format!("{target}#{field}")
    }

    fn target(&self) -> Target {
        self.target.clone()
    }

    fn label(&self) -> String {
        match &self.kind {
            MutationKind::AutoMerge { enable: true } => "arming merge".into(),
            MutationKind::AutoMerge { enable: false } => "disarming merge".into(),
            MutationKind::MarkReady => "marking ready".into(),
            MutationKind::Ship => "shipping".into(),
            MutationKind::Reviewers { .. } => "requesting reviewers".into(),
            MutationKind::Status { state } => format!("setting {}", state.as_str()),
            MutationKind::Archive { archived: true } => "archiving".into(),
            MutationKind::Archive { archived: false } => "restoring".into(),
            MutationKind::MoveSection { section } => {
                format!("moving to {}", section.as_deref().unwrap_or("Inbox"))
            }
            MutationKind::FoldSection { .. } => "folding".into(),
            MutationKind::RenameSection { .. } => "renaming section".into(),
            MutationKind::Title { .. } => "retitling".into(),
            MutationKind::Base { .. } => "setting base".into(),
            MutationKind::Issue { .. } => "setting issue".into(),
            MutationKind::Automations { paused: true } => "pausing automations".into(),
            MutationKind::Automations { paused: false } => "resuming automations".into(),
            MutationKind::AttentionSeen { .. } => "marking seen".into(),
            MutationKind::DismissReview { .. } => "dismissing review".into(),
            MutationKind::Create { resolved: None, .. } => "creating".into(),
            MutationKind::Create { .. } => "updating inventory".into(),
            MutationKind::Remove { .. } => "removing".into(),
            MutationKind::Primary { .. } => "switching harness".into(),
            MutationKind::Other { label, .. } => (*label).into(),
        }
    }

    fn domains(&self) -> &'static [Domain] {
        match &self.kind {
            MutationKind::AutoMerge { .. }
            | MutationKind::MarkReady
            | MutationKind::Ship
            | MutationKind::Reviewers { .. } => &[Domain::Github],
            MutationKind::Status { .. }
            | MutationKind::Archive { .. }
            | MutationKind::MoveSection { .. }
            | MutationKind::FoldSection { .. }
            | MutationKind::RenameSection { .. }
            | MutationKind::Base { .. }
            | MutationKind::Issue { .. }
            | MutationKind::Automations { .. }
            | MutationKind::AttentionSeen { .. } => &[Domain::Metadata],
            MutationKind::Title { .. } => &[Domain::Metadata, Domain::Naming],
            MutationKind::DismissReview { .. } => &[Domain::Reviews],
            MutationKind::Create { .. } | MutationKind::Remove { .. } => {
                &[Domain::Git, Domain::Metadata]
            }
            MutationKind::Primary { .. } => &[Domain::Sessions],
            MutationKind::Other { domains, .. } => domains,
        }
    }

    fn apply(&self, board: &mut Board) {
        let key = self.row_key().unwrap_or_default().to_owned();
        match &self.kind {
            MutationKind::AutoMerge { enable } => {
                if let Some(pr) = row_mut(board, &key).and_then(|row| row.pr.as_mut()) {
                    pr.auto_merge_armed = *enable;
                    if !enable {
                        pr.merge_queue = None;
                    }
                }
            }
            MutationKind::MarkReady => {
                if let Some(pr) = row_mut(board, &key).and_then(|row| row.pr.as_mut()) {
                    pr.draft = false;
                }
            }
            MutationKind::Ship => {
                if let Some(pr) = row_mut(board, &key).and_then(|row| row.pr.as_mut()) {
                    pr.draft = false;
                    if pr.merge_queue.is_none() {
                        pr.auto_merge_armed = true;
                    }
                }
            }
            MutationKind::Reviewers { reviewers } => {
                if let Some(pr) = row_mut(board, &key).and_then(|row| row.pr.as_mut()) {
                    pr.reviewers = reviewers.clone();
                }
            }
            MutationKind::Status { state } => {
                if let Some(row) = row_mut(board, &key) {
                    let work = row.work.get_or_insert_with(Default::default);
                    match &mut work.record {
                        Some(record) => record.state = *state,
                        None => work.record = Some(wt_core::WorkStatusRecord::new(*state, "")),
                    }
                    work.effective_state = Some(*state);
                    work.derived = false;
                }
            }
            MutationKind::Archive { archived } => {
                if let Some(index) = board.rows.iter().position(|row| row.key == key) {
                    board.rows[index].archived = *archived;
                    if *archived {
                        place(board, index, ARCHIVED, "Archived");
                    } else if row_section(board, index)
                        .is_some_and(|section| board.sections[section].key == ARCHIVED)
                    {
                        place(board, index, INBOX, "Inbox");
                    }
                }
            }
            MutationKind::MoveSection { section } => {
                if let Some(index) = board.rows.iter().position(|row| row.key == key) {
                    match section {
                        None => place(board, index, INBOX, "Inbox"),
                        Some(name) => {
                            let (key, title) = match find_section(board, name) {
                                Some(found) => (
                                    board.sections[found].key.clone(),
                                    board.sections[found].title.clone(),
                                ),
                                None => (name.trim().to_owned(), name.trim().to_owned()),
                            };
                            place(board, index, &key, &title);
                        }
                    }
                }
            }
            MutationKind::FoldSection { section, folded } => {
                if let Some(found) = board.sections.iter_mut().find(|s| &s.key == section) {
                    found.folded = *folded;
                }
            }
            MutationKind::RenameSection { old, new } => {
                if let Some(found) = find_section(board, old) {
                    board.sections[found].title = new.clone();
                }
            }
            MutationKind::Title { title } => {
                if let Some(row) = row_mut(board, &key) {
                    row.title = title.clone();
                    row.title_source = crate::TitleSource::Manual;
                }
            }
            MutationKind::Base { base } => {
                if let Some(row) = row_mut(board, &key) {
                    row.base_branch = base.clone();
                }
            }
            MutationKind::Issue { issue_id } => {
                if let Some(row) = row_mut(board, &key) {
                    row.issue_id = issue_id.clone();
                }
            }
            MutationKind::Automations { paused } => match self.row_key() {
                Some(key) => {
                    if let Some(row) = row_mut(board, key) {
                        row.automations_paused = *paused;
                    }
                }
                None => board.automations_paused = *paused,
            },
            MutationKind::AttentionSeen { at_ms } => {
                board.attention_seen_ms = board.attention_seen_ms.max(*at_ms);
            }
            MutationKind::DismissReview { url } => {
                board.review_requests.retain(|row| &row.url != url);
            }
            MutationKind::Create {
                placeholder,
                input,
                resolved,
            } => {
                if resolved
                    .as_deref()
                    .is_some_and(|key| board.rows.iter().any(|row| row.key == key))
                {
                    return;
                }
                let ready = resolved.is_some();
                let index = board.rows.len();
                board.rows.push(BoardRow {
                    key: placeholder.clone(),
                    host: self.target.host.clone(),
                    title: input.clone(),
                    busy: busy(
                        if ready {
                            "ready · updating inventory"
                        } else {
                            "creating"
                        },
                        "init",
                    ),
                    detail_groups: vec![crate::PreparedDetailGroup {
                        id: "git".into(),
                        label: "git".into(),
                        ..Default::default()
                    }],
                    badge: if ready {
                        "ready · updating inventory"
                    } else {
                        "creating · F12 to open when ready"
                    }
                    .into(),
                    details: vec![
                        if ready {
                            "Checkout ready. Waiting for inventory."
                        } else {
                            "Creating checkout. F12 will open the agent when ready."
                        }
                        .into(),
                    ],
                    ..Default::default()
                });
                let inbox = section_index(board, INBOX, "Inbox");
                board.sections[inbox].rows.push(index);
                board.sections[inbox].folded = false;
            }
            MutationKind::Remove { keys } => {
                for key in keys {
                    if let Some(index) = board.rows.iter().position(|row| &row.key == key) {
                        let row = &mut board.rows[index];
                        row.archived = true;
                        row.badge = "removing".into();
                        row.busy = busy("removing", "remove");
                        place(board, index, ARCHIVED, "Archived");
                    }
                }
            }
            MutationKind::Primary { harness } => {
                if let Some(harness) = harness {
                    board.display.primary_harness = harness.clone();
                }
            }
            MutationKind::Other { .. } => {}
        }
    }

    fn observed(&self, board: &Board) -> bool {
        let row = self.row(board);
        match &self.kind {
            // A row or PR that is gone has nothing left to show.
            MutationKind::AutoMerge { enable } => self
                .pr(board)
                .is_none_or(|pr| (pr.auto_merge_armed || pr.merge_queue.is_some()) == *enable),
            MutationKind::MarkReady => self.pr(board).is_none_or(|pr| !pr.draft),
            MutationKind::Ship => self
                .pr(board)
                .is_none_or(|pr| !pr.draft && (pr.auto_merge_armed || pr.merge_queue.is_some())),
            MutationKind::Reviewers { reviewers } => self.pr(board).is_none_or(|pr| {
                let mut shown = pr.reviewers.clone();
                let mut wanted = reviewers.clone();
                shown.sort();
                wanted.sort();
                shown == wanted
            }),
            MutationKind::Status { state } => row.is_none_or(|row| {
                row.work
                    .as_ref()
                    .and_then(|work| work.record.as_ref())
                    .is_some_and(|record| record.state == *state)
            }),
            MutationKind::Archive { archived } => row.is_none_or(|row| row.archived == *archived),
            MutationKind::MoveSection { section } => row.is_none_or(|_| {
                let index = board
                    .rows
                    .iter()
                    .position(|row| Some(row.key.as_str()) == self.row_key());
                index
                    .and_then(|index| row_section(board, index))
                    .is_some_and(|found| {
                        let found = &board.sections[found];
                        match section {
                            None => found.key == INBOX,
                            Some(name) => {
                                found.key.eq_ignore_ascii_case(name.trim())
                                    || found.title.eq_ignore_ascii_case(name.trim())
                            }
                        }
                    })
            }),
            MutationKind::FoldSection { section, folded } => board
                .sections
                .iter()
                .find(|s| &s.key == section)
                .is_none_or(|s| s.folded == *folded),
            MutationKind::RenameSection { new, .. } => find_section(board, new).is_some(),
            MutationKind::Title { title } => row.is_none_or(|row| &row.title == title),
            MutationKind::Base { base } => row.is_none_or(|row| &row.base_branch == base),
            MutationKind::Issue { issue_id } => row.is_none_or(|row| &row.issue_id == issue_id),
            MutationKind::Automations { paused } => match self.row_key() {
                Some(_) => row.is_none_or(|row| row.automations_paused == *paused),
                None => board.automations_paused == *paused,
            },
            MutationKind::AttentionSeen { at_ms } => board.attention_seen_ms >= *at_ms,
            MutationKind::DismissReview { url } => {
                !board.review_requests.iter().any(|row| &row.url == url)
            }
            // The placeholder stands in for the row until inventory shows
            // it. Confirmed data never contains a placeholder, so only the
            // real row can settle the creation.
            MutationKind::Create {
                resolved,
                placeholder,
                ..
            } => board.rows.iter().any(|row| {
                Some(row.key.as_str()) == resolved.as_deref() || &row.key == placeholder
            }),
            MutationKind::Remove { keys } => !keys.iter().any(|key| {
                board
                    .rows
                    .iter()
                    .any(|row| &row.key == key && !row.archived)
            }),
            MutationKind::Primary { harness } => harness
                .as_deref()
                .is_none_or(|h| h.eq_ignore_ascii_case(&board.display.primary_harness)),
            MutationKind::Other { .. } => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PrPresentation, ReviewRequestRow};
    use wt_runtime::conformance;

    fn board() -> Board {
        Board {
            rows: vec![BoardRow {
                key: "a".into(),
                title: "Old".into(),
                pr: Some(PrPresentation {
                    number: Some(7),
                    head_sha: Some("abc".into()),
                    state: Some("OPEN".into()),
                    draft: true,
                    ..Default::default()
                }),
                ..Default::default()
            }],
            sections: vec![
                BoardSection {
                    key: INBOX.into(),
                    title: "Inbox".into(),
                    rows: vec![0],
                    ..Default::default()
                },
                BoardSection {
                    key: "Batch".into(),
                    title: "Batch".into(),
                    ..Default::default()
                },
            ],
            review_requests: vec![ReviewRequestRow {
                url: "https://github.com/o/r/pull/9".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn row(kind: MutationKind) -> UiMutation {
        UiMutation {
            target: Target::row(None, "a"),
            kind,
        }
    }

    fn whole(kind: MutationKind) -> UiMutation {
        UiMutation {
            target: Target::board(None),
            kind,
        }
    }

    fn with(edit: impl FnOnce(&mut Board)) -> Board {
        let mut board = board();
        edit(&mut board);
        board
    }

    fn pr(board: &mut Board) -> &mut PrPresentation {
        board.rows[0].pr.as_mut().unwrap()
    }

    /// Every mutation kind meets the shared optimistic contract against data a
    /// real fetch would return.
    #[tokio::test(start_paused = true)]
    async fn every_mutation_kind_meets_the_contract() {
        let cases: Vec<(UiMutation, Board, Board)> = vec![
            (
                row(MutationKind::AutoMerge { enable: true }),
                board(),
                with(|b| pr(b).auto_merge_armed = true),
            ),
            (
                row(MutationKind::AutoMerge { enable: false }),
                with(|b| pr(b).auto_merge_armed = true),
                board(),
            ),
            (
                row(MutationKind::MarkReady),
                board(),
                with(|b| pr(b).draft = false),
            ),
            (
                row(MutationKind::Ship),
                board(),
                with(|b| {
                    pr(b).draft = false;
                    pr(b).merge_queue = Some(Default::default());
                }),
            ),
            (
                row(MutationKind::Reviewers {
                    reviewers: vec!["sam".into()],
                }),
                board(),
                with(|b| pr(b).reviewers = vec!["sam".into()]),
            ),
            (
                row(MutationKind::Status {
                    state: wt_core::WorkState::Review,
                }),
                board(),
                with(|b| {
                    b.rows[0].work = Some(crate::WorkPresentation {
                        record: Some(wt_core::WorkStatusRecord::new(
                            wt_core::WorkState::Review,
                            "2026-10-11T00:00:00Z",
                        )),
                        ..Default::default()
                    })
                }),
            ),
            (
                row(MutationKind::Archive { archived: true }),
                board(),
                with(|b| {
                    b.rows[0].archived = true;
                    b.sections[0].rows.clear();
                    b.sections.push(BoardSection {
                        key: ARCHIVED.into(),
                        title: "Archived".into(),
                        rows: vec![0],
                        ..Default::default()
                    });
                }),
            ),
            (
                row(MutationKind::MoveSection {
                    section: Some("batch".into()),
                }),
                board(),
                with(|b| {
                    b.sections[0].rows.clear();
                    b.sections[1].rows.push(0);
                }),
            ),
            (
                whole(MutationKind::FoldSection {
                    section: "Batch".into(),
                    folded: true,
                }),
                board(),
                with(|b| b.sections[1].folded = true),
            ),
            (
                whole(MutationKind::RenameSection {
                    old: "Batch".into(),
                    new: "Later".into(),
                }),
                board(),
                with(|b| {
                    b.sections[1].key = "Later".into();
                    b.sections[1].title = "Later".into();
                }),
            ),
            (
                row(MutationKind::Title {
                    title: "New".into(),
                }),
                board(),
                with(|b| b.rows[0].title = "New".into()),
            ),
            (
                row(MutationKind::Base {
                    base: Some("feature".into()),
                }),
                board(),
                with(|b| b.rows[0].base_branch = Some("feature".into())),
            ),
            (
                row(MutationKind::Issue {
                    issue_id: Some("ENG-1".into()),
                }),
                board(),
                with(|b| b.rows[0].issue_id = Some("ENG-1".into())),
            ),
            (
                row(MutationKind::Automations { paused: true }),
                board(),
                with(|b| b.rows[0].automations_paused = true),
            ),
            (
                whole(MutationKind::Automations { paused: true }),
                board(),
                with(|b| b.automations_paused = true),
            ),
            (
                whole(MutationKind::AttentionSeen { at_ms: 5 }),
                board(),
                with(|b| b.attention_seen_ms = 9),
            ),
            (
                whole(MutationKind::DismissReview {
                    url: "https://github.com/o/r/pull/9".into(),
                }),
                board(),
                with(|b| b.review_requests.clear()),
            ),
            (
                whole(MutationKind::Create {
                    placeholder: "\0creating:1".into(),
                    input: "new".into(),
                    resolved: Some("new".into()),
                }),
                board(),
                with(|b| {
                    b.rows.push(BoardRow {
                        key: "new".into(),
                        ..Default::default()
                    })
                }),
            ),
            (
                whole(MutationKind::Remove {
                    keys: vec!["a".into()],
                }),
                board(),
                with(|b| {
                    b.rows.clear();
                    b.sections[0].rows.clear();
                }),
            ),
            (
                whole(MutationKind::Primary {
                    harness: Some("Codex".into()),
                }),
                board(),
                with(|b| b.display.primary_harness = "codex".into()),
            ),
        ];
        for (mutation, before, after) in cases {
            conformance::mutation(mutation, before, after).await;
        }
    }

    #[test]
    fn every_action_declares_an_effect_and_toggles_use_the_shown_state() {
        let shown = with(|b| {
            pr(b).auto_merge_armed = true;
            b.rows[0].archived = true;
        });
        let effect = UiAction::ToggleArchive { key: "a".into() }.effect(&shown);
        assert!(matches!(
            effect,
            Effect::Mutation(UiMutation {
                kind: MutationKind::Archive { archived: false },
                ..
            })
        ));
        let remote = UiAction::OnHost {
            host: Some("box".into()),
            action: Box::new(UiAction::GithubSetAutoMerge {
                key: "missing".into(),
                enable: false,
                pin: None,
            }),
        }
        .effect(&shown);
        let Effect::Mutation(mutation) = remote else {
            panic!("a GitHub write is a mutation");
        };
        assert_eq!(mutation.target.host.as_deref(), Some("box"));
        assert_eq!(mutation.domains(), [Domain::Github]);
        assert_eq!(UiAction::PrepareCleanup.effect(&shown), Effect::ReadOnly);
        assert_eq!(
            UiAction::MoveSection {
                key: "a".into(),
                section: Some(NEW_SECTION.into()),
            }
            .effect(&shown),
            Effect::ReadOnly
        );
    }

    #[test]
    fn creation_placeholder_survives_stale_boards_until_its_row_arrives() {
        let mut creating = whole(MutationKind::Create {
            placeholder: "\0creating:1".into(),
            input: "new --base staging".into(),
            resolved: None,
        });
        let mut stale = board();
        creating.apply(&mut stale);
        assert_eq!(stale.rows.len(), 2);
        assert_eq!(stale.rows[1].badge, "creating · F12 to open when ready");
        assert!(stale.sections[0].rows.contains(&1));
        if let MutationKind::Create { resolved, .. } = &mut creating.kind {
            *resolved = Some("new".into());
        }
        let mut still_stale = board();
        creating.apply(&mut still_stale);
        assert_eq!(still_stale.rows[1].badge, "ready · updating inventory");
        let mut fresh = with(|b| {
            b.rows.push(BoardRow {
                key: "new".into(),
                ..Default::default()
            })
        });
        creating.apply(&mut fresh);
        assert_eq!(fresh.rows.len(), 2, "no placeholder beside the real row");
    }

    #[test]
    fn removal_moves_the_row_and_keeps_later_source_fields() {
        let removing = whole(MutationKind::Remove {
            keys: vec!["a".into()],
        });
        let mut latest = with(|b| b.rows[0].title = "New title".into());
        removing.apply(&mut latest);
        assert!(latest.rows[0].archived);
        assert_eq!(latest.rows[0].title, "New title");
        assert!(latest.sections[0].rows.is_empty());
        assert_eq!(latest.sections.last().unwrap().rows, [0]);
    }
}
