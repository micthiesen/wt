use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::Args;
use tokio_util::sync::CancellationToken;
use wt_config::LoadOptions;
use wt_platform::process::{CommandSpec, ProcessRunner};

#[derive(Debug, Args)]
pub struct InitArgs {
    pub directory: Option<PathBuf>,
    #[arg(long, value_parser = ["claude", "codex", "opencode"])]
    pub primary: Option<String>,
    /// Branch namespace, required when the user config does not provide one.
    #[arg(long)]
    pub prefix: Option<String>,
}

pub async fn run(
    options: &LoadOptions,
    args: &InitArgs,
    cancel: &CancellationToken,
) -> Result<i32> {
    let (path, namespace) = initialize(options, args, cancel).await?;
    println!("created {}\nnamespace {namespace}", path.display());
    Ok(0)
}

async fn initialize(
    options: &LoadOptions,
    args: &InitArgs,
    cancel: &CancellationToken,
) -> Result<(PathBuf, String)> {
    let directory = options
        .cwd
        .join(args.directory.as_deref().unwrap_or(Path::new(".")));
    let runner = ProcessRunner::default();
    let root = git(
        &runner,
        &directory,
        ["rev-parse", "--show-toplevel"],
        cancel,
    )
    .await?
    .with_context(|| format!("{} is not inside a Git repository", directory.display()))?;
    let root = PathBuf::from(root);
    let config_path = root.join(".wt.toml");
    if tokio::fs::try_exists(&config_path).await? {
        bail!("{} already exists", config_path.display());
    }
    let prefix = match &args.prefix {
        Some(prefix) => prefix.trim().to_owned(),
        None => inherited_prefix(options).await?.context(
            "branch.prefix is not configured; run wt init --prefix <your-branch-namespace>",
        )?,
    };
    if prefix.is_empty()
        || git(
            &runner,
            &root,
            ["check-ref-format", "--branch", &format!("{prefix}/wt-init")],
            cancel,
        )
        .await?
        .is_none()
    {
        bail!("--prefix must be a valid, nonempty Git branch namespace");
    }
    let base = detect_base(&runner, &root, cancel).await?;
    let namespace = wt_config::repository_namespace(&config_path, &options.home, &options.cwd);
    // Git prints physical paths on macOS (for example /private/var), while
    // HOME may use its symlink alias. Compare like paths when rendering `~`.
    let display_home = tokio::fs::canonicalize(&options.home)
        .await
        .unwrap_or_else(|_| options.home.clone());
    let content = render_config(
        &root,
        &display_home,
        &namespace,
        &base,
        &prefix,
        args.primary.as_deref(),
    )?;
    if cancel.is_cancelled() {
        bail!("initialization cancelled");
    }
    let destination = config_path.clone();
    // Publish only the complete file. persist_noclobber also protects against a
    // concurrent initializer that won the race after the existence check.
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut temporary = tempfile::NamedTempFile::new_in(&root)?;
        temporary.write_all(content.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist_noclobber(&destination).with_context(|| {
            format!(
                "create {} without replacing existing configuration",
                destination.display()
            )
        })?;
        std::fs::File::open(&root)?.sync_all()?;
        Ok(())
    })
    .await??;
    Ok((config_path, namespace))
}

async fn inherited_prefix(options: &LoadOptions) -> Result<Option<String>> {
    let path = options.user_config_path();
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(value
        .get("branch")
        .and_then(|branch| branch.get("prefix"))
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned))
}

async fn git<const N: usize>(
    runner: &ProcessRunner,
    directory: &Path,
    args: [&str; N],
    cancel: &CancellationToken,
) -> Result<Option<String>> {
    let result = runner
        .run(CommandSpec::new("git").cwd(directory).args(args), cancel)
        .await?;
    if !result.status.success() {
        return Ok(None);
    }
    let output = String::from_utf8(result.stdout)
        .context("Git returned a non-UTF-8 repository path or branch")?;
    Ok((!output.trim().is_empty()).then(|| output.trim().to_owned()))
}

