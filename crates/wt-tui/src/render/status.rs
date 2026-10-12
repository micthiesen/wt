//! The one place that turns source provenance and pending changes into
//! pixels. Panes draw their border through `pane`, naming the domains they
//! show; rows and the details title use `pending_chip`. No pane formats
//! cached, refreshing, failed, pending or unconfirmed states itself.
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders},
};
use wt_runtime::{Domain, PendingPhase, PendingView, Provenance, SourceState};

use super::text::{age, now_ms};
use crate::{Model, theme};

/// Domains behind the worktree list and its review section.
pub(crate) const LIST: &[Domain] = &[
    Domain::Git,
    Domain::Metadata,
    Domain::Github,
    Domain::Reviews,
    Domain::Issues,
    Domain::Sessions,
    Domain::Naming,
    Domain::Presentation,
    Domain::Host,
];
/// Domains behind the selected row's details.
pub(crate) const DETAILS: &[Domain] = &[
    Domain::Git,
    Domain::Metadata,
    Domain::Github,
    Domain::Issues,
    Domain::Sessions,
    Domain::Presentation,
    Domain::Actions,
    Domain::Dev,
    Domain::Host,
];
/// Domains behind the output pane.
pub(crate) const OUTPUT: &[Domain] = &[Domain::Activity, Domain::Actions, Domain::Sessions];
pub(crate) const HISTORY: &[Domain] = &[Domain::History];
pub(crate) const PERF: &[Domain] = &[Domain::Perf];
/// Everything, for the header summary.
pub(crate) const ALL: &[Domain] = &[
    Domain::Git,
    Domain::Metadata,
    Domain::Github,
    Domain::Reviews,
    Domain::Issues,
    Domain::Sessions,
    Domain::Activity,
    Domain::Actions,
    Domain::Dev,
    Domain::Origin,
    Domain::Naming,
    Domain::Presentation,
    Domain::History,
    Domain::Automations,
    Domain::Host,
];

/// The combined condition of some domains across every host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Status {
    /// Oldest restore time among restored domains, epoch ms.
    pub cached_since: Option<u64>,
    pub refreshing: bool,
    pub loading: bool,
    /// The first failed domain and its error.
    pub failed: Option<(Domain, String)>,
}

pub(crate) fn status(model: &Model, domains: &[Domain]) -> Status {
    let hosts =
        std::iter::once(&model.local_domains).chain(model.source_board.host_domains.values());
    combine(hosts, domains)
}

/// The status of one host's domains, for panes about a single row.
pub(crate) fn host_status(model: &Model, host: Option<&str>, domains: &[Domain]) -> Status {
    match host {
        None => combine(std::iter::once(&model.local_domains), domains),
        Some(host) => combine(model.source_board.host_domains.get(host), domains),
    }
}

fn combine<'a>(
    hosts: impl IntoIterator<Item = &'a wt_runtime::DomainMap>,
    domains: &[Domain],
) -> Status {
    let mut status = Status::default();
    for map in hosts {
        for domain in domains {
            let Some(meta) = map.get(domain) else {
                continue;
            };
            if let Provenance::Restored { saved_at_ms } = meta.provenance() {
                status.cached_since = Some(
                    status
                        .cached_since
                        .map_or(saved_at_ms, |at| at.min(saved_at_ms)),
                );
            }
            match meta.state() {
                SourceState::Refreshing => status.refreshing = true,
                SourceState::Empty => status.loading = true,
                SourceState::Failed(error) if status.failed.is_none() => {
                    status.failed = Some((*domain, error.to_string()));
                }
                _ => {}
            }
        }
    }
    status
}

/// Short spans describing a status, or nothing when everything is live and
/// ready. Saved data says how old it is; a failure is named beside it; while
/// neither applies, a pane is loading or refreshing.
pub(crate) fn chip(status: &Status, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if let Some(saved) = status.cached_since {
        spans.push(Span::styled(
            format!(" · cached {}", age(saved, now_ms())),
            base.fg(theme::WARN),
        ));
    }
    if let Some((domain, _)) = &status.failed {
        spans.push(Span::styled(
            format!(" · {} failed", domain.label()),
            base.fg(theme::ERR),
        ));
    }
    if !spans.is_empty() {
        return spans;
    }
    if status.loading {
        return vec![Span::styled(" · loading…", base.fg(theme::FG_DIM))];
    }
    if status.refreshing {
        return vec![Span::styled(" ↻", base.fg(theme::FG_DIM))];
    }
    Vec::new()
}

/// A bordered pane whose title carries the status of the domains it shows.
/// Pass `&[]` for panes that show no source data.
pub(crate) fn pane(title: String, domains: &[Domain], model: &Model) -> Block<'static> {
    pane_with(title, Vec::new(), domains, model)
}

/// `pane` with extra title spans, such as the selected row's pending chip.
pub(crate) fn pane_with(
    title: String,
    extra: Vec<Span<'static>>,
    domains: &[Domain],
    model: &Model,
) -> Block<'static> {
    let status = (!domains.is_empty()).then(|| status(model, domains));
    frame(title, extra, status)
}

