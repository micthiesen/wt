//! One confirmation over cleanup candidates prepared independently by each
//! configured host. Revisions stay tied to their host through confirmation.

use std::collections::BTreeMap;

use tokio::task::JoinSet;
use wt_tui::{ConfirmAction, RemovalRevision, UiAction, UiModal, UiReply};

use crate::{remote_board::Fleet, remote_host::RemoteHost};

#[derive(Clone, Debug, PartialEq, Eq)]
struct HostIdentity {
    key: Option<String>,
    label: String,
}

struct PreparedHost {
    identity: HostIdentity,
    result: Result<UiReply, String>,
}

/// Collect the per-host lifecycle planner results and show one fleet-wide
/// confirmation. Every displayed candidate is the exact revision later sent
/// back to its owner; unavailable hosts are listed explicitly.
pub async fn prepare(fleet: &Fleet) -> UiReply {
    let mut tasks = JoinSet::new();
    let local = fleet.local.clone();
    let identity = HostIdentity {
        key: None,
        label: "This machine".into(),
    };
    tasks.spawn(async move {
        PreparedHost {
            identity,
            result: local
                .execute(UiAction::PrepareCleanup)
                .await
                .map_err(|error| format!("{error:#}")),
        }
    });
    let mut remotes = fleet.remotes.iter().cloned();
    for _ in 0..3 {
        let Some(remote) = remotes.next() else {
            break;
        };
        tasks.spawn(async move { prepare_remote(remote).await });
    }

    let mut prepared = Vec::new();
    let mut unexpected_failures = 0usize;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(host) => prepared.push(host),
            Err(_) => unexpected_failures += 1,
        }
        if let Some(remote) = remotes.next() {
            tasks.spawn(async move { prepare_remote(remote).await });
        }
    }
    aggregate_preparations(prepared, unexpected_failures)
}

fn aggregate_preparations(mut prepared: Vec<PreparedHost>, unexpected_failures: usize) -> UiReply {
    prepared.sort_by(|a, b| a.identity.key.cmp(&b.identity.key));
    let mut revisions = Vec::new();
    let mut lines = Vec::new();
    let mut failed = unexpected_failures > 0;
    for host in prepared {
        match host.result {
            Err(error) => {
                failed = true;
                lines.push(format!("{}: unavailable: {}", host.identity.label, error));
            }
            Ok(reply) => {
                if reply.failed {
                    failed = true;
                }
                let message = reply.message.clone();
                match reply.modal {
                    Some(UiModal::Confirm {
                        action:
                            ConfirmAction::Cleanup {
                                revisions: host_revisions,
                            },
                        lines: host_lines,
                        ..
                    }) => {
                        let start = revisions.len();
                        for revision in host_revisions {
                            match qualify_revision(revision, &host.identity) {
                                Ok(revision) => revisions.push(revision),
                                Err(error) => {
                                    failed = true;
                                    lines.push(format!(
                                        "{}: invalid cleanup candidate: {}",
                                        host.identity.label, error
                                    ));
                                }
                            }
                        }
                        if !message.trim().is_empty() {
                            lines.push(format!("{}: {message}", host.identity.label));
                        }
                        let had_lines = !host_lines.is_empty();
                        for line in host_lines {
                            lines.push(format!("{}: {line}", host.identity.label));
                        }
                        if revisions.len() == start && !had_lines {
                            lines.push(format!("{}: no safe candidates", host.identity.label));
                        }
                    }
                    Some(_) => {
                        failed = true;
                        lines.push(format!(
                            "{}: cleanup planner returned an unexpected response",
                            host.identity.label
                        ));
                    }
                    None => {
                        if !message.trim().is_empty() {
                            lines.push(format!("{}: {message}", host.identity.label));
                        } else {
                            lines.push(format!("{}: no safe candidates", host.identity.label));
                        }
                    }
                }
            }
        }
    }
    if unexpected_failures > 0 {
        lines.push(format!(
            "{unexpected_failures} host cleanup check(s) ended unexpectedly"
        ));
    }

    if revisions.is_empty() {
        return UiReply {
            message: lines.join("\n"),
            failed,
            ..Default::default()
        };
    }
    UiReply {
        message: String::new(),
        failed,
        modal: Some(UiModal::Confirm {
            title: format!("Clean {} landed worktree(s)?", revisions.len()),
            lines,
            action: ConfirmAction::Cleanup { revisions },
            cancel_key: Some('c'),
        }),
        ..Default::default()
    }
}

async fn prepare_remote(remote: RemoteHost) -> PreparedHost {
    let identity = HostIdentity {
        key: Some(remote.endpoint.key()),
        label: wt_core::sanitize_terminal_text(&remote.endpoint.label),
    };
    let result = remote
        .execute(UiAction::PrepareCleanup)
        .await
        .map_err(|error| error.to_string());
    PreparedHost { identity, result }
}

fn qualify_revision(
    mut revision: RemovalRevision,
    host: &HostIdentity,
) -> Result<RemovalRevision, String> {
    let reference = wt_core::parse_worktree_ledger_key(&revision.key)
        .ok_or_else(|| format!("invalid worktree identity {:?}", revision.key))?;
    match (host.key.as_deref(), reference) {
        (None, wt_core::WorktreeRef::Local { .. }) => Ok(revision),
        (None, wt_core::WorktreeRef::Remote { .. }) => {
            Err("local planner returned a remote identity".into())
        }
        (Some(_), wt_core::WorktreeRef::Remote { .. }) => {
            Err("remote planner returned an already-qualified identity".into())
        }
        (Some(key), wt_core::WorktreeRef::Local { slug }) => {
            revision.key = wt_core::remote_worktree_ledger_key(key, &slug);
            Ok(revision)
        }
    }
}

