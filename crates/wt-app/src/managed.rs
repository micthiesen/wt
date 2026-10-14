//! Controller half of a managed remote. The controller is the source of every
//! file a managed worker owns: its wt config, environment, git settings,
//! credentials, and Codex setup. Each connection sends them again, so the
//! worker converges on the controller without manual setup on the worker.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use wt_config::{LoadOptions, RemoteConfig};
use wt_platform::process::CommandSpec;
use wt_remote::{
    RemoteClient,
    managed::{
        CheckState, MANAGED_PROTOCOL, ManagedCheck, ManagedFile, ManagedReport, ManagedRequest,
        ManagedTree, ROOT_PLACEHOLDER, SignIn,
    },
};

use crate::context::AppContext;

const APPLY_TIMEOUT: Duration = Duration::from_secs(35 * 60);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TREE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TREE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REPO_FILE_BYTES: u64 = 1024 * 1024;
const TREE_DEPTH: usize = 12;

/// A managed worker's latest checks and when the controller received them.
#[derive(Clone, Debug, PartialEq)]
pub struct ManagedStatus {
    pub report: ManagedReport,
    pub at_ms: u64,
}

impl ManagedStatus {
    pub fn now(report: ManagedReport) -> Self {
        Self {
            report,
            at_ms: crate::activity_source::epoch_ms(),
        }
    }
}

/// The sign-in a request needs before a managed worker can serve it: GitHub
/// for creating worktrees, Codex for starting or entering Codex agents.
/// `primary` is the harness a plain harness session starts; the worker
/// config is generated from the controller's, so both agree on it.
pub fn required_sign_in(
    report: &ManagedReport,
    action: &wt_tui::UiAction,
    primary: wt_core::HarnessId,
) -> Option<SignIn> {
    let provider = required_provider(action, primary)?;
    report.sign_ins().contains(&provider).then_some(provider)
}

pub fn required_provider(action: &wt_tui::UiAction, primary: wt_core::HarnessId) -> Option<SignIn> {
    use wt_core::HarnessId;
    use wt_tui::{SessionTarget, UiAction};
    let needs = Some;
    match action {
        UiAction::Create { .. } => needs(SignIn::Github),
        UiAction::EnterHarness {
            harness: HarnessId::Codex,
            ..
        } => needs(SignIn::Codex),
        UiAction::Session {
            target: SessionTarget::Harness | SessionTarget::Manager | SessionTarget::Main,
            ..
        } if primary == HarnessId::Codex => needs(SignIn::Codex),
        UiAction::SelectSession { selection } if selection.harness == HarnessId::Codex => {
            needs(SignIn::Codex)
        }
        _ => None,
    }
}

pub fn sign_in_modal(host: &str, provider: SignIn, then: wt_tui::UiAction) -> wt_tui::UiModal {
    let mut lines = vec![format!(
        "{} is not signed in on {host}. Sign in now, then continue?",
        provider.label()
    )];
    lines.push(match provider {
        SignIn::Codex => "wt opens Codex's sign-in over SSH. Open the link it prints in this machine's browser; the callback is forwarded to the host.".into(),
        SignIn::Github => "wt runs `gh auth login` on the host. Signing in to gh on this machine instead lets wt supply the token automatically.".into(),
    });
    wt_tui::UiModal::Confirm {
        action: wt_tui::ConfirmAction::ManagedSignIn {
            provider: provider.as_str().into(),
            then: Some(Box::new(then)),
        },
        title: format!("Sign in on {host}"),
        lines,
        cancel_key: None,
    }
}

/// Attention lines for a managed worker: what is degraded and how to fix it.
pub fn notices(report: &ManagedReport) -> Vec<String> {
    report
        .checks
        .iter()
        .filter(|check| check.state != CheckState::Ok)
        .map(|check| match check.sign_in {
            Some(_) if check.state == CheckState::SignInNeeded => format!(
                "{}. wt asks you to sign in when you next create a worktree or start an agent there.",
                check.detail
            ),
            _ => check.detail.clone(),
        })
        .collect()
}

/// Controller-side facts about the request, reported beside the worker's.
struct Prepared {
    request: ManagedRequest,
    checks: Vec<ManagedCheck>,
}

/// Bring a managed worker in line with this controller and return its checks.
/// The runtime must already be staged on `client`.
pub async fn provision(context: &AppContext, client: &RemoteClient) -> Result<ManagedReport> {
    let remote = client.remote();
    let prepared = prepare(context, remote).await?;
    let input = serde_json::to_vec(&prepared.request)?;
    let output = client
        .run_managed(
            &["apply".into()],
            input,
            APPLY_TIMEOUT,
            &context.cancellation,
        )
        .await?;
    if !output.status.success() {
        bail!(
            "managed worker setup on {} failed: {}",
            remote.label,
            output.stderr_text().trim()
        );
    }
    let stdout = output.stdout_text();
    let line = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .context("managed worker setup returned no report")?;
    let mut report: ManagedReport =
        serde_json::from_str(line).context("parse managed worker report")?;
    if report.protocol != MANAGED_PROTOCOL {
        bail!(
            "managed worker report protocol {} is not supported",
            report.protocol
        );
    }
    report.checks.extend(prepared.checks);
    Ok(report)
}