/// A pane about one row: only that row's host's sources set its status.
pub(crate) fn row_pane(
    title: String,
    extra: Vec<Span<'static>>,
    host: Option<&str>,
    domains: &[Domain],
    model: &Model,
) -> Block<'static> {
    frame(title, extra, Some(host_status(model, host, domains)))
}

fn frame(title: String, extra: Vec<Span<'static>>, status: Option<Status>) -> Block<'static> {
    let mut spans = vec![
        Span::styled("─ ", theme::fg(theme::BORDER)),
        Span::styled(title, theme::fg(theme::FG_DIM)),
    ];
    if !extra.is_empty() {
        spans.push(Span::raw(" "));
        spans.extend(extra);
    }
    if let Some(status) = &status {
        spans.extend(chip(status, Style::new()));
    }
    spans.push(Span::styled(" ", theme::fg(theme::BORDER)));
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(theme::fg(theme::BORDER))
        .title(Line::from(spans))
}

fn phase_style(phase: PendingPhase) -> (&'static str, Color) {
    match phase {
        PendingPhase::InFlight => ("…", theme::FG_DIM),
        PendingPhase::Accepted => ("↻", theme::ACCENT),
        PendingPhase::Unconfirmed => ("?", theme::WARN),
    }
}

/// Pending changes aimed at one row, newest last.
pub(crate) fn row_pending<'a>(model: &'a Model, key: &str) -> Vec<&'a PendingView> {
    model
        .board
        .pending
        .iter()
        .filter(|view| view.target.key.as_deref() == Some(key))
        .collect()
}

/// Pending changes that are not about a row (harness switch, sections).
pub(crate) fn board_pending(model: &Model) -> Vec<&PendingView> {
    model
        .board
        .pending
        .iter()
        .filter(|view| view.target.key.is_none())
        .collect()
}

/// One marker for pending changes: the newest one's label, with a glyph for
/// its phase and a count when there are several.
pub(crate) fn pending_chip(views: &[&PendingView], base: Style) -> Vec<Span<'static>> {
    let Some(last) = views.last() else {
        return Vec::new();
    };
    let (glyph, color) = phase_style(last.phase);
    let mut text = format!("{glyph} {}", last.label);
    if last.phase == PendingPhase::Unconfirmed {
        text.push_str(" (unconfirmed)");
    }
    if views.len() > 1 {
        text.push_str(&format!(" +{}", views.len() - 1));
    }
    vec![Span::styled(text, base.fg(color))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_runtime::{SourceSnapshot, Target};

    fn model_with(snapshot: SourceSnapshot<crate::Board>) -> Model {
        let mut model = Model::default();
        model.apply(snapshot);
        model
    }

    #[test]
    fn a_remote_hosts_failure_marks_the_list_but_not_a_local_rows_details() {
        let failed = SourceSnapshot::<()>::test_live(
            None,
            SourceState::Failed("offline".into()),
            Domain::Github,
        );
        let mut board = crate::Board::default();
        board
            .host_domains
            .insert("box".into(), failed.domains().clone());
        let model = model_with(SourceSnapshot::test_live(
            Some(board),
            SourceState::Ready,
            Domain::Git,
        ));
        assert!(status(&model, LIST).failed.is_some());
        assert!(host_status(&model, None, DETAILS).failed.is_none());
        assert!(host_status(&model, Some("box"), DETAILS).failed.is_some());
    }

    #[test]
    fn restored_failed_and_pending_states_render_the_same_way_everywhere() {
        let restored = SourceSnapshot::test_restored(crate::Board::default(), Domain::Github, 0);
        let model = model_with(restored);
        let shown = status(&model, LIST);
        assert!(shown.cached_since.is_some());
        let text: String = chip(&shown, Style::new())
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert!(text.starts_with(" · cached "), "{text}");
        // A pane that does not show GitHub is not marked stale by it.
        assert!(chip(&super::status(&model, OUTPUT), Style::new()).is_empty());

        let failed = SourceSnapshot::test_live(
            Some(crate::Board::default()),
            SourceState::Failed("offline".into()),
            Domain::Git,
        );
        let model = model_with(failed);
        let text: String = chip(&status(&model, DETAILS), Style::new())
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert_eq!(text, " · Git failed");

        let views = [
            PendingView {
                target: Target::row(None, "a"),
                label: "arming merge".into(),
                phase: PendingPhase::Accepted,
            },
            PendingView {
                target: Target::row(None, "a"),
                label: "setting ready".into(),
                phase: PendingPhase::Unconfirmed,
            },
        ];
        let refs: Vec<_> = views.iter().collect();
        let text: String = pending_chip(&refs, Style::new())
            .iter()
            .map(|span| span.content.to_string())
            .collect();
        assert_eq!(text, "? setting ready (unconfirmed) +1");
    }
}