async fn detect_base(
    runner: &ProcessRunner,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<String> {
    if let Some(remote) = git(
        runner,
        root,
        [
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        cancel,
    )
    .await?
        && let Some(base) = remote
            .strip_prefix("origin/")
            .filter(|base| !base.is_empty())
    {
        return Ok(base.to_owned());
    }
    for candidate in ["main", "master"] {
        if git(
            runner,
            root,
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{candidate}"),
            ],
            cancel,
        )
        .await?
        .is_some()
        {
            return Ok(candidate.into());
        }
    }
    Ok(git(
        runner,
        root,
        ["symbolic-ref", "--quiet", "--short", "HEAD"],
        cancel,
    )
    .await?
    .unwrap_or_else(|| "main".into()))
}

fn display_path(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".into(),
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

fn render_config(
    root: &Path,
    home: &Path,
    namespace: &str,
    base: &str,
    prefix: &str,
    primary: Option<&str>,
) -> Result<String> {
    let name = root
        .file_name()
        .context("repository root must have a directory name")?
        .to_string_lossy();
    let worktrees = root.with_file_name(format!("{name}-worktrees"));
    let mut value = toml::Table::new();
    let mut paths = toml::Table::new();
    for (key, path) in [
        ("main_clone", root.to_path_buf()),
        ("worktree_root", worktrees),
        (
            "cache_db",
            home.join(".cache/wt").join(namespace).join("cache.sqlite"),
        ),
    ] {
        paths.insert(key.into(), toml::Value::String(display_path(&path, home)));
    }
    value.insert("paths".into(), paths.into());
    value.insert(
        "branch".into(),
        toml::Value::Table(
            [
                ("base".into(), base.into()),
                ("prefix".into(), prefix.into()),
            ]
            .into_iter()
            .collect(),
        ),
    );
    value.insert(
        "tmux".into(),
        toml::Value::Table(
            [("socket".into(), format!("wt-{namespace}").into())]
                .into_iter()
                .collect(),
        ),
    );
    if let Some(primary) = primary {
        value.insert(
            "harness".into(),
            toml::Value::Table([("primary".into(), primary.into())].into_iter().collect()),
        );
    }
    Ok(format!(
        "# Generated by `wt init`.\n{}",
        toml::to_string_pretty(&value)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn initializes_nested_repository_without_global_config_and_never_overwrites() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let root = home.join("Code/project");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let cancel = CancellationToken::new();
        let runner = ProcessRunner::default();
        git(&runner, &root, ["init", "--initial-branch=trunk"], &cancel)
            .await
            .unwrap();
        let options = LoadOptions::new(root.join("nested"), &home, BTreeMap::new());
        let args = InitArgs {
            directory: None,
            primary: Some("codex".into()),
            prefix: Some("developer".into()),
        };
        let (path, _) = initialize(&options, &args, &cancel).await.unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        assert!(original.contains("~/Code/project"));
        let config = wt_config::Config::load(&options).unwrap();
        assert_eq!(config.branch.base, "trunk");
        assert_eq!(config.branch.prefix, "developer");
        assert_eq!(config.harness.primary, wt_core::HarnessId::Codex);
        assert!(
            initialize(&options, &args, &cancel)
                .await
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[tokio::test]
    async fn missing_prefix_leaves_no_config_and_inherits_explicit_user_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        let cancel = CancellationToken::new();
        git(
            &ProcessRunner::default(),
            &root,
            ["init", "--initial-branch=main"],
            &cancel,
        )
        .await
        .unwrap();
        let mut options = LoadOptions::new(&root, temp.path(), BTreeMap::new());
        let args = InitArgs {
            directory: None,
            primary: None,
            prefix: None,
        };
        assert!(
            initialize(&options, &args, &cancel)
                .await
                .unwrap_err()
                .to_string()
                .contains("--prefix")
        );
        assert!(!root.join(".wt.toml").exists());
        let user_config = temp.path().join("user.toml");
        std::fs::write(&user_config, "[branch]\nprefix = 'personal'\n").unwrap();
        options
            .env
            .insert("WT_CONFIG".into(), user_config.to_str().unwrap().into());
        initialize(&options, &args, &cancel).await.unwrap();
        assert_eq!(
            wt_config::Config::load(&options).unwrap().branch.prefix,
            "personal"
        );
    }
}
