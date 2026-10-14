//! Private, versioned Pi runtime for managed workers. Provider credentials and
//! settings stay in the host's native Pi home; only the executable is managed.
use std::{ffi::OsString, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessRunner};

pub const VERSION: &str = wt_harness::PI_VERSION;
const PACKAGE: &str = "@earendil-works/pi-coding-agent";

/// Called under the managed root's apply lock. Validate a staged runtime before
/// activating it; existing processes and native Pi installations are untouched.
pub async fn provision(
    root: &Path,
    version: &str,
    runner: &ProcessRunner,
    environment: &[(OsString, Option<OsString>)],
    cancellation: &CancellationToken,
) -> Result<()> {
    if version != VERSION {
        bail!("unsupported managed Pi version {version:?}; this wt build requires {VERSION}");
    }
    let runtimes = root.join("tools/pi");
    for path in [root.join("tools"), runtimes.clone(), root.join("bin")] {
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            bail!(
                "managed Pi directory must not be a symlink: {}",
                path.display()
            );
        }
        std::fs::create_dir_all(&path)?;
    }
    let destination = runtimes.join(version);
    if std::fs::symlink_metadata(&destination).is_ok_and(|meta| meta.file_type().is_symlink()) {
        bail!(
            "managed Pi runtime must not be a symlink: {}",
            destination.display()
        );
    }
    if destination.exists() {
        validate(&destination, version, runner, environment, cancellation).await?;
    } else {
        let staging = tempfile::Builder::new()
            .prefix(".install-")
            .tempdir_in(&runtimes)?;
        let mut install = CommandSpec::new("npm").args([
            OsString::from("install"),
            OsString::from("--prefix"),
            staging.path().as_os_str().to_owned(),
            OsString::from("--no-audit"),
            OsString::from("--no-fund"),
            OsString::from("--omit=dev"),
            OsString::from("--ignore-scripts"),
            OsString::from("--save-exact"),
            OsString::from(format!("{PACKAGE}@{version}")),
        ]);
        install.env = environment.to_vec();
        install.timeout = Duration::from_secs(10 * 60);
        let output = runner
            .run(install, cancellation)
            .await
            .context("install managed Pi runtime")?;
        if !output.status.success() {
            bail!(
                "managed Pi installation exited {}; the previous runtime remains active",
                output.status
            );
        }
        validate(staging.path(), version, runner, environment, cancellation).await?;
        std::fs::rename(staging.path(), &destination).context("publish managed Pi runtime")?;
    }
    let links = tempfile::Builder::new()
        .prefix(".pi-link-")
        .tempdir_in(root.join("bin"))?;
    let staged_link = links.path().join("pi");
    std::os::unix::fs::symlink(destination.join("node_modules/.bin/pi"), &staged_link)?;
    std::fs::rename(staged_link, root.join("bin/pi")).context("activate managed Pi runtime")?;
    Ok(())
}

