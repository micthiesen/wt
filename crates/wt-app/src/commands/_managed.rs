//! Worker half of a managed remote. Runs from the provisioned runtime before a
//! worker config exists, so it never loads configuration. The controller sends
//! every file; this side writes them, keeps the clone present, and reports the
//! checks that need a person.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    io::{IsTerminal, Read, Write},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessError, ProcessOutput, ProcessRunner};
use wt_remote::managed::{
    CODEX_LOGIN_PORT, CheckState, MANAGED_PROTOCOL, ManagedCheck, ManagedFile, ManagedReport,
    ManagedRequest, ROOT_PLACEHOLDER, SignIn,
};

const CLONE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(15);
const PATH_MARKER: &str = "__WT_LOGIN_PATH__=";
const TREE_MARKER: &str = ".wt-tree-sha256";
const TREE_MANIFEST: &str = ".wt-tree-files";

pub async fn run(args: &[String], cancellation: &CancellationToken) -> Result<i32> {
    let argv = match args {
        [payload] => wt_remote::decode_remote_args(payload)?,
        _ => bail!("usage: wt _managed <payload>"),
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set on this worker")?;
    match argv
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["apply"] => {
            let mut input = Vec::new();
            std::io::stdin()
                .read_to_end(&mut input)
                .context("read managed request")?;
            let request: ManagedRequest =
                serde_json::from_slice(&input).context("parse managed request")?;
            let report = apply(&home, &request, cancellation).await?;
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer(&mut stdout, &report)?;
            stdout.write_all(b"\n")?;
            Ok(0)
        }
        ["bridge-probe"] => crate::commands::controller_bridge::exec("__wt_status").await,
        ["bridge-path"] => {
            let path = std::env::var_os("WT_CONTROLLER_SOCKET")
                .context("worker has no controller services")?;
            let path = PathBuf::from(path);
            println!("{}", path.display());
            Ok(0)
        }
        ["auth-sync", generation] => {
            let root = std::env::var_os("WT_MANAGED_ROOT").context("worker has no managed root")?;
            let generation = generation
                .parse::<u64>()
                .context("invalid account-switch generation")?;
            crate::codex_reauth::write_worker_desired(
                &PathBuf::from(root),
                Some(generation),
                cancellation,
            )
            .await?;
            Ok(0)
        }
        ["login", provider] => {
            let provider = SignIn::parse(provider)
                .with_context(|| format!("unknown managed sign-in {provider:?}"))?;
            sign_in(provider, cancellation).await
        }
        _ => bail!("unknown managed command {argv:?}"),
    }
}

/// Converge the managed root on the controller's request and report checks.
pub async fn apply(
    home: &Path,
    request: &ManagedRequest,
    cancellation: &CancellationToken,
) -> Result<ManagedReport> {
    if request.protocol != MANAGED_PROTOCOL {
        bail!(
            "managed request protocol {} does not match worker protocol {MANAGED_PROTOCOL}",
            request.protocol
        );
    }
    let relative_root = safe_relative(&request.root).context("managed root")?;
    let root = home.join(relative_root);
    let root_text = root
        .to_str()
        .filter(|text| !text.contains(['\'', '"', '\n', '\\']))
        .context("the managed root path must not contain quotes or newlines")?
        .to_owned();
    create_private_dir(&root)?;
    crate::codex_reauth::write_worker_desired(&root, request.codex_auth_generation, cancellation)
        .await?;
    // A reconnect and a finished sign-in can both converge the same root;
    // one apply at a time keeps the clone and mirrored trees consistent.
    let lock_path = root.join(".apply.lock");
    let _apply_lock = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        file.lock()
            .with_context(|| format!("lock {}", lock_path.display()))?;
        Ok(file)
    })
    .await
    .context("managed apply lock task")??;
    for directory in ["bin", "secrets", "codex", "gh", "worktrees"] {
        create_private_dir(&root.join(directory))?;
    }

    for file in &request.files {
        write_managed(&root, file, &root_text)?;
    }
    for path in &request.remove {
        let path = root.join(safe_relative(path)?);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
        }
    }
    for tree in &request.trees {
        mirror_tree(&root, &tree.path, &tree.files, &root_text)?;
    }
    link_runtime(&root)?;
    let mut checks = Vec::new();
    if let Some(config) = &request.codex_config {
        // The parse error quotes the offending line, which may hold a token.
        match adapt_codex_config(config, &root) {
            Ok(adapted) => {
                write_if_changed(&root.join("codex/config.toml"), adapted.as_bytes(), 0o600)?
            }
            Err(_) => checks.push(ManagedCheck::new(
                "codex-config",
                CheckState::Warning,
                "The controller's Codex config.toml does not parse, so the worker keeps its previous copy",
                None,
            )),
        }
    }

    let runner = ProcessRunner::default();
    let login_path = capture_login_path(&runner, &root, cancellation).await;
    write_if_changed(&root.join("login-path"), login_path.as_bytes(), 0o600)?;
    let environment = child_environment(&root, &login_path);

    let token = github_token_present(&root, &runner, &environment, cancellation).await;
    let repository =
        ensure_repository(&root, request, &runner, &environment, token, cancellation).await;
    let repository_ready = repository.state == CheckState::Ok;
    checks.push(repository);
    if repository_ready {
        write_repo_files(
            &root,
            request,
            &runner,
            &environment,
            &root_text,
            cancellation,
        )
        .await?;
    }
    checks.push(if token {
        ManagedCheck::ok("github", "GitHub credentials are available")
    } else {
        ManagedCheck::new(
            "github",
            CheckState::SignInNeeded,
            "GitHub sign-in needed: git push and gh commands will fail",
            Some(SignIn::Github),
        )
    });
    checks.extend(codex_checks(&runner, &environment, cancellation).await);
    if crate::codex_reauth::worker_pending(&root)? {
        checks.retain(|check| check.id != "codex-login");
        checks.push(ManagedCheck::new(
            "codex-login",
            CheckState::SignInNeeded,
            "The controller changed Codex accounts; this managed host needs its own sign-in",
            Some(SignIn::Codex),
        ));
    }
    checks.push(match probe(&runner, &environment, "gh", &["--version"], cancellation).await {
        Some(output) if output.status.success() => ManagedCheck::ok("gh", "gh is installed"),
        _ => ManagedCheck::new(
            "gh",
            CheckState::Warning,
            "gh is not installed: agents cannot open or inspect pull requests (brew install gh)",
            None,
        ),
    });
    Ok(ManagedReport {
        protocol: MANAGED_PROTOCOL,
        checks,
        codex_auth_generation: crate::codex_reauth::worker_desired(&root)?,
    })
}