async fn prepare(context: &AppContext, remote: &RemoteConfig) -> Result<Prepared> {
    let root = remote
        .managed_root
        .clone()
        .context("managed remote has no derived root")?;
    let main = context.config.paths.main_clone.clone();
    let mut checks = Vec::new();
    // Required dependencies are collected completely before sending any
    // worker update, so a broken declaration cannot publish partial setup.
    let declared = context.config.managed.files.clone();
    let declared_files = tokio::task::spawn_blocking(move || collect_declared_files(&declared))
        .await
        .context("join managed dependency collection")??;

    let origin = run(context, &main, "git", &["remote", "get-url", "origin"])
        .await
        .context("read this repository's origin URL")?;
    let repository = https_clone_url(origin.trim()).with_context(|| {
        format!(
            "origin {} is not a GitHub URL; managed remotes clone over HTTPS from GitHub",
            origin.trim()
        )
    })?;

    let mut files = vec![
        file("config.toml", worker_config(context)?.into_bytes(), 0o600),
        file("env.sh", environment_script(&root).into_bytes(), 0o600),
        file(
            "bin/git-credential-wt",
            CREDENTIAL_HELPER.as_bytes().to_vec(),
            0o700,
        ),
    ];

    let global = run(
        context,
        &main,
        "git",
        &["config", "--global", "--null", "--list"],
    )
    .await
    .unwrap_or_default();
    // Commits made on the worker are unsigned; the signing key stays here.
    // That is documented rather than repeated in the attention feed.
    let gitconfig = git_config(&global);
    files.push(file("gitconfig", gitconfig.into_bytes(), 0o600));

    // The account wt already uses for this repository on the controller.
    let mut remove = Vec::new();
    match run(
        context,
        &main,
        "gh",
        &["auth", "token", "--hostname", "github.com"],
    )
    .await
    {
        Ok(token) if !token.trim().is_empty() => {
            let token = token.trim().to_owned();
            let login = run(context, &main, "gh", &["api", "user", "--jq", ".login"])
                .await
                .ok()
                .map(|login| login.trim().to_owned())
                .filter(|login| !login.is_empty());
            files.push(file(
                "secrets/github-token",
                format!("{token}\n").into_bytes(),
                0o600,
            ));
            files.push(file(
                "gh/hosts.yml",
                gh_hosts(&token, login.as_deref()).into_bytes(),
                0o600,
            ));
        }
        // Signed out here, so the worker must not keep the old token.
        _ => remove.extend(["secrets/github-token".into(), "gh/hosts.yml".into()]),
    }

    let mut secrets =
        String::from("# Generated by wt from [managed.env]; rewritten on every connection.\n");
    if !context.config.managed.services.is_empty() {
        let identity =
            crate::controller_tunnels::identity(&context.home, &context.cancellation).await?;
        secrets.push_str(&format!("export WT_CONTROLLER_BRIDGE=1\nexport WT_CONTROLLER_SOCKET=\"$WT_MANAGED_ROOT/controller-{identity}.sock\"\n"));
        files.push(file(
            "controller-endpoint",
            format!("{ROOT_PLACEHOLDER}/controller-{identity}.sock").into_bytes(),
            0o600,
        ));
    } else {
        remove.push("controller-endpoint".into());
    }
    for (name, command) in &context.config.managed.env {
        match run(context, &main, "/bin/sh", &["-c", command]).await {
            Ok(value) => {
                let value = value.strip_suffix('\n').unwrap_or(&value);
                secrets.push_str(&format!("export {name}={}\n", shell_quote(value)));
            }
            Err(error) => checks.push(ManagedCheck::new(
                &format!("env:{name}"),
                CheckState::Warning,
                format!(
                    "managed.env.{name} is unset on {}: {}",
                    remote.label,
                    first_line(&format!("{error:#}"))
                ),
                None,
            )),
        }
    }
    files.push(file("secrets/env.sh", secrets.into_bytes(), 0o600));

    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| context.home.join(".codex"));
    let codex_config = std::fs::read_to_string(codex_home.join("config.toml")).ok();
    for name in ["AGENTS.md", "hooks.json"] {
        if let Ok(content) = std::fs::read(codex_home.join(name)) {
            files.push(file(&format!("codex/{name}"), content, 0o600));
        }
    }
    let home = context.home.clone();
    let (mut trees, tree_warnings) = tokio::task::spawn_blocking(move || {
        let mut warnings = Vec::new();
        // Later sources win: shared `~/.agents/skills` over Codex's own copy.
        let skills = collect_tree(
            &[codex_home.join("skills"), home.join(".agents/skills")],
            &mut warnings,
        );
        let rules = collect_tree(&[codex_home.join("rules")], &mut warnings);
        // A tree that could not be read completely is left as it is on the
        // worker rather than mirrored as empty.
        let trees = [("codex/skills", skills), ("codex/rules", rules)]
            .into_iter()
            .filter_map(|(path, files)| {
                files.map(|files| ManagedTree {
                    path: path.into(),
                    files,
                })
            })
            .collect::<Vec<_>>();
        (trees, warnings)
    })
    .await
    .context("join Codex skill collection")?;
    trees.push(ManagedTree {
        path: "files".into(),
        files: declared_files,
    });
    checks.extend(
        tree_warnings
            .into_iter()
            .map(|detail| ManagedCheck::new("codex-mirror", CheckState::Warning, detail, None)),
    );

    let repo_files = repository_files(context, &main).await;

    Ok(Prepared {
        request: ManagedRequest {
            protocol: MANAGED_PROTOCOL,
            root,
            repository,
            files,
            remove,
            trees,
            repo_files,
            codex_config,
            codex_auth_generation: crate::codex_reauth::controller_generation(
                &crate::codex_reauth::controller_home(&context.home),
            )?,
        },
        checks,
    })
}

