//! Package-manager selection for managed projects, independent of wt's own runtime.

use std::{ffi::OsString, path::Path, time::Duration};

use crate::process::CommandSpec;

const LOCKFILES: [(&str, &str); 6] = [
    ("bun.lock", "bun"),
    ("bun.lockb", "bun"),
    ("pnpm-lock.yaml", "pnpm"),
    ("yarn.lock", "yarn"),
    ("package-lock.json", "npm"),
    ("npm-shrinkwrap.json", "npm"),
];

#[derive(Clone, Debug)]
pub struct InstallPolicy {
    pub command: Option<String>,
    pub shell: OsString,
}

pub struct InstallCommand {
    pub spec: CommandSpec,
    pub gate_lockfiles: Vec<&'static str>,
    pub label: String,
}

impl InstallPolicy {
    /// Resolve after updating the checkout, since a pull can introduce a lockfile.
    /// Overrides run verbatim and must themselves preserve committed lockfiles.
    pub async fn resolve(
        &self,
        directory: &Path,
        frozen: bool,
    ) -> Result<Option<InstallCommand>, std::io::Error> {
        let (mut spec, gate_lockfiles, label) = if let Some(command) = &self.command {
            let mut spec = CommandSpec::new(self.shell.clone());
            spec.args = vec!["-lc".into(), command.into()];
            (
                spec,
                LOCKFILES.iter().map(|(file, _)| *file).collect(),
                command.clone(),
            )
        } else {
            let mut detected = None;
            for (file, manager) in LOCKFILES {
                if tokio::fs::try_exists(directory.join(file)).await? {
                    detected = Some((file, manager));
                    break;
                }
            }
            let Some((file, manager)) = detected else {
                return Ok(None);
            };
            let mut spec = CommandSpec::new(manager);
            spec.args = if frozen && manager == "npm" {
                vec!["ci".into()]
            } else if frozen {
                vec!["install".into(), "--frozen-lockfile".into()]
            } else {
                vec!["install".into()]
            };
            let label = format!(
                "{manager} {}",
                spec.args
                    .iter()
                    .map(|arg| arg.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            (spec, vec![file], label)
        };
        spec.cwd = Some(directory.to_path_buf());
        spec.timeout = Duration::from_secs(20 * 60);
        spec.output_limit = 2 * 1024 * 1024;
        Ok(Some(InstallCommand {
            spec,
            gate_lockfiles,
            label,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detection_precedence_frozen_commands_and_override_gates() {
        let dir = tempfile::tempdir().unwrap();
        let mut policy = InstallPolicy {
            command: None,
            shell: "sh".into(),
        };
        assert!(policy.resolve(dir.path(), true).await.unwrap().is_none());
        for (file, expected) in [
            ("package-lock.json", "npm ci"),
            ("pnpm-lock.yaml", "pnpm install --frozen-lockfile"),
            ("bun.lock", "bun install --frozen-lockfile"),
        ] {
            tokio::fs::write(dir.path().join(file), "").await.unwrap();
            let plan = policy.resolve(dir.path(), true).await.unwrap().unwrap();
            assert_eq!(plan.label, expected);
            assert_eq!(plan.gate_lockfiles, [file]);
        }
        assert_eq!(
            policy
                .resolve(dir.path(), false)
                .await
                .unwrap()
                .unwrap()
                .label,
            "bun install"
        );
        policy.command = Some("custom && install".into());
        let plan = policy.resolve(dir.path(), true).await.unwrap().unwrap();
        assert_eq!(plan.gate_lockfiles.len(), 6);
        assert_eq!(
            plan.spec.args,
            vec![OsString::from("-lc"), OsString::from("custom && install")]
        );
    }
}
