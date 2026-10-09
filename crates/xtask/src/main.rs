#![allow(clippy::print_stdout, clippy::print_stderr)]

mod deps;
mod hygiene;

use std::{path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn cargo(args: &[String]) -> Result<()> {
    println!("gate: cargo {}", args.join(" "));
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(args)
        .current_dir(repo_root())
        .status()
        .context("starting cargo")?;
    if !status.success() {
        bail!("cargo {} exited with {status}", args.join(" "));
    }
    Ok(())
}

fn nextest_installed() -> bool {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["nextest", "--version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn package_has_docs(package: &str) -> Result<bool> {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(repo_root())
        .output()
        .context("running cargo metadata to find doctest targets")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("parsing cargo metadata for doctest targets")?;
    let packages = metadata["packages"]
        .as_array()
        .context("metadata packages missing")?;
    let package = packages
        .iter()
        .find(|candidate| candidate["name"] == package)
        .with_context(|| format!("package {package:?} not found in cargo metadata"))?;
    Ok(package["targets"].as_array().is_some_and(|targets| {
        targets.iter().any(|target| {
            target["kind"].as_array().is_some_and(|kinds| {
                kinds
                    .iter()
                    .any(|kind| kind == "lib" || kind == "proc-macro")
            })
        })
    }))
}

fn gate(package: Option<&str>) -> Result<()> {
    hygiene::report(&repo_root())?;
    cargo(&strings(["fmt", "--all", "--check"]))?;
    let mut clippy = strings(["clippy"]);
    if let Some(package) = package {
        clippy.extend(["-p".into(), package.into()]);
    } else {
        clippy.extend(strings(["--workspace"]));
    }
    clippy.extend(strings([
        "--all-targets",
        "--locked",
        "--",
        "-D",
        "warnings",
    ]));
    cargo(&clippy)?;
    deps::check(&repo_root())?;

    let mut tests = if nextest_installed() {
        strings(["nextest", "run"])
    } else {
        println!("gate: cargo-nextest not installed; falling back to cargo test");
        strings(["test"])
    };
    if let Some(package) = package {
        tests.extend(["-p".into(), package.into()]);
    } else {
        tests.extend(strings(["--workspace"]));
    }
    tests.extend(strings(["--locked"]));
    cargo(&tests)?;

    let run_doctests = match package {
        Some(package) => package_has_docs(package)?,
        None => true,
    };
    if run_doctests {
        let mut doctests = strings(["test", "--doc"]);
        if let Some(package) = package {
            doctests.extend(["-p".into(), package.into()]);
        } else {
            doctests.extend(strings(["--workspace"]));
        }
        doctests.extend(strings(["--locked"]));
        cargo(&doctests)?;
    } else if let Some(package) = package {
        println!("gate: package {package} has no library/doc-test target; doctests skipped");
    }
    if let Some(package) = package {
        println!("gate: package {package} checks passed; workspace coverage remains pending");
    } else {
        println!("gate: workspace checks passed");
    }
    Ok(())
}

fn strings<const N: usize>(values: [&str; N]) -> Vec<String> {
    values.into_iter().map(str::to_owned).collect()
}

fn usage() {
    eprintln!("usage: cargo xtask <gate [--package NAME] | deps-check | hygiene>");
}

fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("gate") => {
            let mut package = None;
            let mut iter = args[1..].iter();
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--package" => {
                        if package.is_some() {
                            bail!("--package may be supplied once");
                        }
                        package = Some(iter.next().context("--package requires a name")?.as_str());
                    }
                    other => bail!("unknown gate argument {other:?}"),
                }
            }
            gate(package)
        }
        Some("deps-check") => deps::check(&repo_root()),
        Some("hygiene") if args.len() <= 1 => hygiene::report(&repo_root()),
        Some("hygiene") if args.len() == 2 && args[1] == "--clean-incremental" => {
            hygiene::clean_incremental(&repo_root())
        }
        Some("hygiene") => bail!("usage: cargo xtask hygiene [--clean-incremental]"),
        Some("help" | "--help" | "-h") | None => {
            usage();
            Ok(())
        }
        Some(other) => bail!("unknown command {other:?}"),
    }
}

fn main() {
    if let Err(error) = run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