fn file(path: &str, content: Vec<u8>, mode: u32) -> ManagedFile {
    ManagedFile {
        path: path.to_owned(),
        content,
        mode,
    }
}

fn collect_declared_files(declared: &BTreeMap<String, PathBuf>) -> Result<Vec<ManagedFile>> {
    use std::io::Read;
    let mut total = 0;
    let mut files = Vec::new();
    for (destination, source) in declared {
        let description = || {
            format!(
                "managed.files.{destination}: read controller source {}",
                source.display()
            )
        };
        let metadata = std::fs::metadata(source).with_context(description)?;
        if !metadata.is_file() {
            bail!(
                "managed.files.{destination}: {} is not a regular file",
                source.display()
            );
        }
        if metadata.len() > MAX_TREE_FILE_BYTES {
            bail!("managed.files.{destination}: source exceeds the 64 MiB file limit");
        }
        let mut input = std::fs::File::open(source).with_context(description)?;
        let mut content = Vec::new();
        (&mut input)
            .take(MAX_TREE_FILE_BYTES + 1)
            .read_to_end(&mut content)
            .with_context(description)?;
        if content.len() as u64 > MAX_TREE_FILE_BYTES {
            bail!("managed.files.{destination}: source exceeds the 64 MiB file limit");
        }
        total += content.len() as u64;
        if total > MAX_TREE_BYTES {
            bail!("managed.files: declared dependencies exceed the 128 MiB total limit");
        }
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 != 0 {
                0o700
            } else {
                0o600
            }
        };
        #[cfg(not(unix))]
        let mode = 0o600;
        files.push(file(destination, content, mode));
    }
    Ok(files)
}

