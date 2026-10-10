//! Bounded, cancellable subprocess execution.
//!
//! A permit covers queue admission through process reaping and pipe cleanup.
//! Every command owns a Unix process group so cancellation also reaches children
//! that inherited stdout/stderr. Intentional daemons must detach themselves.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Semaphore;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;

const TERMINATION_GRACE: Duration = Duration::from_millis(500);
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);
type StreamObserver = Arc<dyn Fn(ProcessStream, &[u8]) + Send + Sync>;

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    /// `None` removes a variable from the inherited environment.
    pub env: Vec<(OsString, Option<OsString>)>,
    pub input: Option<Vec<u8>>,
    /// Includes queue wait, execution, and output collection.
    pub timeout: Duration,
    pub output_limit: usize,
    /// Explicit ownership transfer for desktop launchers/clipboard providers.
    /// Only a successful leader exit with closed pipes releases its children.
    /// Errors, cancellation and timeouts still terminate the entire group.
    pub preserve_children_on_success: bool,
}

impl CommandSpec {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            input: None,
            timeout: Duration::from_secs(45),
            output_limit: 16 * 1024 * 1024,
            preserve_children_on_success: false,
        }
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
}

#[derive(Debug)]
pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// True when a streaming caller's bounded in-memory capture omitted bytes.
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessStream {
    Stdout,
    Stderr,
}

impl ProcessOutput {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Nonzero exit is data until a caller requires success (for example,
    /// `git merge-base --is-ancestor` deliberately uses exit status as an answer).
    pub fn checked(self, program: impl AsRef<OsStr>) -> Result<Self, ProcessError> {
        if self.status.success() {
            Ok(self)
        } else {
            Err(ProcessError::Exit {
                program: program.as_ref().to_string_lossy().into_owned(),
                code: self.status.code(),
                stderr: self.stderr_text(),
                stdout: self.stdout_text(),
            })
        }
    }
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("{program}: {operation}: {source}")]
    Io {
        program: String,
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("{program}: cancelled")]
    Cancelled { program: String },
    #[error("{program}: timed out after {timeout:?}")]
    Timeout { program: String, timeout: Duration },
    #[error("{program}: {stream} exceeded the {limit}-byte capture limit")]
    OutputLimit {
        program: String,
        stream: &'static str,
        limit: usize,
    },
    #[error("{program}: output pipes remained open after the process exited")]
    PipeDrain { program: String },
    #[error("{program}: streaming observer panicked")]
    ObserverPanicked { program: String },
    #[error("{program}: exited with {code:?}: {stderr}")]
    Exit {
        program: String,
        code: Option<i32>,
        stderr: String,
        stdout: String,
    },
}

#[derive(Clone)]
pub struct ProcessRunner {
    permits: Arc<Semaphore>,
}