async fn sign_in(provider: SignIn, cancellation: &CancellationToken) -> Result<i32> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("managed sign-in requires an interactive terminal; no daemon was stopped");
    }
    let root = std::env::var_os("WT_MANAGED_ROOT")
        .map(PathBuf::from)
        .context("this managed worker has not been provisioned yet; reconnect and retry")?;
    // The controller forwards Codex's callback port, so the browser on the
    // controller completes the sign-in on this worker's loopback. Codex moves
    // to another port when that one is taken, which the forward would miss,
    // so a busy port selects the device-code flow instead.
    let callback_free = std::net::TcpListener::bind(("127.0.0.1", CODEX_LOGIN_PORT)).is_ok();
    let (program, args): (&str, Vec<&str>) = match provider {
        SignIn::Codex if callback_free => ("codex", vec!["login"]),
        SignIn::Codex => ("codex", vec!["login", "--device-auth"]),
        SignIn::Github => (
            "gh",
            vec![
                "auth",
                "login",
                "--hostname",
                "github.com",
                "--git-protocol",
                "https",
                "--insecure-storage",
                "--web",
            ],
        ),
    };
    println!(
        "Signing in to {} for wt's managed worker ({}).",
        provider.label(),
        root.display()
    );
    match provider {
        SignIn::Codex if callback_free => println!(
            "Open the sign-in link below in the browser on the machine running wt; the callback is forwarded here."
        ),
        SignIn::Codex => println!(
            "Port {CODEX_LOGIN_PORT} is in use on this host, so Codex uses a device code. Device-code sign-in must be enabled in ChatGPT's security settings."
        ),
        SignIn::Github => {}
    }
    // Pin the managed homes rather than trusting the inherited environment,
    // so a sign-in can never land in this account's own ~/.codex or gh config.
    let login_path = std::fs::read_to_string(root.join("login-path")).unwrap_or_default();
    let environment = child_environment(&root, login_path.trim());
    let status = if provider == SignIn::Codex {
        codex_login_lifecycle(&root, &environment, &args, cancellation).await
    } else {
        interactive_login(program, &args, &environment, cancellation).await
    };
    if status.is_err() {
        if let Err(error) = &status {
            eprintln!("wt: {error:#}");
        }
        eprintln!("Sign-in did not complete. Press Enter or Ctrl+C to return to wt.");
        // wt handles Ctrl+C itself, so wait for either. A detached thread owns
        // the blocking read; exiting the process ends it.
        let (entered, line) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::stdin().read_line(&mut text);
            let _ = entered.send(());
        });
        tokio::select! {
            _ = cancellation.cancelled() => {}
            _ = line => {}
        }
        return Ok(1);
    }
    Ok(0)
}