async fn run(context: &AppContext, cwd: &Path, program: &str, args: &[&str]) -> Result<String> {
    let mut spec = CommandSpec::new(program)
        .args(args.iter().copied())
        .cwd(cwd);
    spec.timeout = COMMAND_TIMEOUT;
    spec.output_limit = 4 * 1024 * 1024;
    let output = context
        .processes
        .run(spec, &context.cancellation)
        .await?
        .checked(program)?;
    Ok(output.stdout_text())
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// The worker's configuration is this controller's merged configuration with
/// worker-local paths and controller-only integrations removed.
fn worker_config(context: &AppContext) -> Result<String> {
    let raw = context
        .config
        .raw_layers(&LoadOptions::default())
        .context("read this controller's configuration layers")?;
    worker_config_from(raw)
}

pub(crate) fn worker_config_from(raw: toml::Value) -> Result<String> {
    let mut table = match raw {
        toml::Value::Table(table) => table,
        _ => bail!("configuration root must be a table"),
    };
    // Controller-only: remotes and roles, local paths, the update channel,
    // the webhook daemon and the tmux socket name (derived per worker).
    for key in [
        "instance", "remote", "remotes", "managed", "paths", "update",
    ] {
        table.remove(key);
    }
    if let Some(toml::Value::Table(tmux)) = table.get_mut("tmux") {
        tmux.remove("socket");
    }
    if let Some(toml::Value::Table(github)) = table.get_mut("github") {
        github.remove("events");
    }
    let mut instance = toml::Table::new();
    instance.insert("role".into(), toml::Value::String("worker".into()));
    table.insert("instance".into(), toml::Value::Table(instance));
    let mut paths = toml::Table::new();
    paths.insert(
        "main_clone".into(),
        toml::Value::String(format!("{ROOT_PLACEHOLDER}/repo")),
    );
    paths.insert(
        "worktree_root".into(),
        toml::Value::String(format!("{ROOT_PLACEHOLDER}/worktrees")),
    );
    table.insert("paths".into(), toml::Value::Table(paths));
    // Managed workers promise Git as their only repository dependency. Rift
    // belongs to the controller's storage setup and may not exist remotely.
    let mut backend = toml::Table::new();
    backend.insert("kind".into(), toml::Value::String("git-worktree".into()));
    table.insert("backend".into(), toml::Value::Table(backend));
    let mut skills = toml::Table::new();
    skills.insert("startup_check".into(), toml::Value::Boolean(false));
    table.insert("skills".into(), toml::Value::Table(skills));
    Ok(format!(
        "# Generated by wt for a managed worker; rewritten on every connection.\n{}",
        toml::to_string(&table)?
    ))
}

/// Sourced before every worker command. Sessions inherit the variables named
/// in `WT_SESSION_ENV` and source `secrets/env.sh` themselves, so agents see
/// the same credentials as wt itself without secrets appearing in argv.
pub(crate) fn environment_script(root: &str) -> String {
    let session = [
        "WT_MANAGED_ROOT",
        "WT_MANAGED_FILES",
        "CODEX_HOME",
        "GH_CONFIG_DIR",
        "GIT_CONFIG_GLOBAL",
        "GIT_TERMINAL_PROMPT",
    ];
    format!(
        r#"# Generated by wt for a managed worker; rewritten on every connection.
WT_MANAGED_ROOT="$HOME/{root}"
export WT_MANAGED_ROOT
export WT_MANAGED_FILES="$WT_MANAGED_ROOT/files"
# The login shell's PATH (Homebrew, version managers) comes first; the live
# PATH stays as a fallback for entries that changed since it was captured.
WT_LOGIN_PATH=
if [ -r "$WT_MANAGED_ROOT/login-path" ]; then
  IFS= read -r WT_LOGIN_PATH < "$WT_MANAGED_ROOT/login-path" || true
fi
# Retain the first occurrence of each component, including an empty component
# (the current directory). Re-sourcing this file must not grow PATH. Shell
# builtins avoid relying on tools which this PATH is still being prepared for.
WT_PATH_REMAINING="${{WT_LOGIN_PATH:+$WT_LOGIN_PATH:}}$PATH"
WT_PATH_UNIQUE="$WT_MANAGED_ROOT/bin"
while :; do
  WT_PATH_ENTRY=${{WT_PATH_REMAINING%%:*}}
  case ":$WT_PATH_UNIQUE:" in
    *":$WT_PATH_ENTRY:"*) ;;
    *) WT_PATH_UNIQUE="$WT_PATH_UNIQUE:$WT_PATH_ENTRY" ;;
  esac
  [ "$WT_PATH_REMAINING" = "$WT_PATH_ENTRY" ] && break
  WT_PATH_REMAINING=${{WT_PATH_REMAINING#*:}}
done
PATH=$WT_PATH_UNIQUE
unset WT_LOGIN_PATH WT_PATH_REMAINING WT_PATH_UNIQUE WT_PATH_ENTRY
export PATH
export CODEX_HOME="$WT_MANAGED_ROOT/codex"
export GH_CONFIG_DIR="$WT_MANAGED_ROOT/gh"
export GIT_CONFIG_GLOBAL="$WT_MANAGED_ROOT/gitconfig"
export GIT_TERMINAL_PROMPT=0
if [ -r "$WT_MANAGED_ROOT/secrets/env.sh" ]; then
  . "$WT_MANAGED_ROOT/secrets/env.sh"
fi
export WT_SESSION_ENV="{session}"
"#,
        session = session.join(":"),
    )
}

/// Answers git's HTTPS credential requests for github.com from the managed
/// token, falling back to a gh sign-in made on the worker itself.
const CREDENTIAL_HELPER: &str = r#"#!/bin/sh
# Generated by wt for a managed worker; rewritten on every connection.
[ "$1" = get ] || exit 0
protocol=
host=
while IFS= read -r line && [ -n "$line" ]; do
  case $line in
    protocol=*) protocol=${line#protocol=} ;;
    host=*) host=${line#host=} ;;
  esac
done
[ "$protocol" = https ] && [ "$host" = github.com ] || exit 0
token=
if [ -r '@@WT_MANAGED_ROOT@@/secrets/github-token' ]; then
  IFS= read -r token < '@@WT_MANAGED_ROOT@@/secrets/github-token' || true
fi
if [ -z "$token" ] && command -v gh >/dev/null 2>&1; then
  token=$(GH_CONFIG_DIR='@@WT_MANAGED_ROOT@@/gh' gh auth token --hostname github.com 2>/dev/null)
fi
[ -n "$token" ] || exit 0
printf 'username=x-access-token\npassword=%s\n' "$token"
"#;

/// Copy the controller's global git settings except credentials, signing,
/// includes and URL rewrites, which name controller-only programs and keys.
pub(crate) fn git_config(listing: &str) -> String {
    let mut entries: Vec<(String, String)> = Vec::new();
    for record in listing.split('\0').filter(|record| !record.is_empty()) {
        let (key, value) = record.split_once('\n').unwrap_or((record, "true"));
        let lower = key.to_ascii_lowercase();
        // Credentials and signing, plus settings that name controller-only
        // programs or paths, or can carry a token.
        let skipped = [
            "credential.",
            "gpg.",
            "include.",
            "includeif.",
            "url.",
            "http.",
            "https.",
            "sendemail.",
            "github.",
            "hub.",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
            || matches!(
                lower.as_str(),
                "user.signingkey"
                    | "commit.gpgsign"
                    | "tag.gpgsign"
                    | "core.sshcommand"
                    | "core.askpass"
                    | "core.hookspath"
                    | "core.fsmonitor"
                    | "core.editor"
                    | "core.excludesfile"
                    | "core.attributesfile"
                    | "commit.template"
                    | "init.templatedir"
                    | "sequence.editor"
            );
        if !skipped {
            entries.push((key.to_owned(), value.to_owned()));
        }
    }
    entries.extend([
        ("credential.helper".into(), String::new()),
        (
            "credential.helper".into(),
            // Quoted for git's shell: the worker's home may contain spaces.
            format!("!'{ROOT_PLACEHOLDER}/bin/git-credential-wt'"),
        ),
        (
            "url.https://github.com/.insteadOf".into(),
            "git@github.com:".into(),
        ),
        (
            "url.https://github.com/.insteadOf".into(),
            "ssh://git@github.com/".into(),
        ),
    ]);
    let mut out =
        String::from("# Generated by wt for a managed worker; rewritten on every connection.\n");
    let mut current = String::new();
    for (key, value) in entries {
        let Some((section, name)) = key.rsplit_once('.') else {
            continue;
        };
        let header = match section.split_once('.') {
            Some((section, subsection)) => format!(
                "[{section} \"{}\"]",
                subsection.replace('\\', "\\\\").replace('"', "\\\"")
            ),
            None => format!("[{section}]"),
        };
        if header != current {
            out.push_str(&header);
            out.push('\n');
            current = header;
        }
        let escaped = value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\t', "\\t");
        out.push_str(&format!("\t{name} = \"{escaped}\"\n"));
    }
    out
}

fn gh_hosts(token: &str, login: Option<&str>) -> String {
    let mut out = format!(
        "github.com:\n    git_protocol: https\n    oauth_token: {}\n",
        yaml_quote(token)
    );
    if let Some(login) = login {
        out.push_str(&format!(
            "    user: {login}\n    users:\n        {login}:\n            oauth_token: {}\n",
            yaml_quote(token),
            login = yaml_quote(login),
        ));
    }
    out
}

fn yaml_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Managed workers authenticate to GitHub with an HTTPS token, so SSH remote
/// URLs are rewritten to their HTTPS form.
pub(crate) fn https_clone_url(origin: &str) -> Option<String> {
    let path = origin
        .strip_prefix("git@github.com:")
        .or_else(|| origin.strip_prefix("ssh://git@github.com/"))
        .or_else(|| origin.strip_prefix("https://github.com/"))
        .or_else(|| origin.strip_prefix("http://github.com/"))?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("https://github.com/{owner}/{name}.git"))
}

/// Gather files from several directories into one mirror, later directories
/// replacing earlier ones file by file. Symlinks are followed with a depth
/// bound; an oversized file leaves the previous mirror intact. `None` means the tree
/// could not be read completely and must not replace the worker's copy.
fn collect_tree(sources: &[PathBuf], warnings: &mut Vec<String>) -> Option<Vec<ManagedFile>> {
    let mut files: BTreeMap<String, ManagedFile> = BTreeMap::new();
    let mut total = 0u64;
    for source in sources {
        let mut pending = vec![(source.clone(), String::new(), 0usize)];
        while let Some((directory, prefix, depth)) = pending.pop() {
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                // A source that does not exist contributes nothing.
                Err(error) if depth == 0 && error.kind() == std::io::ErrorKind::NotFound => {
                    continue;
                }
                Err(error) => {
                    warnings.push(format!(
                        "Did not update {} on managed workers: {error}",
                        directory.display()
                    ));
                    return None;
                }
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    warnings.push(format!(
                        "Did not update {} on managed workers: a directory entry could not be read",
                        directory.display()
                    ));
                    return None;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                // `.system` holds the skills each Codex install writes for
                // itself, so the worker's own copy is left alone.
                if name == ".git"
                    || name == ".DS_Store"
                    || name.starts_with(".wt-")
                    || (depth == 0 && name == ".system")
                {
                    continue;
                }
                let path = entry.path();
                let relative = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                let metadata = match std::fs::metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        warnings.push(format!("Did not mirror {} to managed workers: {error}; keeping the previous complete tree", path.display()));
                        return None;
                    }
                };
                if metadata.is_dir() {
                    if depth < TREE_DEPTH {
                        pending.push((path, relative, depth + 1));
                    } else {
                        warnings.push(format!("Did not mirror {} to managed workers: directory nesting exceeds {TREE_DEPTH}; keeping the previous complete tree", path.display()));
                        return None;
                    }
                } else if metadata.is_file() {
                    if metadata.len() > MAX_TREE_FILE_BYTES {
                        warnings.push(format!(
                            "Did not mirror {} to managed workers: a file exceeds the 64 MiB limit; keeping the previous complete tree",
                            path.display()
                        ));
                        return None;
                    }
                    let Ok(content) = std::fs::read(&path) else {
                        warnings.push(format!(
                            "Did not update managed workers' copy of {}: it could not be read",
                            path.display()
                        ));
                        return None;
                    };
                    #[cfg(unix)]
                    let mode = {
                        use std::os::unix::fs::PermissionsExt;
                        if metadata.permissions().mode() & 0o111 != 0 {
                            0o700
                        } else {
                            0o600
                        }
                    };
                    #[cfg(not(unix))]
                    let mode = 0o600;
                    if let Some(previous) = files.get(&relative) {
                        total = total.saturating_sub(previous.content.len() as u64);
                    }
                    total += content.len() as u64;
                    if total > MAX_TREE_BYTES {
                        warnings.push("Did not mirror skills or rules to managed workers: the tree exceeds the 128 MiB limit; keeping the previous complete tree".into());
                        return None;
                    }
                    files.insert(relative.clone(), file(&relative, content, mode));
                }
            }
        }
    }
    Some(files.into_values().collect())
}

