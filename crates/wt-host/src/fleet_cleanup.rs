//! One cleanup confirmation, built from the board, over candidates on every
//! configured host. Each candidate stays tied to its host through the commit.

use std::collections::BTreeMap;

use tokio::task::JoinSet;
use wt_tui::{CleanupTarget, ReplyOutcome, UiAction, UiReply};

use crate::{remote_board::Fleet, remote_host::RemoteHostError};

/// Route the rows the combined confirmation captured to the hosts that own
/// them. Unknown or reconfigured hosts are reported and skipped; each host
/// checks its own rows again before it removes anything.
pub async fn cleanup_targets(fleet: &Fleet, targets: Vec<CleanupTarget>) -> UiReply {
    let hosts = fleet
        .remotes
        .iter()
        .map(|remote| remote.endpoint.key())
        .collect::<Vec<_>>();
    let mut partition = partition_items(targets, &hosts);
    let mut lines = std::mem::take(&mut partition.lines);
    let mut summary = CleanupSummary {
        failed: partition.failed,
        ..Default::default()
    };
    let local = std::mem::take(&mut partition.local);
    let remote = std::mem::take(&mut partition.remote);

    let mut tasks = JoinSet::new();
    if !local.is_empty() {
        let service = fleet.local.clone();
        let revisions = local;
        tasks.spawn(async move {
            (
                "This machine".to_owned(),
                revisions.len(),
                service
                    .execute(UiAction::CleanupTargets { targets: revisions })
                    .await,
            )
        });
    }
    let mut remote_work = Vec::new();
    for (host, revisions) in remote {
        let count = revisions.len();
        let Some(endpoint) = fleet
            .remotes
            .iter()
            .find(|remote| remote.endpoint.key() == host)
            .cloned()
        else {
            // The configuration cannot change during this call, but retain a
            // fail-closed guard should that invariant change later.
            summary.failed = true;
            lines.push(format!("Unavailable host {host}: candidates skipped"));
            continue;
        };
        remote_work.push((endpoint, revisions, count));
    }
    let mut remote_work = remote_work.into_iter();
    let initial_remote_tasks = 4usize.saturating_sub(tasks.len());
    for (endpoint, revisions, count) in remote_work.by_ref().take(initial_remote_tasks) {
        tasks.spawn(async move {
            let label = wt_core::sanitize_terminal_text(&endpoint.endpoint.label);
            (
                label,
                count,
                endpoint
                    .execute(UiAction::CleanupTargets { targets: revisions })
                    .await
                    .map_err(anyhow::Error::from),
            )
        });
    }

    while let Some(result) = tasks.join_next().await {
        match result {
            // An ambiguous remote error is surfaced once; this coordinator
            // never retries a potentially delivered write.
            Ok((label, count, Err(error))) if RemoteHostError::is_ambiguous(&error) => {
                summary.uncertain = true;
                lines.push(format!(
                    "{label}: cleanup of {count} candidate(s) is unconfirmed: {error:#}; the result is unknown and will not be resent; check before retrying"
                ));
            }
            Ok((label, count, Err(error))) => {
                summary.failed = true;
                lines.push(format!(
                    "{label}: cleanup of {count} candidate(s) failed: {error:#}"
                ));
            }
            Ok((label, count, Ok(reply))) => {
                summary.record(&reply);
                let detail = if reply.message.trim().is_empty() {
                    format!("cleanup request finished for {count} candidate(s)")
                } else {
                    reply.message
                };
                lines.push(format!("{label}: {detail}"));
            }
            Err(error) => {
                summary.failed = true;
                lines.push(format!("cleanup request ended unexpectedly: {error}"));
            }
        }
        if let Some((endpoint, revisions, count)) = remote_work.next() {
            tasks.spawn(async move {
                let label = wt_core::sanitize_terminal_text(&endpoint.endpoint.label);
                (
                    label,
                    count,
                    endpoint
                        .execute(UiAction::CleanupTargets { targets: revisions })
                        .await
                        .map_err(anyhow::Error::from),
                )
            });
        }
    }

    summary.reply(lines.join("\n"))
}

/// What the owning hosts answered, folded into the one reply the pending
/// removal settles on. Each host classified its own write; this only combines
/// those answers and never upgrades an ambiguous one.
#[derive(Default)]
struct CleanupSummary {
    failed: bool,
    uncertain: bool,
    accepted: bool,
}

impl CleanupSummary {
    fn record(&mut self, reply: &UiReply) {
        if reply.failed {
            self.failed = true;
            return;
        }
        match reply.outcome {
            Some(ReplyOutcome::Uncertain) => self.uncertain = true,
            Some(ReplyOutcome::NoChange | ReplyOutcome::Deferred) => {}
            Some(ReplyOutcome::Accepted(_)) | None => self.accepted = true,
        }
    }

    fn reply(self, message: String) -> UiReply {
        if self.failed {
            return UiReply {
                message,
                failed: true,
                ..Default::default()
            };
        }
        if self.uncertain {
            return UiReply::uncertain(message);
        }
        UiReply {
            // Hosts stamp on their own clocks. The pending removal spans
            // hosts and is confirmed by this process's board, so it takes a
            // stamp here, after the last host answered.
            outcome: Some(if self.accepted {
                ReplyOutcome::Accepted(wt_runtime::Stamp::now())
            } else {
                ReplyOutcome::NoChange
            }),
            ..UiReply::message(message)
        }
    }
}