async fn interactive_login(
    program: &str,
    args: &[&str],
    environment: &[(OsString, Option<OsString>)],
    cancellation: &CancellationToken,
) -> Result<()> {
    let mut command = tokio::process::Command::new(program);
    command.args(args).kill_on_drop(true);
    for (name, value) in environment {
        if let Some(value) = value {
            command.env(name, value);
        } else {
            command.env_remove(name);
        }
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start `{program}`"))?;
    let status = tokio::select! {
        _ = cancellation.cancelled() => {
            child.kill().await.context("stop cancelled login")?;
            bail!("sign-in cancelled");
        }
        status = child.wait() => status.context("wait for native login")?,
    };
    if !status.success() {
        bail!("native login exited with {status}");
    }
    Ok(())
}

async fn codex_login_lifecycle(
    root: &Path,
    environment: &[(OsString, Option<OsString>)],
    login_args: &[&str],
    cancellation: &CancellationToken,
) -> Result<()> {
    if cancellation.is_cancelled() {
        bail!("sign-in cancelled before daemon changes");
    }
    let _lock =
        wt_platform::lock::FileLock::try_acquire(root, "codex-login", "managed Codex login")
            .await?
            .context("another managed Codex sign-in is already running")?;
    let expected = crate::codex_reauth::worker_desired(root)?;
    let runner = ProcessRunner::default();
    // Never signal arbitrary processes or touch tmux. Native daemon ownership
    // checks scope the stop/restart to this worker's pinned CODEX_HOME.
    // Native 0.162.1 serializes stop against its updater. The updater's
    // try_restart_if_running cannot resurrect a stopped daemon, so leave the
    // updater running instead of introducing custom PID ownership/signals.
    let stopped = codex_control(
        &runner,
        environment,
        &["app-server", "daemon", "stop"],
        cancellation,
    )
    .await;
    let login = match stopped {
        Ok(()) => interactive_login("codex", login_args, environment, cancellation).await,
        Err(error) => Err(error),
    };
    // Recovery is owed even when the transport cancelled its interactive child.
    let recovery = codex_control(
        &runner,
        environment,
        &["app-server", "daemon", "restart"],
        &CancellationToken::new(),
    )
    .await;
    if let Err(error) = recovery {
        bail!("managed Codex daemon recovery failed: {error:#}; sign-in remains pending");
    }
    login?;
    codex_control(&runner, environment, &["login", "status"], cancellation).await?;
    crate::codex_reauth::acknowledge(root, expected, cancellation).await
}

async fn codex_control(
    runner: &ProcessRunner,
    environment: &[(OsString, Option<OsString>)],
    args: &[&str],
    cancellation: &CancellationToken,
) -> Result<()> {
    let mut spec = CommandSpec::new("codex").args(args.iter().copied());
    spec.env = environment.to_vec();
    // Native daemon stop serializes with updates (up to 375s lock wait) and
    // allows a configured shutdown grace up to 300s. Do not abort a healthy
    // drain with the ordinary short read-probe timeout.
    let daemon = args.starts_with(&["app-server", "daemon"]);
    spec.timeout = if daemon {
        Duration::from_secs(15 * 60)
    } else {
        PROBE_TIMEOUT
    };
    let operation = runner.run(spec, cancellation);
    tokio::pin!(operation);
    let mut progress = tokio::time::interval(Duration::from_secs(5));
    progress.tick().await;
    let output = loop {
        tokio::select! {
            output = &mut operation => break output?,
            _ = progress.tick(), if daemon => {
                eprintln!("Waiting for managed Codex daemon {} (finishing active work or another native update)…", args.last().copied().unwrap_or("operation"));
            }
        }
    };
    if !output.status.success() {
        // Native errors may include credential material; expose the operation
        // and status, never captured login output.
        bail!("codex {} exited with {}", args.join(" "), output.status);
    }
    Ok(())
}

async fn codex_checks(
    runner: &ProcessRunner,
    environment: &[(OsString, Option<OsString>)],
    cancellation: &CancellationToken,
) -> Vec<ManagedCheck> {
    match probe(runner, environment, "codex", &["--version"], cancellation).await {
        Some(output) if output.status.success() => {}
        _ => {
            return vec![ManagedCheck::new(
                "codex",
                CheckState::Failed,
                "Codex is not installed on this host: agents cannot start (brew install codex)",
                None,
            )];
        }
    }
    let status = probe(
        runner,
        environment,
        "codex",
        &["login", "status"],
        cancellation,
    )
    .await;
    vec![match status {
        Some(output) if output.status.success() => {
            ManagedCheck::ok("codex-login", "Codex is signed in")
        }
        _ => ManagedCheck::new(
            "codex-login",
            CheckState::SignInNeeded,
            "Codex sign-in needed before agents can start",
            Some(SignIn::Codex),
        ),
    }]
}

async fn github_token_present(
    root: &Path,
    runner: &ProcessRunner,
    environment: &[(OsString, Option<OsString>)],
    cancellation: &CancellationToken,
) -> bool {
    if std::fs::read_to_string(root.join("secrets/github-token"))
        .is_ok_and(|token| !token.trim().is_empty())
    {
        return true;
    }
    probe(
        runner,
        environment,
        "gh",
        &["auth", "token", "--hostname", "github.com"],
        cancellation,
    )
    .await
    .is_some_and(|output| output.status.success() && !output.stdout_text().trim().is_empty())
}

async fn ensure_repository(
    root: &Path,
    request: &ManagedRequest,
    runner: &ProcessRunner,
    environment: &[(OsString, Option<OsString>)],
    token: bool,
    cancellation: &CancellationToken,
) -> ManagedCheck {
    let repo = root.join("repo");
    if repo.join(".git").exists() {
        let origin = git(
            runner,
            environment,
            Some(&repo),
            &["remote", "get-url", "origin"],
            PROBE_TIMEOUT,
            cancellation,
        )
        .await;
        return match origin {
            Ok(output) if output.status.success() => {
                let actual = output.stdout_text().trim().to_owned();
                if same_repository(&actual, &request.repository) {
                    ManagedCheck::ok("repository", "Repository clone is present")
                } else {
                    ManagedCheck::new(
                        "repository",
                        CheckState::Failed,
                        format!(
                            "The managed clone at {} tracks {actual}, not {}; move it aside to re-clone",
                            repo.display(),
                            request.repository
                        ),
                        None,
                    )
                }
            }
            Ok(output) => ManagedCheck::new(
                "repository",
                CheckState::Failed,
                format!(
                    "Cannot read the managed clone's origin: {}",
                    tail(&output.stderr_text())
                ),
                None,
            ),
            Err(error) => ManagedCheck::new(
                "repository",
                CheckState::Failed,
                format!("Cannot read the managed clone's origin: {error}"),
                None,
            ),
        };
    }
    // Clone into a staging name so an interrupted clone never looks complete.
    let staging = root.join(".repo-clone");
    let _ = std::fs::remove_dir_all(&staging);
    let staging_text = staging.to_string_lossy().into_owned();
    let result = git(
        runner,
        environment,
        None,
        &["clone", "--", &request.repository, &staging_text],
        CLONE_TIMEOUT,
        cancellation,
    )
    .await;
    match result {
        Ok(output) if output.status.success() => match std::fs::rename(&staging, &repo) {
            Ok(()) => ManagedCheck::ok("repository", "Cloned the repository"),
            Err(error) => ManagedCheck::new(
                "repository",
                CheckState::Failed,
                format!("Cloned, but could not move the clone into place: {error}"),
                None,
            ),
        },
        Ok(output) => {
            let _ = std::fs::remove_dir_all(&staging);
            let stderr = output.stderr_text();
            let auth = [
                "Authentication failed",
                "could not read Username",
                "Repository not found",
                "terminal prompts disabled",
            ]
            .iter()
            .any(|needle| stderr.contains(needle));
            if auth && !token {
                ManagedCheck::new(
                    "repository",
                    CheckState::SignInNeeded,
                    "GitHub sign-in needed to clone the repository",
                    Some(SignIn::Github),
                )
            } else {
                ManagedCheck::new(
                    "repository",
                    CheckState::Failed,
                    format!("Cloning {} failed: {}", request.repository, tail(&stderr)),
                    None,
                )
            }
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            ManagedCheck::new(
                "repository",
                CheckState::Failed,
                format!("Cloning {} failed: {error}", request.repository),
                None,
            )
        }
    }
}

/// Compare clone URLs ignoring a trailing `.git` and slash.
fn same_repository(left: &str, right: &str) -> bool {
    fn normalize(url: &str) -> &str {
        let url = url.trim().trim_end_matches('/');
        url.strip_suffix(".git").unwrap_or(url)
    }
    normalize(left).eq_ignore_ascii_case(normalize(right))
}

async fn git(
    runner: &ProcessRunner,
    environment: &[(OsString, Option<OsString>)],
    cwd: Option<&Path>,
    args: &[&str],
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<ProcessOutput, ProcessError> {
    let mut spec = CommandSpec::new("git").args(args.iter().copied());
    spec.env = environment.to_vec();
    spec.cwd = cwd.map(Path::to_path_buf);
    spec.timeout = timeout;
    runner.run(spec, cancellation).await
}

async fn probe(
    runner: &ProcessRunner,
    environment: &[(OsString, Option<OsString>)],
    program: &str,
    args: &[&str],
    cancellation: &CancellationToken,
) -> Option<ProcessOutput> {
    let mut spec = CommandSpec::new(program).args(args.iter().copied());
    spec.env = environment.to_vec();
    spec.timeout = PROBE_TIMEOUT;
    runner.run(spec, cancellation).await.ok()
}

/// The managed variables every child of this command needs. The environment
/// file may not exist yet on a first connection, so they are set explicitly.
fn child_environment(root: &Path, login_path: &str) -> Vec<(OsString, Option<OsString>)> {
    let live = std::env::var_os("PATH").unwrap_or_default();
    let path = child_path(root, login_path, &live);
    vec![
        ("PATH".into(), Some(path)),
        ("WT_MANAGED_ROOT".into(), Some(root.into())),
        ("WT_MANAGED_FILES".into(), Some(root.join("files").into())),
        ("CODEX_HOME".into(), Some(root.join("codex").into())),
        ("GH_CONFIG_DIR".into(), Some(root.join("gh").into())),
        (
            "GIT_CONFIG_GLOBAL".into(),
            Some(root.join("gitconfig").into()),
        ),
        ("GIT_TERMINAL_PROMPT".into(), Some("0".into())),
    ]
}

fn child_path(root: &Path, login_path: &str, live: &std::ffi::OsStr) -> OsString {
    let mut seen = BTreeSet::new();
    std::env::join_paths(
        std::iter::once(root.join("bin"))
            .chain(std::env::split_paths(login_path))
            .chain(std::env::split_paths(live))
            .filter(|entry| seen.insert(entry.clone())),
    )
    .unwrap_or_else(|_| OsString::from(login_path))
}

/// Read PATH from the user's login shell. Non-interactive SSH commands on
/// macOS get only the system PATH, which misses Homebrew and version managers.
async fn capture_login_path(
    runner: &ProcessRunner,
    root: &Path,
    cancellation: &CancellationToken,
) -> String {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    let script = format!("printf '\\n{PATH_MARKER}%s\\n' \"$PATH\"");
    for flags in [["-l", "-i"].as_slice(), ["-l"].as_slice()] {
        let mut spec = CommandSpec::new(&shell)
            .args(flags.iter().copied())
            .args(["-c", script.as_str()]);
        spec.timeout = LOGIN_SHELL_TIMEOUT;
        spec.output_limit = 1024 * 1024;
        let Ok(output) = runner.run(spec, cancellation).await else {
            continue;
        };
        if let Some(path) = output
            .stdout_text()
            .lines()
            .rev()
            .find_map(|line| line.strip_prefix(PATH_MARKER))
            .filter(|path| !path.trim().is_empty())
        {
            return clean_path(path, root);
        }
    }
    clean_path(&std::env::var("PATH").unwrap_or_default(), root)
}

/// Drop managed and runtime-cache entries a prior environment file added, and
/// duplicates, so repeated connections never grow PATH.
fn clean_path(path: &str, root: &Path) -> String {
    let mut seen = BTreeSet::new();
    std::env::split_paths(path)
        .filter(|entry| !entry.as_os_str().is_empty())
        .filter(|entry| !entry.starts_with(root))
        .filter(|entry| {
            !entry
                .to_string_lossy()
                .contains("/.cache/wt/native-runtimes/")
        })
        .filter(|entry| seen.insert(entry.clone()))
        .map(|entry| entry.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

/// Keep a stable `wt` on PATH for agents and scripts on this worker. It points
/// at the runtime this controller connection bound to.
fn link_runtime(root: &Path) -> Result<()> {
    let runtime = std::env::current_exe().context("resolve running wt runtime")?;
    let link = root.join("bin/wt");
    if std::fs::read_link(&link).is_ok_and(|current| current == runtime) {
        return Ok(());
    }
    let staging = root.join("bin/.wt-link");
    let _ = std::fs::remove_file(&staging);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&runtime, &staging).context("link managed wt runtime")?;
    std::fs::rename(&staging, &link).context("publish managed wt runtime link")?;
    Ok(())
}

/// Mirror the controller's Codex settings for this worker: credentials stay in
/// a file (SSH sessions cannot unlock the login keychain) and the managed
/// clone and worktrees are trusted. Controller project trust does not apply.
pub(crate) fn adapt_codex_config(raw: &str, root: &Path) -> Result<String> {
    let mut value: toml::Table = toml::from_str(raw).context("parse controller Codex config")?;
    value.insert(
        "cli_auth_credentials_store".into(),
        toml::Value::String("file".into()),
    );
    let mut projects = toml::Table::new();
    for directory in ["repo", "worktrees"] {
        let mut trust = toml::Table::new();
        trust.insert("trust_level".into(), toml::Value::String("trusted".into()));
        projects.insert(
            root.join(directory).to_string_lossy().into_owned(),
            toml::Value::Table(trust),
        );
    }
    value.insert("projects".into(), toml::Value::Table(projects));
    Ok(toml::to_string(&value)?)
}

fn write_managed(base: &Path, file: &ManagedFile, root_text: &str) -> Result<()> {
    let content = substitute(&file.content, root_text);
    write_managed_content(base, file, &content)
}

fn write_managed_content(base: &Path, file: &ManagedFile, content: &[u8]) -> Result<()> {
    let relative = safe_relative(&file.path)?;
    let path = base.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    write_if_changed(&path, content, file.mode)
}

/// Copy the controller's untracked project files into the managed clone.
/// Files the worker's checkout tracks are left alone, and no path may pass
/// through a symlink, because the clone's contents come from the repository.
async fn write_repo_files(
    root: &Path,
    request: &ManagedRequest,
    runner: &ProcessRunner,
    environment: &[(OsString, Option<OsString>)],
    root_text: &str,
    cancellation: &CancellationToken,
) -> Result<()> {
    if request.repo_files.is_empty() {
        return Ok(());
    }
    let repo = root.join("repo");
    let mut args = vec!["--literal-pathspecs", "ls-files", "-z", "--"];
    args.extend(request.repo_files.iter().map(|file| file.path.as_str()));
    let tracked = git(
        runner,
        environment,
        Some(&repo),
        &args,
        PROBE_TIMEOUT,
        cancellation,
    )
    .await
    .ok()
    .filter(|output| output.status.success())
    .context("list tracked files in the managed clone")?
    .stdout_text()
    .split('\0')
    .filter(|path| !path.is_empty())
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    for file in &request.repo_files {
        if tracked.contains(&file.path) || crosses_symlink(&repo, &safe_relative(&file.path)?) {
            continue;
        }
        write_managed(&repo, file, root_text)?;
    }
    Ok(())
}

/// Whether any existing component of `relative` under `base` is a symlink.
fn crosses_symlink(base: &Path, relative: &Path) -> bool {
    let mut path = base.to_path_buf();
    for component in relative.components() {
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

fn substitute(content: &[u8], root_text: &str) -> Vec<u8> {
    match std::str::from_utf8(content) {
        Ok(text) if text.contains(ROOT_PLACEHOLDER) => {
            text.replace(ROOT_PLACEHOLDER, root_text).into_bytes()
        }
        _ => content.to_vec(),
    }
}

/// Replace a directory only when its manifest changed, through a staged copy,
/// so a reader never sees a half-written tree.
fn mirror_tree(root: &Path, path: &str, files: &[ManagedFile], root_text: &str) -> Result<()> {
    if path == "files" && crosses_symlink(root, &safe_relative(path)?) {
        bail!("managed dependency destination must not be a symlink");
    }
    let destination = root.join(safe_relative(path)?);
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.path.as_bytes());
        hasher.update([0]);
        hasher.update(file.mode.to_le_bytes());
        hasher.update(&file.content);
        hasher.update([0]);
    }
    let digest = format!("{:x}", hasher.finalize());
    if std::fs::read_to_string(destination.join(TREE_MARKER)).is_ok_and(|seen| seen == digest)
        && (path != "files" || declared_files_current(&destination, files))
    {
        return Ok(());
    }
    let parent = destination
        .parent()
        .context("a mirrored tree needs a parent directory")?;
    let name = destination
        .file_name()
        .context("a mirrored tree needs a name")?
        .to_string_lossy()
        .into_owned();
    let staging = parent.join(format!(".{name}.wt-new"));
    let retired = parent.join(format!(".{name}.wt-old"));
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&retired);
    create_private_dir(&staging)?;
    for file in files {
        if path == "files" {
            write_managed_content(&staging, file, &file.content)?;
        } else {
            write_managed(&staging, file, root_text)?;
        }
    }
    write_if_changed(&staging.join(TREE_MARKER), digest.as_bytes(), 0o600)?;
    // Keep what the worker created itself (Codex's `.system` skills, rules it
    // records from approvals): only entries wt sent before or sends now are
    // replaced.
    let previous = std::fs::read_to_string(destination.join(TREE_MANIFEST)).unwrap_or_default();
    let owned = previous
        .lines()
        .chain(files.iter().map(|file| file.path.as_str()))
        .filter_map(|path| path.split('/').next())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let manifest = files
        .iter()
        .map(|file| format!("{}\n", file.path))
        .collect::<String>();
    write_if_changed(&staging.join(TREE_MANIFEST), manifest.as_bytes(), 0o600)?;
    if let Ok(entries) = std::fs::read_dir(&destination) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == TREE_MARKER || name == TREE_MANIFEST || owned.contains(&name) {
                continue;
            }
            std::fs::rename(entry.path(), staging.join(&name))
                .with_context(|| format!("keep {}", entry.path().display()))?;
        }
    }
    if destination.exists() {
        std::fs::rename(&destination, &retired)
            .with_context(|| format!("retire {}", destination.display()))?;
    }
    std::fs::rename(&staging, &destination)
        .with_context(|| format!("publish {}", destination.display()))?;
    let _ = std::fs::remove_dir_all(&retired);
    Ok(())
}

