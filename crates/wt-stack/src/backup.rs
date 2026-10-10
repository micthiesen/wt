use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio_util::sync::CancellationToken;
use wt_platform::process::ProcessRunner;
use wt_vcs::{GitRepository, RepositoryKind};

use crate::{
    git::run_git,
    service::{StackConfig, StackError, StackEvent},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PruneBackupsResult {
    pub deleted: Vec<String>,
    pub kept: Vec<String>,
}

pub(crate) async fn prune_backups(
    config: &StackConfig,
    repository: &GitRepository,
    processes: &ProcessRunner,
    older_than_days: u64,
    cancellation: &CancellationToken,
    on_event: &mut dyn FnMut(StackEvent),
) -> Result<PruneBackupsResult, StackError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let cutoff =
        now.saturating_sub(Duration::from_secs(older_than_days.saturating_mul(86_400)).as_millis());
    let mut result = PruneBackupsResult::default();
    prune_in(
        processes,
        &config.main_clone,
        cutoff,
        cancellation,
        on_event,
        &mut result,
    )
    .await?;
    let rows = repository.inventory(cancellation).await?;
    for row in rows
        .into_iter()
        .filter(|row| !row.is_main && row.kind == RepositoryKind::RiftClone)
    {
        prune_in(
            processes,
            Path::new(&row.target.path),
            cutoff,
            cancellation,
            on_event,
            &mut result,
        )
        .await?;
    }
    Ok(result)
}

async fn prune_in(
    processes: &ProcessRunner,
    cwd: &Path,
    cutoff: u128,
    cancellation: &CancellationToken,
    on_event: &mut dyn FnMut(StackEvent),
    result: &mut PruneBackupsResult,
) -> Result<(), StackError> {
    let output = run_git(
        processes,
        cwd,
        [
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/backup/",
        ],
        cancellation,
    )
    .await?;
    if !output.status.success() {
        result
            .kept
            .extend(output.stdout_text().lines().map(str::to_owned));
        return Ok(());
    }
    for reference in output
        .stdout_text()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Some(timestamp) = backup_timestamp(reference) else {
            result.kept.push(reference.to_owned());
            continue;
        };
        if timestamp > cutoff {
            result.kept.push(reference.to_owned());
            continue;
        }
        let deleted = run_git(processes, cwd, ["branch", "-D", reference], cancellation).await?;
        if deleted.status.success() {
            result.deleted.push(reference.to_owned());
            on_event(StackEvent::Log(format!(
                "deleted {reference}{}",
                if cwd == Path::new(".") {
                    String::new()
                } else {
                    format!(" in {}", cwd.display())
                }
            )));
        } else {
            result.kept.push(reference.to_owned());
            on_event(StackEvent::Log(format!(
                "could not delete {reference}: {}",
                deleted.stderr_text().trim()
            )));
        }
    }
    Ok(())
}

pub(crate) fn backup_owner(reference: &str) -> Option<&str> {
    if let Some(rest) = reference.strip_prefix("backup/restack-") {
        let (timestamp, branch) = rest.split_once('-')?;
        return (timestamp.parse::<u128>().is_ok() && !branch.is_empty()).then_some(branch);
    }
    let rest = reference.strip_prefix("backup/stack-sync-")?;
    let suffix = rest.split_once("Z-")?;
    (!suffix.1.is_empty()).then_some(suffix.1)
}

fn backup_timestamp(reference: &str) -> Option<u128> {
    if let Some(rest) = reference.strip_prefix("backup/restack-") {
        return rest.split_once('-')?.0.parse().ok();
    }
    let rest = reference.strip_prefix("backup/stack-sync-")?;
    let timestamp = rest.split_once("Z-")?.0;
    let (date, clock) = timestamp.split_once('T')?;
    let mut date_parts = date.split('-');
    let year = date_parts.next()?.parse::<i32>().ok()?;
    let month = date_parts.next()?.parse::<u8>().ok()?;
    let day = date_parts.next()?.parse::<u8>().ok()?;
    if date_parts.next().is_some() || clock.len() != 9 {
        return None;
    }
    let hour = clock.get(..2)?.parse::<u8>().ok()?;
    let minute = clock.get(2..4)?.parse::<u8>().ok()?;
    let second = clock.get(4..6)?.parse::<u8>().ok()?;
    let millisecond = clock.get(6..)?.parse::<u16>().ok()?;
    let date =
        time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()?;
    let clock = time::Time::from_hms_milli(hour, minute, second, millisecond).ok()?;
    u128::try_from(
        time::PrimitiveDateTime::new(date, clock)
            .assume_utc()
            .unix_timestamp_nanos(),
    )
    .ok()
    .map(|nanos| nanos / 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_owner_keeps_dashed_branch_names() {
        assert_eq!(
            backup_owner("backup/restack-1234-feature/one-a"),
            Some("feature/one-a")
        );
        assert_eq!(
            backup_owner("backup/stack-sync-2026-10-01T120000000Z-feature/a"),
            Some("feature/a")
        );
        assert_eq!(backup_owner("backup/other-feature"), None);
    }
}
