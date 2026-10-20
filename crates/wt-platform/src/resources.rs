//! Enter a systemd user scope before executing a workload.
//!
//! The caller joins its own scope, then replaces itself with the job. This
//! preserves terminal ownership, signals, and process identity across tmux.

use std::{io, path::PathBuf, time::Duration};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    lock::{FileLock, LockError},
    process::{CommandSpec, ProcessError, ProcessRunner},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub cpu_quota_percent: Option<u32>,
    pub memory_high: Option<u64>,
    pub memory_max: Option<u64>,
    pub memory_swap_max: Option<u64>,
}

impl Limits {
    pub fn arguments(&self, prefix: &str) -> Vec<String> {
        self.values()
            .into_iter()
            .filter_map(|(name, value)| value.map(|value| (name, value)))
            .flat_map(|(name, value)| [format!("--{prefix}-{name}"), value.to_string()])
            .collect()
    }

    fn values(&self) -> [(&'static str, Option<u64>); 4] {
        [
            ("cpu", self.cpu_quota_percent.map(u64::from)),
            ("memory-high", self.memory_high),
            ("memory-max", self.memory_max),
            ("memory-swap-max", self.memory_swap_max),
        ]
    }

    fn properties(&self) -> Vec<String> {
        let mut properties = Vec::new();
        if let Some(percent) = self.cpu_quota_percent {
            properties.extend([
                "CPUQuotaPerSecUSec".into(),
                "t".into(),
                (u64::from(percent) * 10_000).to_string(),
            ]);
        }
        for (name, value) in [
            ("MemoryHigh", self.memory_high),
            ("MemoryMax", self.memory_max),
            ("MemorySwapMax", self.memory_swap_max),
        ] {
            if let Some(value) = value {
                properties.extend([name.into(), "t".into(), value.to_string()]);
            }
        }
        properties
    }

    fn assignments(&self) -> Vec<String> {
        let cpu = self
            .cpu_quota_percent
            .map(|value| format!("{value}%"))
            .unwrap_or_default();
        let mut assignments = vec![format!("CPUQuota={cpu}")];
        for (name, value) in [
            ("MemoryHigh", self.memory_high),
            ("MemoryMax", self.memory_max),
            ("MemorySwapMax", self.memory_swap_max),
        ] {
            let value = value
                .map(|value| value.to_string())
                .unwrap_or("infinity".into());
            assignments.push(format!("{name}={value}"));
        }
        assignments
    }

    async fn verify(&self, directory: &std::path::Path) -> Result<(), ResourceError> {
        if let Some(percent) = self.cpu_quota_percent {
            let value = tokio::fs::read_to_string(directory.join("cpu.max")).await?;
            if !cpu_quota_matches(&value, percent) {
                return Err(ResourceError::Unavailable(format!(
                    "CPU quota was not enforced at {}",
                    directory.display()
                )));
            }
        }
        for (name, expected) in [
            ("memory.high", self.memory_high),
            ("memory.max", self.memory_max),
            ("memory.swap.max", self.memory_swap_max),
        ] {
            if let Some(expected) = expected {
                let value = tokio::fs::read_to_string(directory.join(name)).await?;
                let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
                if !memory_limit_matches(&value, expected, u64::try_from(page_size).unwrap_or(0)) {
                    return Err(ResourceError::Unavailable(format!(
                        "{name} was not enforced at {}",
                        directory.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ResourceError {
    #[error(
        "workload limits require Linux, cgroup v2, and an available systemd user manager with delegated CPU and memory controllers: {0}"
    )]
    Unavailable(String),
    #[error("workload resource I/O: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error(transparent)]
    Lock(#[from] LockError),
}

fn cpu_quota_matches(value: &str, percent: u32) -> bool {
    let fields = value.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 2 {
        return false;
    }
    match (fields[0].parse::<u64>(), fields[1].parse::<u64>()) {
        (Ok(quota), Ok(period)) if period > 0 => {
            u128::from(quota) * 100 == u128::from(percent) * u128::from(period)
        }
        _ => false,
    }
}

fn memory_limit_matches(value: &str, expected: u64, page_size: u64) -> bool {
    let Ok(actual) = value.trim().parse::<u64>() else {
        return false;
    };
    if actual == expected {
        return true;
    }
    if page_size == 0 {
        return false;
    }
    // cgroup v2 can round byte limits up to the host's page size.
    let rounded = expected
        .checked_add(page_size - 1)
        .map(|value| value / page_size * page_size);
    rounded == Some(actual)
}

/// Fail before executing the job if the kernel cannot enforce its policy.
pub async fn enter(
    group: &str,
    shared: &Limits,
    job: &Limits,
    runner: &ProcessRunner,
    cancel: &CancellationToken,
) -> Result<(), ResourceError> {
    if !cfg!(target_os = "linux") {
        return Err(ResourceError::Unavailable("this host is not Linux".into()));
    }
    if group.is_empty()
        || group.len() > 64
        || !group
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(ResourceError::Unavailable("invalid resource group".into()));
    }
    let controllers = tokio::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers").await?;
    if !controllers.split_whitespace().any(|name| name == "cpu")
        || !controllers.split_whitespace().any(|name| name == "memory")
    {
        return Err(ResourceError::Unavailable(
            "CPU or memory controller is absent".into(),
        ));
    }
    // Select this host's user bus rather than a socket inherited from another host.
    let uid = unsafe { libc::geteuid() };
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    let bus = runtime.join("bus");
    if !bus.exists() {
        return Err(ResourceError::Unavailable(format!(
            "{} is absent",
            bus.display()
        )));
    }
    let environment = vec![
        (
            "XDG_RUNTIME_DIR".into(),
            Some(runtime.as_os_str().to_owned()),
        ),
        (
            "DBUS_SESSION_BUS_ADDRESS".into(),
            Some(format!("unix:path={}", bus.display()).into()),
        ),
    ];
    let run = async |program: &str, arguments: Vec<String>| {
        let mut command = CommandSpec::new(program).args(arguments);
        command.timeout = Duration::from_secs(10);
        command.output_limit = 64 * 1024;
        command.env = environment.clone();
        let output = runner.run(command, cancel).await?;
        if !output.status.success() {
            return Err(ResourceError::Unavailable(format!(
                "{program}: {}",
                output.stderr_text().trim()
            )));
        }
        Ok(output.stdout_text())
    };

    let slice = format!("wtworkloads-{group}.slice");
    let _lock = FileLock::acquire(
        &runtime.join("wt-resources"),
        group,
        "configure workload limits",
        cancel,
    )
    .await?;
    let unit_directory = runtime.join("systemd/user");
    tokio::fs::create_dir_all(&unit_directory).await?;
    let unit = unit_directory.join(&slice);
    let content = "# Generated by wt. Limits come from resources.linux.shared.\n[Unit]\nDescription=wt shared workload budget\n[Slice]\n";
    let previous = match tokio::fs::read_to_string(&unit).await {
        Ok(previous) if previous.starts_with("# Generated by wt.") => Some(previous),
        Ok(_) => {
            return Err(ResourceError::Unavailable(format!(
                "{} already exists and is not managed by wt",
                unit.display()
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if previous.as_deref() != Some(content) {
        let temporary = unit.with_extension(format!("tmp-{}", std::process::id()));
        tokio::fs::write(&temporary, content).await?;
        tokio::fs::rename(&temporary, &unit).await?;
        run("systemctl", vec!["--user".into(), "daemon-reload".into()]).await?;
    }
    run(
        "systemctl",
        vec!["--user".into(), "start".into(), slice.clone()],
    )
    .await?;
    let mut assignments = vec![
        "--user".into(),
        "set-property".into(),
        "--runtime".into(),
        slice.clone(),
    ];
    assignments.extend(shared.assignments());
    run("systemctl", assignments).await?;

    let scope = format!(
        "wtjob-{}-{}.scope",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let properties = job.properties();
    let mut arguments = vec![
        "--user".into(),
        "call".into(),
        "org.freedesktop.systemd1".into(),
        "/org/freedesktop/systemd1".into(),
        "org.freedesktop.systemd1.Manager".into(),
        "StartTransientUnit".into(),
        "ssa(sv)a(sa(sv))".into(),
        scope.clone(),
        "fail".into(),
        (3 + properties.len() / 3).to_string(),
        "PIDs".into(),
        "au".into(),
        "1".into(),
        std::process::id().to_string(),
        "Slice".into(),
        "s".into(),
        slice.clone(),
        "CollectMode".into(),
        "s".into(),
        "inactive-or-failed".into(),
    ];
    arguments.extend(properties);
    arguments.push("0".into());
    // A lost reply never starts a second scope or executes an uncertain job.
    run("busctl", arguments).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let memberships = tokio::fs::read_to_string("/proc/self/cgroup").await?;
        let path = memberships
            .lines()
            .find_map(|line| line.strip_prefix("0::"));
        if let Some(path) = path.filter(|path| path.ends_with(&format!("/{scope}"))) {
            let directory = PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/'));
            job.verify(&directory).await?;
            let parent = directory
                .parent()
                .ok_or_else(|| ResourceError::Unavailable("scope has no slice".into()))?;
            if parent.file_name().and_then(|name| name.to_str()) != Some(slice.as_str()) {
                return Err(ResourceError::Unavailable(
                    "scope joined the wrong slice".into(),
                ));
            }
            shared.verify(parent).await?;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ResourceError::Unavailable(
                "systemd did not place the workload in its scope".into(),
            ));
        }
        tokio::select! {
            _ = cancel.cancelled() => return Err(ResourceError::Unavailable("scope setup cancelled".into())),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_verification_rejects_unlimited_or_wrong_kernel_values() {
        assert!(cpu_quota_matches("200000 100000\n", 200));
        assert!(cpu_quota_matches("100000 50000\n", 200));
        for value in [
            "max 100000",
            "100000 100000",
            "1 0",
            "garbage",
            "200000 100000 extra",
        ] {
            assert!(!cpu_quota_matches(value, 200), "{value}");
        }
    }

    #[test]
    fn memory_readback_accepts_only_exact_or_page_rounded_limits() {
        assert!(memory_limit_matches("4097", 4097, 4096));
        assert!(memory_limit_matches("8192", 4097, 4096));
        assert!(memory_limit_matches("16384", 4097, 16384));
        assert!(memory_limit_matches("0", 0, 4096));
        for value in ["max", "16384", "4096", "garbage"] {
            assert!(!memory_limit_matches(value, 4097, 4096), "{value}");
        }
        assert!(!memory_limit_matches("0", u64::MAX, 4096));
    }

    #[tokio::test]
    async fn verification_refuses_missing_or_ineffective_memory_limits() {
        let root = tempfile::tempdir().unwrap();
        let limits = Limits {
            memory_max: Some(4096),
            memory_swap_max: Some(0),
            ..Default::default()
        };
        tokio::fs::write(root.path().join("memory.max"), "max\n")
            .await
            .unwrap();
        assert!(limits.verify(root.path()).await.is_err());
        tokio::fs::write(root.path().join("memory.max"), "4096\n")
            .await
            .unwrap();
        assert!(limits.verify(root.path()).await.is_err());
        tokio::fs::write(root.path().join("memory.swap.max"), "0\n")
            .await
            .unwrap();
        limits.verify(root.path()).await.unwrap();
    }
}
