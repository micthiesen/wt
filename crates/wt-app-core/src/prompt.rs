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
        use std::io::Read;
        use tokio::io::unix::AsyncFd;
        let file = open_terminal(libc::STDIN_FILENO)?;
        let file = AsyncFd::new(file).context("register interactive terminal")?;
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

/// Open a separate descriptor so nonblocking mode does not change stdin.
/// macOS kqueue rejects /dev/tty; use the concrete device named by stdin.
#[cfg(unix)]
fn open_terminal(fd: std::os::fd::RawFd) -> Result<std::fs::File> {
    use std::{
        ffi::CStr,
        fs::OpenOptions,
        os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    };
    let mut name = vec![0 as libc::c_char; 1024];
    // The buffer is writable for its stated size; ttyname_r adds a NUL on success.
    let error = unsafe { libc::ttyname_r(fd, name.as_mut_ptr(), name.len()) };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error))
            .context("resolve interactive terminal");
    }
    // A successful ttyname_r call returned a NUL-terminated device path.
    let name = unsafe { CStr::from_ptr(name.as_ptr()) };
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(std::ffi::OsStr::from_bytes(name.to_bytes()))
        .context("open interactive terminal")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::{AsRawFd, FromRawFd},
    };

    #[tokio::test]
    async fn terminal_device_supports_async_input_without_changing_stdin_flags() {
        let mut master = -1;
        let mut slave = -1;
        // openpty initializes both descriptors on success; no optional settings are supplied.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // Each descriptor is owned once and closed when its File is dropped.
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let flags = unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) };
        let terminal =
            tokio::io::unix::AsyncFd::new(open_terminal(slave.as_raw_fd()).unwrap()).unwrap();
        assert_eq!(
            unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_GETFL) },
            flags
        );
        master.write_all(b"yes\n").unwrap();
        let mut input = [0; 16];
        let count = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let mut ready = terminal.readable().await.unwrap();
                if let Ok(result) = ready.try_io(|fd| fd.get_ref().read(&mut input)) {
                    break result.unwrap();
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(&input[..count], b"yes\n");
    }
}
