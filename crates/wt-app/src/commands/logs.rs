use crate::context::AppContext;
use anyhow::{Context, Result};
use clap::Args;
use regex::Regex;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::SystemTime,
};

#[derive(Debug, Clone, Args, Default)]
pub struct LogsArgs {
    /// Tail this worktree's destroy log, including retained removed worktrees.
    pub slug: Option<String>,
}

pub async fn run(context: &AppContext, args: &LogsArgs) -> Result<i32> {
    let Some(path) = latest(context, args.slug.as_deref()).await? else {
        println!(
            "No destroy logs{}.\nSession routing: wt agent ls; application logs: {}",
            args.slug
                .as_ref()
                .map(|slug| format!(" for {slug}"))
                .unwrap_or_default(),
            context.config.paths.app_log_dir.display()
        );
        return Ok(1);
    };
    eprintln!("→ {}", path.display());
    // This is an intentionally long-lived streaming child. Capturing it with
    // ProcessRunner would buffer an unbounded log rather than stream to the user.
    let mut command = tokio::process::Command::new("tail");
    command
        .args(["-n", "200", "-F"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("start destroy-log tail")?;
    let status = tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => {
            child.start_kill().context("stop destroy-log tail")?;
            child.wait().await.context("reap destroy-log tail")?;
            return Ok(0);
        }
        status = child.wait() => status.context("wait for destroy-log tail")?,
    };
    Ok(status.code().unwrap_or(1))
}

async fn entries(path: &Path) -> Result<Option<tokio::fs::ReadDir>> {
    match tokio::fs::read_dir(path).await {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read logs in {}", path.display())),
    }
}

async fn consider(path: PathBuf, best: &mut Option<(SystemTime, PathBuf)>) -> Result<()> {
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect log {}", path.display())),
    };
    if metadata.is_file() {
        let modified = metadata.modified()?;
        if best
            .as_ref()
            .is_none_or(|(previous, _)| modified > *previous)
        {
            *best = Some((modified, path));
        }
    }
    Ok(())
}

async fn latest(context: &AppContext, slug: Option<&str>) -> Result<Option<PathBuf>> {
    let mut best = None;
    let legacy = Regex::new(r"^(.+)-\d{4}-\d{2}-\d{2}T.*\.log$").expect("constant regex");
    if let Some(mut entries) = entries(&context.config.paths.log_dir).await? {
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(captures) = legacy.captures(&name)
                && slug.is_none_or(|slug| {
                    captures
                        .get(1)
                        .is_some_and(|matched| matched.as_str() == slug)
                })
            {
                consider(entry.path(), &mut best).await?;
            }
        }
    }
    if let Some(mut entries) = entries(&context.config.paths.lock_dir.join("destroy-jobs")).await? {
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            if entry.metadata().await?.len() > 64 * 1024 {
                continue;
            }
            let bytes = match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let record: serde_json::Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("read destroy job {}", path.display()))?;
            if slug.is_none_or(|slug| {
                record
                    .pointer("/target/key")
                    .and_then(|value| value.as_str())
                    == Some(slug)
            }) {
                // Stored JSON cannot redirect a log reader outside the job directory.
                consider(path.with_extension("log"), &mut best).await?;
            }
        }
    }
    Ok(best.map(|(_, path)| path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::CommandFixture;

    #[tokio::test]
    async fn retained_logs_resolve_exact_slugs_without_a_live_worktree() {
        let fixture = CommandFixture::new().await.unwrap();
        let logs = &fixture.ctx.config.paths.log_dir;
        tokio::fs::create_dir_all(logs).await.unwrap();
        let wanted = logs.join("gone-2026-10-09T10-00-00.log");
        tokio::fs::write(&wanted, "old checkout removed")
            .await
            .unwrap();
        tokio::fs::write(logs.join("gone-longer-2026-10-09T11-00-00.log"), "neighbor")
            .await
            .unwrap();
        assert_eq!(
            latest(&fixture.ctx, Some("gone")).await.unwrap(),
            Some(wanted)
        );
        let jobs = fixture.ctx.config.paths.lock_dir.join("destroy-jobs");
        tokio::fs::create_dir_all(&jobs).await.unwrap();
        tokio::fs::write(
            jobs.join("1-a.json"),
            r#"{"target":{"key":"removed"},"logPath":"/tmp/untrusted"}"#,
        )
        .await
        .unwrap();
        let retained = jobs.join("1-a.log");
        tokio::fs::write(&retained, "done").await.unwrap();
        assert_eq!(
            latest(&fixture.ctx, Some("removed")).await.unwrap(),
            Some(retained)
        );
        assert_eq!(latest(&fixture.ctx, Some("missing")).await.unwrap(), None);
        fixture.close().await.unwrap();
    }
}