async fn validate(
    prefix: &Path,
    version: &str,
    runner: &ProcessRunner,
    environment: &[(OsString, Option<OsString>)],
    cancellation: &CancellationToken,
) -> Result<()> {
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(prefix.join("node_modules/@earendil-works/pi-coding-agent/package.json"))
            .context("read managed Pi package manifest")?,
    )?;
    if manifest["name"].as_str() != Some(PACKAGE) || manifest["version"].as_str() != Some(version) {
        bail!("managed Pi package does not match {PACKAGE}@{version}; preserving current runtime");
    }
    let mut probe = CommandSpec::new(prefix.join("node_modules/.bin/pi")).args(["--version"]);
    probe.env = environment.to_vec();
    probe.timeout = Duration::from_secs(20);
    let output = runner
        .run(probe, cancellation)
        .await
        .context("probe managed Pi runtime")?;
    if !output.status.success() || output.stdout_text().trim() != version {
        bail!("managed Pi runtime failed its version probe; preserving current runtime");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn executable(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn fixture(version: &str) -> (tempfile::TempDir, Vec<(OsString, Option<OsString>)>) {
        let temp = tempfile::tempdir().unwrap();
        let package = temp
            .path()
            .join("fixture/node_modules/@earendil-works/pi-coding-agent");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("package.json"),
            serde_json::json!({
                "name": PACKAGE, "version": version,
            })
            .to_string(),
        )
        .unwrap();
        let bin = temp.path().join("fixture/node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(
            &bin.join("pi"),
            &format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"),
        );
        std::fs::create_dir_all(temp.path().join("bin")).unwrap();
        executable(
            &temp.path().join("bin/npm"),
            r#"#!/bin/sh
set -eu
[ "$1" = install ] && [ "$2" = --prefix ]
prefix=$3
printf '%s\n' "$*" >> "$WT_PI_FIXTURE/install-calls"
cp -R "$WT_PI_FIXTURE/fixture/node_modules" "$prefix/"
"#,
        );
        let environment = vec![
            (
                "PATH".into(),
                Some(format!("{}:/usr/bin:/bin", temp.path().join("bin").display()).into()),
            ),
            (
                "WT_PI_FIXTURE".into(),
                Some(temp.path().as_os_str().to_owned()),
            ),
        ];
        (temp, environment)
    }

    #[tokio::test]
    async fn exact_runtime_installs_once_and_preserves_native_state() -> Result<()> {
        let (temp, environment) = fixture(VERSION);
        let root = temp.path().join("managed");
        let native = temp.path().join(".pi/agent");
        std::fs::create_dir_all(&native)?;
        std::fs::write(native.join("auth.json"), "native-private-auth")?;
        std::fs::write(native.join("settings.json"), "native-rich-settings")?;
        let runner = ProcessRunner::default();
        let cancel = CancellationToken::new();
        provision(&root, VERSION, &runner, &environment, &cancel).await?;
        provision(&root, VERSION, &runner, &environment, &cancel).await?;
        let calls = std::fs::read_to_string(temp.path().join("install-calls"))?;
        assert_eq!(calls.lines().count(), 1);
        assert!(calls.contains("@earendil-works/pi-coding-agent@1.2.0"));
        assert_eq!(
            std::fs::read_link(root.join("bin/pi"))?,
            root.join("tools/pi/1.2.0/node_modules/.bin/pi")
        );
        assert_eq!(
            std::fs::read_to_string(native.join("auth.json"))?,
            "native-private-auth"
        );
        assert_eq!(
            std::fs::read_to_string(native.join("settings.json"))?,
            "native-rich-settings"
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_candidate_never_replaces_active_runtime() -> Result<()> {
        let (temp, environment) = fixture("0.84.2");
        let root = temp.path().join("managed");
        std::fs::create_dir_all(root.join("bin"))?;
        std::os::unix::fs::symlink("/existing/pi", root.join("bin/pi"))?;
        let result = provision(
            &root,
            VERSION,
            &ProcessRunner::default(),
            &environment,
            &CancellationToken::new(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_link(root.join("bin/pi"))?,
            Path::new("/existing/pi")
        );
        assert!(!root.join("tools/pi/1.2.0").exists());
        assert_eq!(std::fs::read_dir(root.join("tools/pi"))?.count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn unsupported_package_version_is_rejected_before_writes() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            provision(
                temp.path(),
                "../../other",
                &ProcessRunner::default(),
                &[],
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn package_metadata_alone_cannot_activate_a_broken_executable() -> Result<()> {
        let (temp, environment) = fixture(VERSION);
        executable(
            &temp.path().join("fixture/node_modules/.bin/pi"),
            "#!/bin/sh\nexit 1\n",
        );
        let root = temp.path().join("managed");
        let result = provision(
            &root,
            VERSION,
            &ProcessRunner::default(),
            &environment,
            &CancellationToken::new(),
        )
        .await;
        assert!(result.is_err());
        assert!(!root.join("bin/pi").exists());
        assert!(!root.join("tools/pi/1.2.0").exists());
        Ok(())
    }
}
