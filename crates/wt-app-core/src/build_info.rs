//! The running binary's release version, build identity and target.
//!
//! The `wt` binary (`crates/wt-app`) is the only crate compiled with these
//! values; it installs them here before doing anything else. Library code reads
//! them at runtime instead of through `env!`, so a release or a new commit
//! recompiles that small binary crate alone and every library comes from cache
//! (docs/ci.md). They are process constants, not configuration.

use std::sync::OnceLock;

/// What the `wt` binary was built as.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildInfo {
    /// The release version, `CARGO_PKG_VERSION` of `wt-app`.
    pub version: &'static str,
    /// The build identity, `WT_BUILD_ID`: a full commit SHA for releases.
    pub build_id: &'static str,
    /// The Rust target triple the binary was built for.
    pub target: &'static str,
}

struct Installed {
    info: BuildInfo,
    /// `<version> (<build id>, <target>)`, as `wt --version` prints it.
    native_version: &'static str,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// Record the binary's identity. Called once, first thing in `main`.
pub fn install(info: BuildInfo) {
    INSTALLED.get_or_init(|| installed(info));
}

fn installed(info: BuildInfo) -> Installed {
    let native_version = format!("{} ({}, {})", info.version, info.build_id, info.target).leak();
    Installed {
        info,
        native_version,
    }
}

fn get() -> &'static Installed {
    #[cfg(any(test, feature = "testing"))]
    {
        INSTALLED.get_or_init(|| installed(test_identity()))
    }
    #[cfg(not(any(test, feature = "testing")))]
    {
        INSTALLED
            .get()
            .expect("the wt binary installs its build identity before any work")
    }
}

/// The identity unit tests run under, matching what the build script gave a
/// development build: `dev-<version>` for the build and the real target.
#[cfg(any(test, feature = "testing"))]
fn test_identity() -> BuildInfo {
    BuildInfo {
        version: "0.0.0",
        build_id: "dev-0.0.0",
        target: if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
            "aarch64-apple-darwin"
        } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
            "x86_64-apple-darwin"
        } else if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
            "aarch64-unknown-linux-gnu"
        } else {
            "x86_64-unknown-linux-gnu"
        },
    }
}

pub fn version() -> &'static str {
    get().info.version
}

pub fn build_id() -> &'static str {
    get().info.build_id
}

pub fn target() -> &'static str {
    get().info.target
}

/// `<version> (<build id>, <target>)`.
pub fn native_version() -> &'static str {
    get().native_version
}
