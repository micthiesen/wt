use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct FileLock(File);

impl FileLock {
    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: `file` owns a valid descriptor for the duration of the lock.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self(file))
    }

    pub(crate) async fn acquire_async(
        path: &Path,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            loop {
                if cancel.is_cancelled() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "cancelled while waiting for harness lock",
                    ));
                }
                // SAFETY: `file` owns a valid descriptor for the duration of the lock.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    return Ok(Self(file));
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::WouldBlock {
                    return Err(error);
                }
                tokio::select! {
                    _ = cancel.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled while waiting for harness lock")),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => (),
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = cancel;
            Ok(Self(file))
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: `file` still owns the descriptor being unlocked.
            let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

pub(crate) fn read_object(path: &Path) -> Value {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Default::default()))
}

pub(crate) fn atomic_write_json(path: &Path, value: &Value) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let tmp = PathBuf::from(format!(
        "{}.{}.{}.tmp",
        path.display(),
        std::process::id(),
        seq
    ));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        use std::io::Write;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