/// The untracked files new worktrees copy from the main clone, such as
/// `.env`. Tracked files arrive with the clone and must not be overwritten.
async fn repository_files(context: &AppContext, main: &Path) -> Vec<ManagedFile> {
    let lifecycle = &context.config.lifecycle;
    let mut candidates: Vec<String> = lifecycle.env_files_to_copy.clone();
    if !lifecycle.copy_globs.is_empty() {
        let globs = lifecycle.copy_globs.clone();
        let root = main.to_path_buf();
        if let Ok(found) = tokio::task::spawn_blocking(move || glob_matches(&root, &globs)).await {
            candidates.extend(found);
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates.retain(|path| {
        Path::new(path)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    });
    if candidates.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["ls-files", "-z", "--"];
    args.extend(candidates.iter().map(String::as_str));
    let Ok(tracked) = run(context, main, "git", &args).await else {
        return Vec::new();
    };
    let tracked = tracked
        .split('\0')
        .collect::<std::collections::BTreeSet<_>>();
    candidates
        .into_iter()
        .filter(|path| !tracked.contains(path.as_str()))
        .filter_map(|path| {
            let full = main.join(&path);
            let metadata = std::fs::metadata(&full).ok()?;
            (metadata.is_file() && metadata.len() <= MAX_REPO_FILE_BYTES).then_some(())?;
            Some(file(&path, std::fs::read(&full).ok()?, 0o600))
        })
        .collect()
}

fn glob_matches(root: &Path, patterns: &[String]) -> Vec<String> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        if let Ok(glob) = globset::GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
        {
            builder.add(glob);
        }
    }
    let Ok(globs) = builder.build() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == ".git" || name == "node_modules" {
                continue;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && let Ok(relative) = path.strip_prefix(root)
                && globs.is_match(relative)
            {
                found.push(relative.to_string_lossy().into_owned());
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_sessions_need_codex_only_when_codex_starts() {
        use wt_core::HarnessId;
        use wt_remote::managed::{CheckState, ManagedCheck};
        let report = ManagedReport {
            protocol: wt_remote::managed::MANAGED_PROTOCOL,
            codex_auth_generation: None,
            checks: vec![ManagedCheck::new(
                "codex-login",
                CheckState::SignInNeeded,
                "Codex",
                Some(SignIn::Codex),
            )],
        };
        let session = wt_tui::UiAction::Session {
            key: Some("app/feature".into()),
            target: wt_tui::SessionTarget::Harness,
        };
        assert_eq!(
            required_sign_in(&report, &session, HarnessId::Codex),
            Some(SignIn::Codex)
        );
        assert_eq!(required_sign_in(&report, &session, HarnessId::Claude), None);
        let shell = wt_tui::UiAction::Session {
            key: Some("app/feature".into()),
            target: wt_tui::SessionTarget::Shell,
        };
        assert_eq!(required_sign_in(&report, &shell, HarnessId::Codex), None);
    }

    #[test]
    fn ssh_and_https_github_origins_clone_over_https() {
        for origin in [
            "git@github.com:acme/app.git",
            "ssh://git@github.com/acme/app",
            "https://github.com/acme/app",
            "https://github.com/acme/app.git/",
        ] {
            assert_eq!(
                https_clone_url(origin).as_deref(),
                Some("https://github.com/acme/app.git"),
                "{origin}"
            );
        }
        assert_eq!(https_clone_url("git@gitlab.com:acme/app.git"), None);
        assert_eq!(https_clone_url("https://github.com/acme"), None);
    }

    #[test]
    fn worker_config_keeps_project_settings_and_drops_controller_only_ones() {
        let raw: toml::Value = toml::from_str(
            r#"
[instance]
role = "controller"
[paths]
main_clone = "~/Code/app"
worktree_root = "~/Code/app-wt"
[[remotes]]
host = "beast"
managed = true
[managed.env]
TOKEN = "op read x"
[tmux]
socket = "wt-app"
[github]
reviewers = false
[github.events]
port = 9000
[harness]
primary = "codex"
[backend]
kind = "rift"
[lifecycle]
env_files_to_copy = [".env"]
"#,
        )
        .unwrap();
        let generated: toml::Table = toml::from_str(&worker_config_from(raw).unwrap()).unwrap();
        assert_eq!(generated["instance"]["role"].as_str(), Some("worker"));
        assert_eq!(generated["backend"]["kind"].as_str(), Some("git-worktree"));
        assert_eq!(
            generated["paths"]["main_clone"].as_str(),
            Some("@@WT_MANAGED_ROOT@@/repo")
        );
        assert_eq!(generated["paths"].as_table().unwrap().len(), 2);
        assert!(generated.get("remotes").is_none());
        assert!(generated.get("managed").is_none());
        assert!(
            generated["tmux"]
                .as_table()
                .unwrap()
                .get("socket")
                .is_none()
        );
        assert!(
            generated["github"]
                .as_table()
                .unwrap()
                .get("events")
                .is_none()
        );
        assert_eq!(generated["github"]["reviewers"].as_bool(), Some(false));
        assert_eq!(generated["harness"]["primary"].as_str(), Some("codex"));
        assert_eq!(generated["skills"]["startup_check"].as_bool(), Some(false));
    }

    #[test]
    fn git_config_replaces_credentials_signing_and_rewrites() {
        let listing = "user.name\nAda Lovelace\0user.email\nada@example.com\0credential.helper\nosxkeychain\0commit.gpgsign\ntrue\0gpg.format\nssh\0url.git@github.com:.insteadof\nhttps://github.com/\0push.autosetupremote\ntrue\0alias.lg\nlog --format=\"%h\"\0";
        let config = git_config(listing);
        assert!(
            config.contains("[user]\n\tname = \"Ada Lovelace\"\n\temail = \"ada@example.com\"\n")
        );
        assert!(config.contains("[push]\n\tautosetupremote = \"true\"\n"));
        assert!(config.contains("\tlg = \"log --format=\\\"%h\\\"\"\n"));
        assert!(!config.contains("osxkeychain"));
        assert!(!config.contains("gpgsign"));
        assert!(!config.contains("https://github.com/\"]\n\tinsteadof = \"git@"));
        assert!(config.contains("[credential]\n\thelper = \"\"\n\thelper = \"!'@@WT_MANAGED_ROOT@@/bin/git-credential-wt'\"\n"));
        assert!(config.contains("[url \"https://github.com/\"]\n\tinsteadOf = \"git@github.com:\"\n\tinsteadOf = \"ssh://git@github.com/\"\n"));
    }

    #[test]
    fn declared_dependencies_are_required_bounded_and_keep_executable_mode() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("helper");
        std::fs::write(&helper, b"#!/bin/sh\necho helper\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let declared = BTreeMap::from([("scripts/helper".into(), helper.clone())]);
        let files = collect_declared_files(&declared).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "scripts/helper");
        assert_eq!(files[0].content, std::fs::read(&helper).unwrap());
        #[cfg(unix)]
        assert_eq!(files[0].mode, 0o700);
        std::fs::remove_file(&helper).unwrap();
        let error = collect_declared_files(&declared).unwrap_err().to_string();
        assert!(error.contains("managed.files.scripts/helper"));
        std::fs::create_dir(&helper).unwrap();
        assert!(
            collect_declared_files(&declared)
                .unwrap_err()
                .to_string()
                .contains("not a regular file")
        );
        std::fs::remove_dir(&helper).unwrap();
        std::fs::File::create(&helper)
            .unwrap()
            .set_len(MAX_TREE_FILE_BYTES + 1)
            .unwrap();
        assert!(
            collect_declared_files(&declared)
                .unwrap_err()
                .to_string()
                .contains("64 MiB")
        );
    }

    #[test]
    fn environment_names_session_variables_but_never_secrets() {
        let script = environment_script(".local/share/wt/managed/app");
        assert!(script.contains("WT_MANAGED_ROOT=\"$HOME/.local/share/wt/managed/app\""));
        assert!(script.contains(
            "export WT_SESSION_ENV=\"WT_MANAGED_ROOT:WT_MANAGED_FILES:CODEX_HOME:GH_CONFIG_DIR:GIT_CONFIG_GLOBAL:GIT_TERMINAL_PROMPT\""
        ));
        assert!(script.contains(". \"$WT_MANAGED_ROOT/secrets/env.sh\""));
    }

    #[test]
    fn managed_environment_path_is_idempotent_and_preserves_long_unique_entries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("managed space");
        std::fs::create_dir_all(&root).unwrap();
        let mut entries = vec![
            "/tools with spaces".to_owned(),
            "/literal*?[glob]".to_owned(),
            "/usr/bin".to_owned(),
        ];
        entries.extend(
            (0..150).map(|index| format!("/long/unique/toolchain/{index:04}/{}", "x".repeat(50))),
        );
        let login = entries.join(":");
        assert!(login.len() > 9000);
        std::fs::write(root.join("login-path"), &login).unwrap();
        let script = root.join("env.sh");
        std::fs::write(&script, environment_script("managed space")).unwrap();
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", ". \"$1\"; first=$PATH; n=0; while [ \"$n\" -lt 25 ]; do . \"$1\"; n=$((n + 1)); done; [ \"$PATH\" = \"$first\" ] || exit 9; printf '%s' \"$PATH\"", "path-test"])
            .arg(script)
            .env("HOME", temp.path())
            .env("PATH", "/usr/bin::/tools with spaces:/live-only:/usr/bin:")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{}:{login}::/live-only", root.join("bin").display())
        );
    }

    #[test]
    fn credential_helper_answers_only_github_https() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_string_lossy().into_owned();
        std::fs::create_dir_all(temp.path().join("secrets")).unwrap();
        std::fs::write(temp.path().join("secrets/github-token"), "gho_secret\n").unwrap();
        let script = temp.path().join("helper");
        std::fs::write(&script, CREDENTIAL_HELPER.replace(ROOT_PLACEHOLDER, &root)).unwrap();
        let ask = |input: &str| {
            use std::io::Write;
            let mut child = std::process::Command::new("/bin/sh")
                .arg(&script)
                .arg("get")
                .env("PATH", "/usr/bin:/bin")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
        };
        assert_eq!(
            ask("protocol=https\nhost=github.com\n\n"),
            "username=x-access-token\npassword=gho_secret\n"
        );
        assert_eq!(ask("protocol=https\nhost=gitlab.com\n\n"), "");
        assert_eq!(ask("protocol=http\nhost=github.com\n\n"), "");
    }

    #[test]
    fn mirrored_trees_prefer_later_sources_and_skip_git_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("codex-skills");
        let second = temp.path().join("agents-skills");
        std::fs::create_dir_all(first.join("start/.git")).unwrap();
        std::fs::create_dir_all(second.join("start")).unwrap();
        std::fs::write(first.join("start/SKILL.md"), "old").unwrap();
        std::fs::write(first.join("start/.git/HEAD"), "x").unwrap();
        std::fs::write(first.join("only.md"), "kept").unwrap();
        std::fs::create_dir_all(first.join(".system/imagegen")).unwrap();
        std::fs::write(first.join(".system/imagegen/SKILL.md"), "bundled").unwrap();
        std::fs::write(second.join("start/SKILL.md"), "new").unwrap();
        let mut warnings = Vec::new();
        let files = collect_tree(&[first, second], &mut warnings).unwrap();
        let map = files
            .iter()
            .map(|file| {
                (
                    file.path.as_str(),
                    String::from_utf8_lossy(&file.content).into_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(map.len(), 2);
        assert_eq!(map["start/SKILL.md"], "new");
        assert_eq!(map["only.md"], "kept");
        assert!(warnings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_tree_is_not_sent_as_empty() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let rules = temp.path().join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(rules.join("default.rules"), "x").unwrap();
        std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut warnings = Vec::new();
        let result = collect_tree(std::slice::from_ref(&rules), &mut warnings);
        std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_none());
        assert_eq!(warnings.len(), 1);
        let missing = collect_tree(&[temp.path().join("absent")], &mut warnings);
        assert_eq!(missing, Some(Vec::new()));
    }

    #[test]
    fn skill_dependencies_larger_than_eight_mib_are_complete() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tsc");
        let content = vec![b'x'; 24 * 1024 * 1024];
        std::fs::write(&path, &content).unwrap();
        let mut warnings = Vec::new();
        let files = collect_tree(&[temp.path().to_owned()], &mut warnings).unwrap();
        assert_eq!(files[0].content, content);
        assert!(warnings.is_empty());
        std::fs::File::create(path)
            .unwrap()
            .set_len(MAX_TREE_FILE_BYTES + 1)
            .unwrap();
        assert!(collect_tree(&[temp.path().to_owned()], &mut warnings).is_none());
        assert!(warnings[0].contains("keeping the previous complete tree"));
    }

    #[test]
    fn aggregate_limit_preserves_the_previous_mirror() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["one", "two", "three"] {
            std::fs::File::create(temp.path().join(name))
                .unwrap()
                .set_len(MAX_TREE_FILE_BYTES)
                .unwrap();
        }
        let mut warnings = Vec::new();
        assert!(collect_tree(&[temp.path().to_owned()], &mut warnings).is_none());
        assert!(warnings[0].contains("128 MiB"));
    }

    #[cfg(unix)]
    #[test]
    fn broken_skill_links_do_not_replace_a_complete_mirror() {
        let temp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("missing", temp.path().join("dependency")).unwrap();
        let mut warnings = Vec::new();
        assert!(collect_tree(&[temp.path().to_owned()], &mut warnings).is_none());
        assert!(warnings[0].contains("keeping the previous complete tree"));
    }
}