impl ProcessRunner {
    pub fn new(max_active: std::num::NonZeroUsize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_active.get())),
        }
    }

    pub async fn run(
        &self,
        spec: CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<ProcessOutput, ProcessError> {
        // Pipe capture holds sizeable read buffers across awaits. Keep that
        // state on the heap once, rather than embedding it through every
        // service future up to a small-stack runtime worker.
        Box::pin(self.run_inner(spec, cancellation, None)).await
    }

    /// Run with bounded in-memory capture while forwarding every read chunk
    /// to a synchronous, non-blocking observer. Once either capture reaches
    /// `output_limit`, the remaining bytes are still drained and streamed but
    /// omitted from the returned buffer; the corresponding truncation flag is
    /// set. The observer must not block on I/O. A bounded `try_send` into an
    /// owned writer task is the intended use; a full queue may drop log chunks
    /// but cannot stall pipe draining or prevent cancellation/reaping.
    pub async fn run_streaming<F>(
        &self,
        spec: CommandSpec,
        cancellation: &CancellationToken,
        observer: F,
    ) -> Result<ProcessOutput, ProcessError>
    where
        F: Fn(ProcessStream, &[u8]) + Send + Sync + 'static,
    {
        Box::pin(self.run_inner(spec, cancellation, Some(Arc::new(observer)))).await
    }

    async fn run_inner(
        &self,
        spec: CommandSpec,
        cancellation: &CancellationToken,
        observer: Option<StreamObserver>,
    ) -> Result<ProcessOutput, ProcessError> {
        let started = Instant::now();
        let deadline = started + spec.timeout;
        let program = spec.program.to_string_lossy().into_owned();
        let cancelled = || ProcessError::Cancelled {
            program: program.clone(),
        };
        let timed_out = || ProcessError::Timeout {
            program: program.clone(),
            timeout: spec.timeout,
        };
        let _permit = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(cancelled()),
            _ = tokio::time::sleep_until(deadline) => return Err(timed_out()),
            permit = self.permits.acquire() => permit.expect("runner semaphore is never closed"),
        };
        // Cancellation can arrive together with the permit. Do not launch work
        // whose caller already withdrew it.
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        if Instant::now() >= deadline {
            return Err(timed_out());
        }

        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(if spec.input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        for (name, value) in &spec.env {
            if let Some(value) = value {
                command.env(name, value);
            } else {
                command.env_remove(name);
            }
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|source| ProcessError::Io {
            program: program.clone(),
            operation: "spawn",
            source,
        })?;
        let mut group = ProcessGroup::new(child.id());
        let stdout = child.stdout.take().expect("stdout requested as pipe");
        let stderr = child.stderr.take().expect("stderr requested as pipe");
        let stdin = child.stdin.take();
        let capture_program = program.clone();
        let stdout_observer = observer.clone();
        let stderr_observer = observer;
        let output_limit = spec.output_limit;
        let captures = async {
            let (stdout, stderr, ()) = tokio::try_join!(
                capture(
                    stdout,
                    output_limit,
                    "stdout",
                    ProcessStream::Stdout,
                    &capture_program,
                    stdout_observer,
                ),
                capture(
                    stderr,
                    output_limit,
                    "stderr",
                    ProcessStream::Stderr,
                    &capture_program,
                    stderr_observer,
                ),
                async {
                    if let (Some(mut stdin), Some(input)) = (stdin, spec.input) {
                        stdin
                            .write_all(&input)
                            .await
                            .map_err(|source| ProcessError::Io {
                                program: capture_program.clone(),
                                operation: "write stdin",
                                source,
                            })?;
                        // Dropping the pipe sends EOF before waiting for the child.
                    }
                    Ok(())
                },
            )?;
            Ok::<_, ProcessError>((stdout, stderr))
        };
        tokio::pin!(captures);
        let mut captured = None;
        let result = loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break Err(cancelled()),
                _ = tokio::time::sleep_until(deadline) => break Err(timed_out()),
                output = &mut captures, if captured.is_none() => {
                    match output {
                        Ok(output) => captured = Some(output),
                        Err(error) => break Err(error),
                    }
                }
                status = child.wait() => {
                    let status = match status {
                        Ok(status) => status,
                        Err(source) => break Err(ProcessError::Io {
                            program: program.clone(), operation: "wait", source,
                        }),
                    };
                    let output = match captured.take() {
                        Some(output) => Ok(output),
                        None => tokio::select! {
                            biased;
                            _ = cancellation.cancelled() => Err(cancelled()),
                            _ = tokio::time::sleep_until(deadline) => Err(timed_out()),
                            output = timeout(PIPE_DRAIN_GRACE, &mut captures) => match output {
                                Ok(output) => output,
                                Err(_) => Err(ProcessError::PipeDrain { program: program.clone() }),
                            },
                        },
                    };
                    break output.map(|((stdout, stdout_truncated), (stderr, stderr_truncated))| ProcessOutput {
                        status, stdout, stderr, stdout_truncated, stderr_truncated,
                        elapsed: started.elapsed(),
                    });
                }
            }
        };
        if result.is_err() {
            terminate(&mut child, &group).await;
        } else if !spec.preserve_children_on_success
            || result.as_ref().is_ok_and(|output| !output.status.success())
        {
            // A leader can exit successfully while background children close
            // their pipes and keep running. They still belong to this command.
            // A service intentionally detached into its own group is unaffected.
            group.signal(libc::SIGKILL);
        }
        group.disarm();
        tracing::debug!(
            program,
            elapsed_ms = started.elapsed().as_millis() as u64,
            success = result.is_ok(),
            "subprocess completed"
        );
        result
    }
}

impl Default for ProcessRunner {
    fn default() -> Self {
        Self::new(std::num::NonZeroUsize::new(8).expect("eight is nonzero"))
    }
}

async fn capture(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
    stream: &'static str,
    process_stream: ProcessStream,
    program: &str,
    observer: Option<StreamObserver>,
) -> Result<(Vec<u8>, bool), ProcessError> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|source| ProcessError::Io {
                program: program.into(),
                operation: stream,
                source,
            })?;
        if read == 0 {
            return Ok((bytes, truncated));
        }
        if let Some(observer) = &observer {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                observer(process_stream, &buffer[..read]);
            }))
            .map_err(|_| ProcessError::ObserverPanicked {
                program: program.into(),
            })?;
        }
        let remaining = limit.saturating_sub(bytes.len());
        let captured = read.min(remaining);
        bytes.extend_from_slice(&buffer[..captured]);
        if captured < read {
            if observer.is_none() {
                return Err(ProcessError::OutputLimit {
                    program: program.into(),
                    stream,
                    limit,
                });
            }
            truncated = true;
        }
    }
}

struct ProcessGroup {
    pid: Option<u32>,
}

impl ProcessGroup {
    fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }
    fn disarm(&mut self) {
        self.pid = None;
    }
    fn signal(&self, signal: i32) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // SAFETY: spawn creates a group led by this child. A negative pid
            // targets that owned group. kill does not dereference pointers.
            unsafe {
                libc::kill(-(pid as i32), signal);
            }
        }
        #[cfg(not(unix))]
        let _ = signal;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Last-resort cleanup when the caller aborts/drops the future, including
        // while graceful cancellation is itself waiting. Child also kills/reaps
        // its direct process through Tokio's kill_on_drop behavior.
        self.signal(libc::SIGKILL);
    }
}

async fn terminate(child: &mut Child, group: &ProcessGroup) {
    group.signal(libc::SIGTERM);
    let _ = timeout(TERMINATION_GRACE, child.wait()).await;
    // The leader may have exited while descendants are still alive.
    group.signal(libc::SIGKILL);
    let _ = child.start_kill();
    let _ = child.wait().await;
}
