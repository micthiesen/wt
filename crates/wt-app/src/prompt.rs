//! CLI prompts use an owned nonblocking terminal descriptor. Cancellation does
//! not leave a blocking stdin reader behind during runtime shutdown.

use anyhow::{Context, Result, bail};
use std::io::{IsTerminal, Write};
use tokio_util::sync::CancellationToken;

pub async fn read_line(message: &str, cancel: &CancellationToken) -> Result<Option<String>> {
    if !std::io::stdin().is_terminal() {
        bail!("interactive input requires a terminal");
    }
    eprint!("{}", wt_core::sanitize_terminal_text(message));
    std::io::stderr().flush()?;
    #[cfg(unix)]
    {
        use std::{fs::OpenOptions, io::Read, os::unix::fs::OpenOptionsExt};
        use tokio::io::unix::AsyncFd;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/tty")
            .context("open interactive terminal")?;
        let file = AsyncFd::new(file)?;
        let mut input = Vec::new();
        loop {
            let mut ready = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("interactive input cancelled"),
                ready = file.readable() => ready?,
            };
            let mut buffer = [0u8; 1024];
            let size = match ready.try_io(|fd| fd.get_ref().read(&mut buffer)) {
                Ok(result) => result?,
                Err(_) => continue,
            };
            if size == 0 {
                return Ok(None);
            }
            if let Some(end) = buffer[..size].iter().position(|&byte| byte == b'\n') {
                input.extend_from_slice(&buffer[..end]);
                if input.len() > 8192 {
                    bail!("terminal input exceeded 8192 bytes");
                }
                return Ok(Some(
                    String::from_utf8(input).context("terminal input was not UTF-8")?,
                ));
            }
            input.extend_from_slice(&buffer[..size]);
            if input.len() > 8192 {
                bail!("terminal input exceeded 8192 bytes");
            }
        }
    }
    #[cfg(not(unix))]
    bail!("interactive prompts currently require a Unix terminal");
}

pub async fn pick(
    options: &[String],
    label: &str,
    cancel: &CancellationToken,
) -> Result<Option<usize>> {
    for (index, option) in options.iter().enumerate() {
        eprintln!(
            "  {}) {}",
            index + 1,
            wt_core::sanitize_terminal_text(option)
        );
    }
    loop {
        let Some(answer) = read_line(
            &format!("{label} [1-{}, Enter cancels]: ", options.len()),
            cancel,
        )
        .await?
        else {
            return Ok(None);
        };
        if answer.trim().is_empty() {
            return Ok(None);
        }
        if let Ok(index) = answer.trim().parse::<usize>()
            && (1..=options.len()).contains(&index)
        {
            return Ok(Some(index - 1));
        }
        eprintln!("Enter a number from the list, or Enter to cancel.");
    }
}

pub async fn confirm(message: &str, default_yes: bool, cancel: &CancellationToken) -> Result<bool> {
    Ok(read_line(message, cancel).await?.is_some_and(|answer| {
        let answer = answer.trim().to_ascii_lowercase();
        matches!(answer.as_str(), "y" | "yes") || (default_yes && answer.is_empty())
    }))
}