struct CleanupPartition {
    local: Vec<CleanupTarget>,
    remote: BTreeMap<String, Vec<CleanupTarget>>,
    lines: Vec<String>,
    failed: bool,
}

fn partition_items(revisions: Vec<CleanupTarget>, configured_hosts: &[String]) -> CleanupPartition {
    let mut partition = CleanupPartition {
        local: vec![],
        remote: BTreeMap::new(),
        lines: vec![],
        failed: false,
    };
    for mut revision in revisions {
        match wt_core::parse_worktree_ledger_key(&revision.key) {
            Some(wt_core::WorktreeRef::Local { .. }) => partition.local.push(revision),
            Some(wt_core::WorktreeRef::Remote { host, slug }) => {
                if !configured_hosts
                    .iter()
                    .any(|configured| configured == &host)
                {
                    partition.failed = true;
                    partition.lines.push(format!(
                        "Unavailable host {host}: skipped its confirmed cleanup candidates"
                    ));
                    continue;
                }
                revision.key = slug;
                partition.remote.entry(host).or_default().push(revision);
            }
            None => {
                partition.failed = true;
                partition.lines.push(format!(
                    "Invalid cleanup identity {:?}: candidate skipped",
                    revision.key
                ));
            }
        }
    }
    partition
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_reply_is_classified_from_the_hosts_answers() {
        let accepted = || UiReply {
            outcome: Some(ReplyOutcome::Accepted(wt_runtime::Stamp::now())),
            ..Default::default()
        };
        let fold = |replies: &[UiReply]| {
            let mut summary = CleanupSummary::default();
            for reply in replies {
                summary.record(reply);
            }
            summary.reply("done".into())
        };

        let before = wt_runtime::Stamp::now();
        let reply = fold(&[accepted(), UiReply::no_change("nothing")]);
        let Some(ReplyOutcome::Accepted(at)) = reply.outcome else {
            panic!("accepted removals must not be left unclassified: {reply:?}");
        };
        assert_eq!(at.at_or_after(&before), Some(true));
        assert!(!reply.failed && !reply.notice);

        let reply = fold(&[accepted(), UiReply::uncertain("lost")]);
        assert_eq!(reply.outcome, Some(ReplyOutcome::Uncertain));
        assert!(!reply.failed);

        let failed = UiReply {
            failed: true,
            ..Default::default()
        };
        let reply = fold(&[accepted(), UiReply::uncertain("lost"), failed]);
        assert!(reply.failed);
        assert_eq!(reply.outcome, None);

        assert_eq!(
            fold(&[UiReply::no_change("nothing")]).outcome,
            Some(ReplyOutcome::NoChange)
        );
        assert_eq!(fold(&[]).outcome, Some(ReplyOutcome::NoChange));
    }

    #[test]
    fn board_targets_keep_host_and_checkout_identity_through_partitioning() {
        let target = |key: String, path: &str| CleanupTarget {
            key,
            path: path.into(),
            branch: "feature/same".into(),
            head: "head".into(),
        };
        let partition = partition_items(
            vec![
                target("same".into(), "/local/same"),
                target(
                    wt_core::remote_worktree_ledger_key("builder", "same"),
                    "/remote/same",
                ),
                target(
                    wt_core::remote_worktree_ledger_key("unknown", "same"),
                    "/unknown/same",
                ),
            ],
            &["builder".into()],
        );
        assert_eq!(partition.local.len(), 1);
        assert_eq!(partition.local[0].path, "/local/same");
        assert_eq!(partition.remote["builder"][0].key, "same");
        assert_eq!(partition.remote["builder"][0].path, "/remote/same");
        assert!(partition.failed);
        assert!(!partition.remote.contains_key("unknown"));
    }

    fn revision(key: &str, path: &str) -> CleanupTarget {
        CleanupTarget {
            key: key.into(),
            path: path.into(),
            branch: format!("feature/{key}"),
            head: "0123456789abcdef".into(),
        }
    }

    #[test]
    fn confirmed_rows_partition_exactly_once_by_config_qualified_host() {
        let first_host = "builder:/etc/first.toml";
        let second_host = "builder:/etc/second.toml";
        let confirmed = vec![
            revision("same", "/local/same"),
            revision(
                &wt_core::remote_worktree_ledger_key(first_host, "same"),
                "/first/same",
            ),
            revision(
                &wt_core::remote_worktree_ledger_key(second_host, "same"),
                "/second/same",
            ),
            revision(
                &wt_core::remote_worktree_ledger_key("removed-host", "old"),
                "/old/old",
            ),
        ];
        let partition =
            partition_items(confirmed, &[first_host.to_owned(), second_host.to_owned()]);

        assert_eq!(partition.local.len(), 1);
        assert_eq!(partition.local[0].key, "same");
        assert_eq!(partition.remote.len(), 2);
        assert_eq!(partition.remote[first_host].len(), 1);
        assert_eq!(partition.remote[first_host][0].key, "same");
        assert_eq!(partition.remote[second_host].len(), 1);
        assert_eq!(partition.remote[second_host][0].key, "same");
        assert!(partition.failed);
        assert!(partition.lines[0].contains("removed-host"));
    }
}
