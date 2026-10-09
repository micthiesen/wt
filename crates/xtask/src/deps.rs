use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde_json::Value;

const FOUNDATION: &[&str] = &["wt-core", "wt-config", "wt-platform", "wt-store"];

pub fn check(root: &Path) -> Result<()> {
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(root)
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let metadata: Value =
        serde_json::from_slice(&output.stdout).context("parsing cargo metadata")?;
    let packages = metadata["packages"]
        .as_array()
        .context("metadata packages missing")?;
    let workspace_members: BTreeSet<&str> = metadata["workspace_members"]
        .as_array()
        .context("metadata workspace members missing")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let by_id: BTreeMap<&str, (&str, PathBuf)> = packages
        .iter()
        .filter_map(|package| {
            let id = package["id"].as_str()?;
            let name = package["name"].as_str()?;
            let manifest = package["manifest_path"].as_str()?;
            Some((id, (name, PathBuf::from(manifest))))
        })
        .collect();
    let root_manifest = root.join("Cargo.toml");
    let root_source = std::fs::read_to_string(&root_manifest)
        .with_context(|| format!("reading {}", root_manifest.display()))?;
    let root_toml: toml::Value =
        toml::from_str(&root_source).context("parsing workspace manifest")?;
    let workspace_dependencies = root_toml
        .get("workspace")
        .and_then(|v| v.get("dependencies"))
        .and_then(toml::Value::as_table)
        .context("[workspace.dependencies] missing")?;

    let mut errors = Vec::new();
    for package in packages {
        let id = package["id"].as_str().unwrap_or_default();
        if !workspace_members.contains(id) {
            continue;
        }
        let name = package["name"].as_str().unwrap_or_default();
        let manifest_path = by_id
            .get(id)
            .map(|(_, path)| path.as_path())
            .unwrap_or(root);
        let manifest: toml::Value = match std::fs::read_to_string(manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))
            .and_then(|source| toml::from_str(&source).context("parsing crate manifest"))
        {
            Ok(manifest) => manifest,
            Err(error) => {
                errors.push(format!("{name}: {error:#}"));
                continue;
            }
        };
        check_workspace_inheritance(name, &manifest, workspace_dependencies, &mut errors);
        let internal_dependencies: BTreeSet<&str> = package["dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|dependency| {
                let dependency_name = dependency["name"].as_str()?;
                by_id
                    .values()
                    .any(|(workspace_name, _)| *workspace_name == dependency_name)
                    .then_some(dependency_name)
            })
            .collect();

        let allowed_foundation = if name == "wt-config" {
            &["wt-core"][..]
        } else {
            &[][..]
        };
        if FOUNDATION.contains(&name)
            && internal_dependencies
                .iter()
                .any(|dependency| !allowed_foundation.contains(dependency))
        {
            errors.push(format!(
                "{name} is foundation and cannot depend on workspace crates: {}",
                internal_dependencies
                    .iter()
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if name == "wt-runtime" {
            let allowed: BTreeSet<&str> = FOUNDATION.iter().copied().collect();
            let forbidden: Vec<_> = internal_dependencies
                .difference(&allowed)
                .copied()
                .collect();
            if !forbidden.is_empty() {
                errors.push(format!(
                    "wt-runtime may depend only on foundation crates; found: {}",
                    forbidden.join(", ")
                ));
            }
        }
        if name == "wt-tui" {
            let forbidden: Vec<_> = internal_dependencies
                .iter()
                .copied()
                .filter(|name| !matches!(*name, "wt-core" | "wt-runtime"))
                .collect();
            if !forbidden.is_empty() {
                errors.push(format!(
                    "wt-tui cannot depend on I/O adapters: {}",
                    forbidden.join(", ")
                ));
            }
        }
    }
    if !errors.is_empty() {
        bail!("dependency policy violations:\n- {}", errors.join("\n- "));
    }
    println!("deps-check: workspace inheritance and foundation/runtime direction ok");
    Ok(())
}

fn check_workspace_inheritance(
    package_name: &str,
    manifest: &toml::Value,
    workspace_dependencies: &toml::map::Map<String, toml::Value>,
    errors: &mut Vec<String>,
) {
    fn scan_table(
        package_name: &str,
        table: Option<&toml::Value>,
        workspace_dependencies: &toml::map::Map<String, toml::Value>,
        path: &str,
        errors: &mut Vec<String>,
    ) {
        let Some(table) = table.and_then(toml::Value::as_table) else {
            return;
        };
        for (name, declaration) in table {
            if !workspace_dependencies.contains_key(name) {
                errors.push(format!(
                    "{package_name}: {path}.{name} must be declared in workspace.dependencies"
                ));
                continue;
            }
            let inherited = declaration
                .as_table()
                .and_then(|value| value.get("workspace"))
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
            if !inherited {
                errors.push(format!(
                    "{package_name}: {path}.{name} must use workspace = true"
                ));
            }
        }
    }
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        scan_table(
            package_name,
            manifest.get(section),
            workspace_dependencies,
            section,
            errors,
        );
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for (target, value) in targets {
            for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                scan_table(
                    package_name,
                    value.get(section),
                    workspace_dependencies,
                    &format!("target.{target}.{section}"),
                    errors,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_dependencies_must_be_inherited_in_target_tables_too() -> Result<()> {
        let manifest: toml::Value =
            toml::from_str("[target.'cfg(unix)'.dependencies]\nserde = \"1\"\n")?;
        let workspace: toml::Value = toml::from_str("[dependencies]\nserde = \"1\"\n")?;
        let workspace_dependencies = workspace["dependencies"]
            .as_table()
            .context("missing test table")?;
        let mut errors = Vec::new();

        check_workspace_inheritance("sample", &manifest, workspace_dependencies, &mut errors);

        assert_eq!(
            errors,
            ["sample: target.cfg(unix).dependencies.serde must use workspace = true"]
        );
        Ok(())
    }
}
