use std::path::Path;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, prelude::*};

pub fn initialize(directory: &Path) -> Result<WorkerGuard> {
    std::fs::create_dir_all(directory).context("create application log directory")?;
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("wt-native")
        .filename_suffix("log")
        .max_log_files(7)
        .build(directory)
        .context("open application log")?;
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(4096)
        .thread_name("wt-log")
        .finish(appender);
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("WT_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(writer)
                .with_ansi(false),
        )
        .try_init()
        .context("initialize application logging")?;
    Ok(guard)
}
