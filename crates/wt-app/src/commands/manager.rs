use std::{
    fs::{self, OpenOptions},
    io::{IsTerminal, Read, Write},
    path::Path,
    process::Stdio,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::process::Command;
use wt_tui::SessionTarget;

use crate::{commands::agent::send_to, context::AppContext, harness::ui_session};

#[derive(Debug, Clone, Args)]
pub struct ManagerArgs {
    #[command(subcommand)]
    pub command: Option<ManagerCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ManagerCommand {
    Send {
        #[arg(long)]
        hold: Option<String>,
        #[arg(trailing_var_arg = true)]
        text: Vec<String>,
    },
    Report {
        #[arg(long, conflicts_with_all = ["warn", "err", "info"])]
        ok: bool,
        #[arg(long, conflicts_with_all = ["err", "info"])]
        warn: bool,
        #[arg(long, conflicts_with = "info")]
        err: bool,
        #[arg(long)]
        info: bool,
        #[arg(trailing_var_arg = true, required = true)]
        text: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ReportLevel {
    #[default]
    Info,
    Ok,
    Warn,
    Err,
}

#[derive(Serialize)]
struct Report<'a> {
    at: String,
    level: &'a str,
    text: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagerReport {
    pub at: String,
    pub level: ReportLevel,
    pub text: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ManagerReportsBatch {
    pub reports: Vec<ManagerReport>,
    pub next_offset: usize,
}

pub async fn run(context: &AppContext, args: &ManagerArgs) -> Result<i32> {
    match &args.command {
        Some(ManagerCommand::Send { hold, text }) => {
            let text = text.join(" ").trim().to_owned();
            if text.is_empty() {
                bail!("wt manager send requires a message");
            }
            send_to(context, "manager", &text, hold.as_deref()).await
        }
        Some(ManagerCommand::Report {
            ok,
            warn,
            err,
            text,
            ..
        }) => {
            let text = text.join(" ").trim().to_owned();
            if text.is_empty() {
                bail!("wt manager report requires a message");
            }
            let level = if *ok {
                ReportLevel::Ok
            } else if *warn {
                ReportLevel::Warn
            } else if *err {
                ReportLevel::Err
            } else {
                ReportLevel::Info
            };
            append_report(
                context
                    .config
                    .paths
                    .cache_root
                    .join("manager/reports.jsonl")
                    .as_path(),
                level,
                &text,
            )?;
            println!(
                "✓ reported\n» surfaces on the wt attention feed (a running TUI picks it up live)"
            );
            Ok(0)
        }
        None => attach(context).await,
    }
}

async fn attach(context: &AppContext) -> Result<i32> {
    if !std::io::stdout().is_terminal() {
        bail!("wt manager (attach) needs a TTY; did you mean `wt manager send`?");
    }
    let ticket = ui_session(context, None, SessionTarget::Manager).await?;
    let mut command = Command::new(&ticket.program);
    command
        .args(&ticket.args)
        .current_dir(&ticket.cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env_remove("TMUX")
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("start tmux manager attachment")?;
    let status = tokio::select! {
        biased;
        _ = context.cancellation.cancelled() => {
            child.start_kill().context("stop manager attachment")?;
            child.wait().await.context("reap manager attachment")?;
            return Ok(0);
        }
        status = child.wait() => status.context("wait for manager attachment")?,
    };
    Ok(status.code().unwrap_or(1))
}

pub fn append_report(path: &Path, level: ReportLevel, text: &str) -> Result<()> {
    if text.is_empty() {
        bail!("manager report requires a message");
    }
    let parent = path.parent().context("manager report path has no parent")?;
    fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("jsonl.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lock_file(&lock)?;
    let mut current = fs::read(path).unwrap_or_default();
    if current.len() > 64 * 1024 {
        current = current
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .take(100)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .flat_map(|line| [line.to_vec(), vec![b'\n']])
            .flatten()
            .collect();
        fs::write(path, current)?;
    }
    let at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format report timestamp")?;
    let level = match level {
        ReportLevel::Info => "info",
        ReportLevel::Ok => "ok",
        ReportLevel::Warn => "warn",
        ReportLevel::Err => "err",
    };
    let mut line = serde_json::to_vec(&Report { at, level, text })?;
    line.push(b'\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&line)?;
    unlock_file(&lock);
    Ok(())
}

/// Read only complete JSONL records. A shrinking spool resets the cursor to
/// zero so a rotation cannot strand the watcher beyond the new file length.
pub fn read_reports_from(path: &Path, offset: usize) -> Result<ManagerReportsBatch> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManagerReportsBatch::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read manager reports {}", path.display()));
        }
    };
    let mut bytes = Vec::new();
    file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 4 * 1024 * 1024 {
        bail!("manager report spool exceeds 4 MiB: {}", path.display());
    }
    let start = if bytes.len() < offset { 0 } else { offset };
    if bytes.len() == start {
        return Ok(ManagerReportsBatch {
            reports: vec![],
            next_offset: start,
        });
    }
    let suffix = &bytes[start..];
    let Some(last_newline) = suffix.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(ManagerReportsBatch {
            reports: vec![],
            next_offset: start,
        });
    };
    let complete = &suffix[..=last_newline];
    let mut reports = Vec::new();
    for line in complete
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let Some(text) = value
            .get("text")
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        let level = match value.get("level").and_then(serde_json::Value::as_str) {
            Some("ok") => ReportLevel::Ok,
            Some("warn") => ReportLevel::Warn,
            Some("err") => ReportLevel::Err,
            _ => ReportLevel::Info,
        };
        reports.push(ManagerReport {
            at: value
                .get("at")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            level,
            text: text.to_owned(),
        });
    }
    Ok(ManagerReportsBatch {
        reports,
        next_offset: start + complete.len(),
    })
}

#[cfg(unix)]
fn lock_file(file: &std::fs::File) -> Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock acts on the owned lock descriptor and does not retain it.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(unix)]
fn unlock_file(file: &std::fs::File) {
    use std::os::fd::AsRawFd;
    // SAFETY: releases the lock while the descriptor is still open.
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}
#[cfg(not(unix))]
fn lock_file(_: &std::fs::File) -> Result<()> {
    Ok(())
}
#[cfg(not(unix))]
fn unlock_file(_: &std::fs::File) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn reports_append_jsonl_and_keep_spool_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manager/reports.jsonl");
        append_report(&path, ReportLevel::Warn, "first result").unwrap();
        let value: Value = serde_json::from_slice(fs::read(&path).unwrap().trim_ascii()).unwrap();
        assert_eq!(value["level"], "warn");
        assert_eq!(value["text"], "first result");
        fs::write(
            &path,
            (0..130)
                .map(|i| format!("{{\"text\":{i}}}\n"))
                .collect::<String>()
                .repeat(400),
        )
        .unwrap();
        append_report(&path, ReportLevel::Info, "last").unwrap();
        let rows = fs::read_to_string(path).unwrap();
        assert!(rows.lines().count() <= 101);
        assert!(rows.ends_with("\"text\":\"last\"}\n"));
    }

    #[test]
    fn report_reader_waits_for_complete_lines_and_recovers_after_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reports.jsonl");
        let first = b"{\"at\":\"a\",\"level\":\"ok\",\"text\":\"one\"}\n";
        fs::write(&path, [first.as_slice(), b"{\"text\":\"par"].concat()).unwrap();
        let batch = read_reports_from(&path, 0).unwrap();
        assert_eq!(batch.reports.len(), 1);
        assert_eq!(batch.reports[0].text, "one");
        assert_eq!(batch.next_offset, first.len());
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"tial\"}\n").unwrap();
        let batch = read_reports_from(&path, batch.next_offset).unwrap();
        assert_eq!(batch.reports.len(), 1);
        assert_eq!(batch.reports[0].text, "partial");
        fs::write(&path, b"{\"text\":\"rotated\"}\n").unwrap();
        let batch = read_reports_from(&path, 10000).unwrap();
        assert_eq!(batch.reports[0].text, "rotated");
    }
}
