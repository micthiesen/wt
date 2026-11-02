//! `cargo xtask preflight`: everything CI runs that a local machine can, then
//! a stamp naming the exact commit that passed. The pre-push hook refuses to
//! send an unstamped commit to `main` or a release tag, so a failure that a
//! local run would have caught never costs a release run.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

/// Scripts the CI lint job runs beside Clippy.
const PACKAGING_TESTS: &[&[&str]] = &[
    &["scripts/test-package-native.py"],
    &["scripts/test-native-check.py"],
    &["scripts/test-restore-mtimes.py"],
    &[
        "-m",
        "unittest",
        "discover",
        "-s",
        ".github/scripts",
        "-p",
        "test_*.py",
    ],
];

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Stamps live in the common Git directory, so every linked worktree of the
/// clone sees them and none is ever committed.
pub fn stamp_dir(root: &Path) -> Result<PathBuf> {
    let common = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Ok(PathBuf::from(common).join("wt-preflight"))
}

fn step(
    root: &Path,
    label: &str,
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<()> {
    println!("preflight: {label}");
    let status = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .current_dir(root)
        .status()
        .with_context(|| format!("starting {program}"))?;
    if !status.success() {
        bail!("{label} failed ({status})");
    }
    Ok(())
}

pub fn run(root: &Path, gate: impl FnOnce() -> Result<()>) -> Result<()> {
    let head = git(root, &["rev-parse", "HEAD"])?;
    // Only a clean tree is the commit being stamped; a dirty one still runs,
    // so the checks are useful mid-change, but earns no stamp.
    let clean = git(root, &["status", "--porcelain", "--untracked-files=no"])?.is_empty();
    gate()?;
    let python = std::env::var("PYTHON").unwrap_or_else(|_| "python3".into());
    for args in PACKAGING_TESTS {
        step(
            root,
            &format!("python3 {}", args.join(" ")),
            &python,
            args,
            &[],
        )?;
    }
    // The native suite drives the built binary and checks its build identity.
    let identity = [("WT_BUILD_ID", head.as_str())];
    step(
        root,
        "build native prerequisites",
        &std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()),
        &["build", "--workspace", "--bins", "--examples", "--locked"],
        &identity,
    )?;
    let jobs = std::env::var("WT_NATIVE_JOBS").unwrap_or_else(|_| "4".into());
    step(
        root,
        "native end-to-end suite",
        &python,
        &[
            "scripts/native-check.py",
            "--binary",
            "target/debug/wt",
            "--jobs",
            &jobs,
        ],
        &identity,
    )?;
    let still = git(root, &["rev-parse", "HEAD"])?;
    let still_clean = git(root, &["status", "--porcelain", "--untracked-files=no"])?.is_empty();
    if !(clean && still_clean && still == head) {
        println!(
            "preflight: passed, but the tree was dirty or HEAD moved during the run, so {} is not stamped; commit and rerun before pushing to main",
            &head[..12]
        );
        return Ok(());
    }
    let dir = stamp_dir(root)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join(&head), "")
        .with_context(|| format!("writing preflight stamp for {head}"))?;
    println!("preflight: passed; stamped {} for push", &head[..12]);
    Ok(())
}
