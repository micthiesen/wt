use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Immutable release identity. `build_id` handles both version tags and
/// rolling preview commit SHAs; it is never interpreted as a path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(try_from = "VersionIdRaw")]
pub struct VersionId {
    release_version: String,
    build_id: String,
    target: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionIdRaw {
    release_version: String,
    build_id: String,
    target: String,
}

impl TryFrom<VersionIdRaw> for VersionId {
    type Error = String;

    fn try_from(raw: VersionIdRaw) -> Result<Self, Self::Error> {
        VersionId::new(raw.release_version, raw.build_id, raw.target)
            .map_err(|error| error.to_string())
    }
}

impl VersionId {
    pub fn new(
        release_version: impl Into<String>,
        build_id: impl Into<String>,
        target: impl Into<String>,
    ) -> Result<Self, PathError> {
        let release_version = release_version.into();
        let build_id = build_id.into();
        let target = target.into();
        if !validate_component(&release_version) {
            return Err(PathError::InvalidVersion(release_version));
        }
        if !validate_component(&build_id) {
            return Err(PathError::InvalidVersion(build_id));
        }
        if !validate_component(&target) {
            return Err(PathError::InvalidTarget(target));
        }
        Ok(Self {
            release_version,
            build_id,
            target,
        })
    }

    pub fn key(&self) -> String {
        format!("{}-{}-{}", self.release_version, self.build_id, self.target)
    }

    pub fn release_version(&self) -> &str {
        &self.release_version
    }

    pub fn build_id(&self) -> &str {
        &self.build_id
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn is_valid(&self) -> bool {
        validate_component(&self.release_version)
            && validate_component(&self.build_id)
            && validate_component(&self.target)
    }
}

/// A fixed install tree rooted at a user-selected per-user directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallPaths {
    root: PathBuf,
}

impl InstallPaths {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, PathError> {
        let root = root.into();
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Err(PathError::RootMustBeAbsolute(root));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn launcher(&self) -> PathBuf {
        self.root.join("bin").join(executable_name("wt"))
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.root.join("update.lock")
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    pub fn version_dir(&self, version: &VersionId) -> PathBuf {
        self.root.join("versions").join(version.key())
    }

    pub fn app_binary(&self, version: &VersionId) -> PathBuf {
        self.version_dir(version)
            .join("bin")
            .join(executable_name("wt"))
    }

    pub fn launcher_binary(&self, version: &VersionId) -> PathBuf {
        self.version_dir(version)
            .join("bin")
            .join(executable_name("wt-launcher"))
    }
}

/// Validate a string intended to occupy exactly one filesystem component.
/// Slashes, traversal components, hidden names, and platform metacharacters
/// are rejected before they reach path construction.
pub fn validate_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.starts_with('.')
        && !value.ends_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
        && Path::new(value)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn executable_name(name: &str) -> &'static str {
    #[cfg(windows)]
    {
        match name {
            "wt" => "wt.exe",
            _ => "wt-launcher.exe",
        }
    }
    #[cfg(not(windows))]
    {
        match name {
            "wt" => "wt",
            _ => "wt-launcher",
        }
    }
}

#[derive(Debug, Error)]
pub enum PathError {
    #[error("install root must be an absolute path: {0}")]
    RootMustBeAbsolute(PathBuf),
    #[error("unsafe release build id: {0:?}")]
    InvalidVersion(String),
    #[error("unsafe release target: {0:?}")]
    InvalidTarget(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_ids_cannot_escape_the_version_tree() {
        for bad in ["", ".", "..", ".hidden", "../x", "a/b", "a\\b", "x:"] {
            assert!(!validate_component(bad), "accepted {bad:?}");
        }
        assert!(validate_component("v1.2.3"));
        assert!(validate_component("preview-a1b2c3d"));
        assert!(validate_component("aarch64-apple-darwin"));
    }

    #[test]
    fn install_paths_are_derived_from_validated_identity() {
        let root = InstallPaths::new("/tmp/wt-install").unwrap();
        let version = VersionId::new("v1.2.3", "deadbeef", "aarch64-apple-darwin").unwrap();
        assert_eq!(
            root.app_binary(&version),
            PathBuf::from("/tmp/wt-install/versions/v1.2.3-deadbeef-aarch64-apple-darwin/bin/wt")
        );
        assert!(InstallPaths::new("relative/path").is_err());
        assert!(VersionId::new("../escape", "deadbeef", "x86_64-unknown-linux-gnu").is_err());
    }
}