/// Unlike optional skill caches, declared hook dependencies must be restored
/// even when their controller-side manifest has not changed.
fn declared_files_current(destination: &Path, files: &[ManagedFile]) -> bool {
    files.iter().all(|file| {
        let Ok(relative) = safe_relative(&file.path) else {
            return false;
        };
        if crosses_symlink(destination, &relative) {
            return false;
        }
        let path = destination.join(relative);
        let Ok(metadata) = std::fs::metadata(&path) else {
            return false;
        };
        if !metadata.is_file() || metadata.len() != file.content.len() as u64 {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o7777 != file.mode & 0o7777 {
                return false;
            }
        }
        let Ok(input) = std::fs::File::open(path) else {
            return false;
        };
        let mut actual = Vec::new();
        input
            .take(file.content.len() as u64 + 1)
            .read_to_end(&mut actual)
            .is_ok()
            && actual == file.content
    })
}

fn write_if_changed(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    let unchanged = std::fs::read(path).is_ok_and(|current| current == content);
    if !unchanged {
        let name = path
            .file_name()
            .context("managed file needs a name")?
            .to_string_lossy()
            .into_owned();
        let staging = path.with_file_name(format!(".{name}.wt-stage"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = options
            .open(&staging)
            .with_context(|| format!("write {}", staging.display()))?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&staging, path).with_context(|| format!("publish {}", path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let current = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if current != mode {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict {}", path.display()))?;
    }
    Ok(())
}

/// Accept only plain relative paths, so a request cannot write outside its
/// base directory.
fn safe_relative(path: &str) -> Result<PathBuf> {
    let candidate = Path::new(path);
    if path.is_empty()
        || !candidate
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        bail!("managed path {path:?} must be relative without `..`");
    }
    Ok(candidate.to_path_buf())
}

fn tail(text: &str) -> String {
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    lines[lines.len().saturating_sub(3)..].join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_relogin_recovers_after_failure_and_acknowledges_only_verified_success() {
        use std::os::unix::fs::PermissionsExt;
        for failure in ["", "stop", "login", "restart", "status", "cancel"] {
            let temp = tempfile::tempdir().unwrap();
            let program = temp.path().join("codex");
            std::fs::write(
                &program,
                r#"#!/bin/sh
printf '%s\n' "$*" >> "$QA_CALLS"
case "$*:$QA_FAILURE" in
  'app-server daemon stop:stop'|'login:login'|'app-server daemon restart:restart'|'login status:status') exit 1 ;;
  'login:cancel') exec /bin/sleep 30 ;;
esac
exit 0
"#,
            )
            .unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            let calls = temp.path().join("calls");
            let environment = vec![
                ("PATH".into(), Some(temp.path().as_os_str().to_owned())),
                (
                    "CODEX_HOME".into(),
                    Some(temp.path().join("codex-home").into()),
                ),
                ("QA_CALLS".into(), Some(calls.as_os_str().to_owned())),
                ("QA_FAILURE".into(), Some(failure.into())),
            ];
            let cancel = CancellationToken::new();
            crate::codex_reauth::write_worker_desired(temp.path(), Some(5), &cancel)
                .await
                .unwrap();
            let canceller = (failure == "cancel").then(|| {
                let calls = calls.clone();
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    tokio::time::timeout(Duration::from_secs(3), async {
                        loop {
                            if std::fs::read_to_string(&calls)
                                .is_ok_and(|text| text.contains("\nlogin\n"))
                            {
                                cancel.cancel();
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .unwrap();
                })
            });
            let result =
                codex_login_lifecycle(temp.path(), &environment, &["login"], &cancel).await;
            if let Some(canceller) = canceller {
                canceller.await.unwrap();
            }
            assert_eq!(result.is_ok(), failure.is_empty(), "{failure}: {result:?}");
            assert_eq!(
                crate::codex_reauth::worker_pending(temp.path()).unwrap(),
                !failure.is_empty()
            );
            let calls = std::fs::read_to_string(calls).unwrap();
            if failure == "stop" {
                assert_eq!(calls, "app-server daemon stop\napp-server daemon restart\n");
            } else {
                assert!(
                    calls.starts_with("app-server daemon stop\nlogin\napp-server daemon restart\n"),
                    "{calls}"
                );
            }
            if failure.is_empty() {
                assert!(calls.ends_with("login status\n"));
            }
        }
    }

    #[test]
    fn declared_dependencies_repair_drift_and_preserve_literal_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let files = [ManagedFile {
            path: "scripts/helper".into(),
            content: format!("literal {ROOT_PLACEHOLDER}\n").into_bytes(),
            mode: 0o700,
        }];
        let helper = temp.path().join("files/scripts/helper");
        mirror_tree(temp.path(), "files", &files, "/worker").unwrap();
        assert_eq!(std::fs::read(&helper).unwrap(), files[0].content);
        std::fs::write(&helper, vec![b'x'; files[0].content.len()]).unwrap();
        mirror_tree(temp.path(), "files", &files, "/worker").unwrap();
        assert_eq!(std::fs::read(&helper).unwrap(), files[0].content);
        std::fs::remove_file(&helper).unwrap();
        mirror_tree(temp.path(), "files", &files, "/worker").unwrap();
        assert_eq!(std::fs::read(&helper).unwrap(), files[0].content);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o777)).unwrap();
            mirror_tree(temp.path(), "files", &files, "/worker").unwrap();
            assert_eq!(
                std::fs::metadata(&helper).unwrap().permissions().mode() & 0o7777,
                0o700
            );
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("helper"), "untouched").unwrap();
            std::fs::remove_dir_all(helper.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(outside.path(), helper.parent().unwrap()).unwrap();
            mirror_tree(temp.path(), "files", &files, "/worker").unwrap();
            assert_eq!(std::fs::read(&helper).unwrap(), files[0].content);
            assert_eq!(
                std::fs::read_to_string(outside.path().join("helper")).unwrap(),
                "untouched"
            );
            assert!(
                !std::fs::symlink_metadata(helper.parent().unwrap())
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[test]
    fn declared_dependency_mirror_removes_old_files_and_refuses_symlink_root() {
        let temp = tempfile::tempdir().unwrap();
        let files = [ManagedFile {
            path: "scripts/helper".into(),
            content: b"helper".to_vec(),
            mode: 0o700,
        }];
        mirror_tree(temp.path(), "files", &files, "/worker").unwrap();
        assert!(temp.path().join("files/scripts/helper").exists());
        mirror_tree(temp.path(), "files", &[], "/worker").unwrap();
        assert!(!temp.path().join("files/scripts/helper").exists());
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("keep"), "untouched").unwrap();
            std::fs::remove_dir_all(temp.path().join("files")).unwrap();
            std::os::unix::fs::symlink(outside.path(), temp.path().join("files")).unwrap();
            assert!(mirror_tree(temp.path(), "files", &files, "/worker").is_err());
            assert_eq!(
                std::fs::read_to_string(outside.path().join("keep")).unwrap(),
                "untouched"
            );
            assert!(!outside.path().join("scripts/helper").exists());
        }
    }

    #[test]
    fn requested_paths_cannot_escape_their_base() {
        for bad in ["", "/etc/passwd", "../x", "a/../../x", "./x"] {
            assert!(safe_relative(bad).is_err(), "{bad}");
        }
        assert_eq!(
            safe_relative("codex/skills/a").unwrap(),
            Path::new("codex/skills/a")
        );
    }

    #[test]
    fn codex_config_trusts_only_managed_paths_and_keeps_credentials_in_a_file() {
        let raw = "model = \"gpt-5\"\ncli_auth_credentials_store = \"keyring\"\n[projects.\"/Users/me/Code/app\"]\ntrust_level = \"trusted\"\n[mcp_servers.docs]\ncommand = \"docs\"\n";
        let adapted: toml::Table =
            toml::from_str(&adapt_codex_config(raw, Path::new("/home/me/m")).unwrap()).unwrap();
        assert_eq!(adapted["model"].as_str(), Some("gpt-5"));
        assert_eq!(adapted["cli_auth_credentials_store"].as_str(), Some("file"));
        let projects = adapted["projects"].as_table().unwrap();
        assert_eq!(
            projects.keys().cloned().collect::<Vec<_>>(),
            ["/home/me/m/repo", "/home/me/m/worktrees"]
        );
        assert!(adapted["mcp_servers"]["docs"].is_table());
    }

    #[test]
    fn child_path_is_idempotent_and_keeps_managed_then_login_then_live_order() {
        let root = Path::new("/managed");
        let login = "/tools with spaces:/usr/bin:/tools with spaces";
        let live = std::ffi::OsStr::new("/usr/bin:/managed/bin::/live-only:");
        let first = child_path(root, login, live);
        assert_eq!(
            first,
            OsString::from("/managed/bin:/tools with spaces:/usr/bin::/live-only")
        );
        for _ in 0..25 {
            assert_eq!(child_path(root, login, &first), first);
        }
    }

    #[test]
    fn login_path_drops_managed_and_runtime_entries_and_duplicates() {
        let root = Path::new("/Users/me/.local/share/wt/managed/app");
        let path = "/Users/me/.local/share/wt/managed/app/bin:/opt/homebrew/bin:/usr/bin:/Users/me/.cache/wt/native-runtimes/x/y:/opt/homebrew/bin::/bin";
        assert_eq!(clean_path(path, root), "/opt/homebrew/bin:/usr/bin:/bin");
    }

    #[test]
    fn repository_identity_ignores_git_suffix_and_case() {
        assert!(same_repository(
            "https://github.com/Acme/App.git",
            "https://github.com/acme/app/"
        ));
        assert!(!same_repository(
            "https://github.com/acme/app",
            "https://github.com/acme/other"
        ));
    }

    #[test]
    fn repository_files_never_pass_through_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(repo.join("apps")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, repo.join("apps/web")).unwrap();
        assert!(crosses_symlink(&repo, Path::new("apps/web/.env")));
        assert!(!crosses_symlink(&repo, Path::new("apps/api/.env")));
        assert!(!crosses_symlink(&repo, Path::new(".env")));
    }

    #[test]
    fn trees_are_replaced_whole_and_files_only_when_changed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let file = |path: &str, content: &str| ManagedFile {
            path: path.into(),
            content: content.as_bytes().to_vec(),
            mode: 0o600,
        };
        mirror_tree(
            root,
            "codex/skills",
            &[file("a/SKILL.md", "one"), file("b/SKILL.md", "two")],
            "/r",
        )
        .unwrap();
        mirror_tree(
            root,
            "codex/skills",
            &[file("a/SKILL.md", "@@WT_MANAGED_ROOT@@/x")],
            "/r",
        )
        .unwrap();
        let skills = root.join("codex/skills");
        assert_eq!(
            std::fs::read_to_string(skills.join("a/SKILL.md")).unwrap(),
            "/r/x"
        );
        assert!(!skills.join("b").exists());

        // Entries the worker created itself survive the next replacement.
        std::fs::create_dir_all(skills.join(".system/builtin")).unwrap();
        std::fs::write(skills.join(".system/builtin/SKILL.md"), "codex").unwrap();
        mirror_tree(root, "codex/skills", &[file("c/SKILL.md", "three")], "/r").unwrap();
        assert!(skills.join(".system/builtin/SKILL.md").exists());
        assert!(skills.join("c/SKILL.md").exists());
        assert!(!skills.join("a").exists());

        let target = root.join("env.sh");
        write_if_changed(&target, b"x", 0o600).unwrap();
        let before = std::fs::metadata(&target).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        write_if_changed(&target, b"x", 0o600).unwrap();
        assert_eq!(
            std::fs::metadata(&target).unwrap().modified().unwrap(),
            before
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
