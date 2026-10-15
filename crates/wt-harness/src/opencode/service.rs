//! Shared native OpenCode V2 service connection for interactive and headless clients.
use super::{OpenCodeError, OpenCodePaths, operation_error};
use crate::persist::FileLock;
use std::{io, path::PathBuf};
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessRunner};
#[derive(serde::Deserialize)]
pub struct OpenCodeConnection {
    pub(super) url: String,
    pub(super) password: String,
    #[serde(default)]
    pid: Option<i32>,
}

impl OpenCodeConnection {
    fn validate(self) -> Result<Self, OpenCodeError> {
        if let Some(pid) = self.pid.filter(|pid| *pid > 0) {
            // ESRCH proves this recorded process is gone. A timeout or EPERM
            // never authorizes restarting a possibly live interactive service.
            if unsafe { libc::kill(pid, 0) } != 0
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return Err(operation_error(
                    "service",
                    "recorded native service process has exited; run `opencode service start` to recover, then retry the action",
                ));
            }
        }
        let local_port = self
            .url
            .strip_prefix("http://127.0.0.1:")
            .and_then(|port| port.parse::<u16>().ok())
            .filter(|port| *port != 0);
        if self.password.is_empty() || local_port.is_none() {
            return Err(operation_error(
                "service",
                "unsupported native service registration; expected an authenticated local OpenCode V2 service",
            ));
        }
        Ok(self)
    }
}

impl OpenCodeConnection {
    /// Pin transport without exposing its password in argv or restarting a busy service.
    pub fn pin_command(self, command: &mut CommandSpec) {
        let index = command
            .args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(command.args.len());
        command
            .args
            .splice(index..index, ["--server".into(), self.url.into()]);
        command
            .env
            .push(("OPENCODE_PASSWORD".into(), Some(self.password.into())));
    }
}
#[derive(Clone)]
pub struct OpenCodeService {
    registration: PathBuf,
}
impl OpenCodeService {
    pub fn for_home(home: impl Into<PathBuf>) -> Self {
        Self {
            registration: OpenCodePaths::new(home, PathBuf::new()).service_registration(),
        }
    }
    pub fn from_environment() -> Result<Self, OpenCodeError> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| operation_error("service", "HOME is not configured"))?;
        Ok(Self::for_home(PathBuf::from(home)))
    }
    /// Native implicit connection discovery may restart a healthy service when
    /// a different checkout is blocking its event loop during cold watcher boot.
    /// Resolve its local registration instead; explicit connections fail without
    /// taking ownership of service recovery. Never replay a failed API mutation.
    pub async fn connect(
        &self,
        program: &std::ffi::OsStr,
        runner: &ProcessRunner,
        cancel: &CancellationToken,
    ) -> Result<OpenCodeConnection, OpenCodeError> {
        let path = self.registration.clone();
        let read = || async {
            match tokio::fs::read(&path).await {
                Ok(bytes) => serde_json::from_slice::<OpenCodeConnection>(&bytes)
                    .map(Some)
                    .map_err(|_| operation_error("service", "invalid native service registration")),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        };
        if let Some(service) = read().await? {
            return service.validate();
        }
        // Serialize bootstrap only, across wt configurations. Once registration
        // exists, unrelated locations never wait for this lock or each other.
        let _lock = FileLock::acquire_async(&path.with_extension("wt-start.lock"), cancel).await?;
        if let Some(service) = read().await? {
            return service.validate();
        }
        let mut command = CommandSpec::new(program).args(["service", "start"]);
        command.preserve_children_on_success = true;
        runner
            .run(command, cancel)
            .await
            .map_err(|error| operation_error("service", error.to_string()))?
            .checked("opencode service start")
            .map_err(|error| operation_error("service", error.to_string()))?;
        read()
            .await?
            .ok_or_else(|| {
                operation_error("service", "native service did not publish its registration")
            })?
            .validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_rejects_exited_process_and_nonlocal_authority() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        let connection = OpenCodeConnection {
            url: "http://127.0.0.1:12345".into(),
            password: "fixture".into(),
            pid: Some(pid),
        };
        assert!(
            connection
                .validate()
                .err()
                .unwrap()
                .to_string()
                .contains("opencode service start")
        );
        let connection = OpenCodeConnection {
            url: "http://127.0.0.1:12345@other.example".into(),
            password: "fixture".into(),
            pid: None,
        };
        assert!(connection.validate().is_err());
    }
}