/// Route only the revisions captured by the combined confirmation. Unknown
/// or reconfigured hosts are reported and skipped; no host is replanned.
pub async fn cleanup(fleet: &Fleet, revisions: Vec<RemovalRevision>) -> UiReply {
    let hosts = fleet
        .remotes
        .iter()
        .map(|remote| remote.endpoint.key())
        .collect::<Vec<_>>();
    let mut partition = partition_revisions(revisions, &hosts);
    let mut lines = std::mem::take(&mut partition.lines);
    let mut failed = partition.failed;
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
                service.execute(UiAction::Cleanup { revisions }).await,
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
            failed = true;
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
                    .execute(UiAction::Cleanup { revisions })
                    .await
                    .map_err(anyhow::Error::from),
            )
        });
    }

    while let Some(result) = tasks.join_next().await {
        match result {
            Ok((label, count, Err(error))) => {
                failed = true;
                // In particular, an Ambiguous remote error is surfaced once;
                // this coordinator never retries a potentially delivered write.
                lines.push(format!(
                    "{label}: cleanup of {count} candidate(s) failed: {error:#}"
                ));
            }
            Ok((label, count, Ok(reply))) => {
                if reply.failed {
                    failed = true;
                }
                let detail = if reply.message.trim().is_empty() {
                    format!("cleanup request finished for {count} candidate(s)")
                } else {
                    reply.message
                };
                lines.push(format!("{label}: {detail}"));
            }
            Err(error) => {
                failed = true;
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
                        .execute(UiAction::Cleanup { revisions })
                        .await
                        .map_err(anyhow::Error::from),
                )
            });
        }
    }

    UiReply {
        message: lines.join("\n"),
        failed,
        ..Default::default()
    }
}

#[derive(Default)]
struct CleanupPartition {
    local: Vec<RemovalRevision>,
    remote: BTreeMap<String, Vec<RemovalRevision>>,
    lines: Vec<String>,
    failed: bool,
}

fn partition_revisions(
    revisions: Vec<RemovalRevision>,
    configured_hosts: &[String],
) -> CleanupPartition {
    let mut partition = CleanupPartition::default();
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

    fn revision(key: &str, path: &str) -> RemovalRevision {
        RemovalRevision {
            key: key.into(),
            path: path.into(),
            branch: format!("feature/{key}"),
            head: "0123456789abcdef".into(),
            digest: "digest".into(),
            hazards: Vec::new(),
            published_base: None,
        }
    }

    #[test]
    fn qualifying_same_slug_keeps_local_and_remote_candidates_distinct() {
        let local = HostIdentity {
            key: None,
            label: "This machine".into(),
        };
        let remote = HostIdentity {
            key: Some("builder:/etc/wt.toml".into()),
            label: "Builder".into(),
        };
        let local_revision = qualify_revision(revision("same", "/local/same"), &local).unwrap();
        let remote_revision = qualify_revision(revision("same", "/remote/same"), &remote).unwrap();

        assert_eq!(local_revision.key, "same");
        assert_eq!(
            remote_revision.key,
            wt_core::remote_worktree_ledger_key("builder:/etc/wt.toml", "same")
        );
        assert_ne!(local_revision.path, remote_revision.path);
    }

    #[test]
    fn revision_qualification_rejects_an_already_qualified_wrong_host() {
        let remote = HostIdentity {
            key: Some("builder".into()),
            label: "Builder".into(),
        };
        assert!(
            qualify_revision(
                revision(
                    &wt_core::remote_worktree_ledger_key("other", "same"),
                    "/other/same"
                ),
                &remote,
            )
            .is_err()
        );
    }

    #[test]
    fn aggregate_keeps_safe_rows_and_exposes_offline_host() {
        let local = HostIdentity {
            key: None,
            label: "This machine".into(),
        };
        let offline = HostIdentity {
            key: Some("builder".into()),
            label: "Builder".into(),
        };
        let reply = aggregate_preparations(
            vec![
                PreparedHost {
                    identity: local,
                    result: Ok(UiReply {
                        modal: Some(UiModal::Confirm {
                            action: ConfirmAction::Cleanup {
                                revisions: vec![revision("same", "/local/same")],
                            },
                            title: "local cleanup".into(),
                            lines: vec!["Remove same".into(), "Keep dirty: changed files".into()],
                            cancel_key: Some('c'),
                        }),
                        ..Default::default()
                    }),
                },
                PreparedHost {
                    identity: offline,
                    result: Err("worker is not connected".into()),
                },
            ],
            0,
        );
        assert!(reply.failed);

        let Some(UiModal::Confirm {
            action: ConfirmAction::Cleanup { revisions },
            lines,
            ..
        }) = reply.modal
        else {
            panic!("expected fleet cleanup confirmation");
        };
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0].key, "same");
        assert!(
            lines
                .iter()
                .any(|line| line == "This machine: Keep dirty: changed files")
        );
        assert!(
            lines
                .iter()
                .any(|line| line == "Builder: unavailable: worker is not connected")
        );
    }

    #[test]
    fn confirmed_revisions_partition_exactly_once_by_config_qualified_host() {
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
            partition_revisions(confirmed, &[first_host.to_owned(), second_host.to_owned()]);

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
