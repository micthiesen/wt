use crate::discovery::{
    absolute, canonical, canonical_repository_config, env, expand_home, merge, path_namespace,
    repository_config, repository_namespace,
};
use crate::error::ConfigError;
use crate::schema::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_ROWS: &[&str] = &["branch", "issue", "stage", "dev", "pr", "claude", "git"];
const DEFAULT_REGEN: &[&str] = &["sst-env.d.ts"];
const BADGE_SLOTS: &[&str] = &[
    "action",
    "dirty",
    "rebase",
    "deploy",
    "session",
    "review_bot",
    "review",
    "pr",
    "checks",
];

#[derive(Clone, Debug)]
pub struct LoadOptions {
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub env: BTreeMap<String, String>,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default(),
            env: std::env::vars().collect(),
        }
    }
}

impl LoadOptions {
    /// User configuration location without requiring a valid repository config.
    pub fn user_config_path(&self) -> PathBuf {
        user_config_path(self)
    }

    pub fn new(
        cwd: impl Into<PathBuf>,
        home: impl Into<PathBuf>,
        env: BTreeMap<String, String>,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            home: home.into(),
            env,
        }
    }
}

impl Config {
    /// Resolve the user config, repository override, merged schema and defaults.
    pub fn load(options: &LoadOptions) -> Result<Self, ConfigError> {
        let config_path = user_config_path(options);
        // A repository config can supply all required paths and identity.
        // In that case the global config is genuinely optional, including
        // from linked worktrees whose ignored .wt.toml is not inherited.
        let user_raw = if config_path.is_file() {
            read_toml(&config_path)?
        } else {
            toml::Value::Table(Default::default())
        };
        let discovered = repository_config(&options.cwd, &options.env);

        // Discovery may have found a worktree copy. Inspect paths from both
        // layers before selecting the owning repository's canonical config.
        let mut preview = user_raw.clone();
        if let Some(path) = discovered.as_ref().filter(|path| path.is_file())
            && let Ok(repo_raw) = read_toml(path)
        {
            merge(&mut preview, &repo_raw);
        }
        let main_clone = table_string(&preview, "paths", "main_clone")
            .map(|p| expand_home(&p, &options.home))
            .unwrap_or_default();
        let worktree_root = table_string(&preview, "paths", "worktree_root")
            .map(|p| expand_home(&p, &options.home))
            .unwrap_or_default();
        let repository =
            canonical_repository_config(discovered, &main_clone, &worktree_root, &options.cwd);

        if repository.is_none() && !config_path.is_file() {
            return Err(ConfigError::NotFound { path: config_path });
        }

        let mut merged = user_raw;
        let mut source_path = config_path;
        if let Some(repository_path) = repository.as_ref() {
            let repo_raw = read_toml(repository_path)?;
            merge(&mut merged, &repo_raw);
            source_path = repository_path.clone();
        }
        Self::from_raw_at(merged, options, repository, source_path)
    }

    /// Build a resolved config from an already parsed, merged TOML value.
    /// Repository discovery still runs so path-derived identity matches `load`.
    pub fn from_raw(raw: toml::Value, options: &LoadOptions) -> Result<Self, ConfigError> {
        let repo = repository_config(&options.cwd, &options.env);
        let label = repo.clone().unwrap_or_else(|| options.cwd.join("<config>"));
        Self::from_raw_at(raw, options, repo, label)
    }

    fn from_raw_at(
        raw: toml::Value,
        options: &LoadOptions,
        repository: Option<PathBuf>,
        source_path: PathBuf,
    ) -> Result<Self, ConfigError> {
        let mut errors = Vec::new();
        let root = raw.as_table();
        if root.is_none() {
            errors.push("configuration root must be a table".to_owned());
        }

        let paths_raw: RawPaths = section(&raw, "paths", &mut errors);
        required(&paths_raw.main_clone, "paths.main_clone", &mut errors);
        required(&paths_raw.worktree_root, "paths.worktree_root", &mut errors);
        let main_clone = expand_home(&paths_raw.main_clone, &options.home);
        let worktree_root = expand_home(&paths_raw.worktree_root, &options.home);
        let repository_dir = repository.as_ref().map(|path| {
            let real = canonical(path, &options.cwd);
            real.parent().unwrap_or(Path::new(".")).to_path_buf()
        });
        let config_is_worktree_copy = repository_dir.as_ref().is_some_and(|dir| {
            !worktree_root.as_os_str().is_empty()
                && crate::discovery::is_inside(&worktree_root, dir, &options.cwd)
        });
        let identify_by_main_clone = repository.is_none() || config_is_worktree_copy;
        let repo_path = if identify_by_main_clone {
            if main_clone.exists() {
                canonical(&main_clone, &options.cwd)
            } else {
                main_clone.clone()
            }
        } else {
            repository_dir
                .clone()
                .unwrap_or_else(|| canonical(&main_clone, &options.cwd))
        };
        let repo_id = if identify_by_main_clone {
            path_namespace(&main_clone, &options.home, &options.cwd)
        } else {
            repository_namespace(
                repository.as_ref().expect("repository exists"),
                &options.home,
                &options.cwd,
            )
        };

        let own_repository_config = !identify_by_main_clone;
        let default_cache_db = if own_repository_config {
            options
                .home
                .join(".cache/wt")
                .join(&repo_id)
                .join("cache.sqlite")
        } else {
            options.home.join(".cache/wt/cache.sqlite")
        };
        let cache_db = path_value(&paths_raw.cache_db, default_cache_db, &options.home);
        let cache_root = path_parent(&cache_db);
        let default_state_db = if own_repository_config {
            options.home.join(".local/state/wt/wt.sqlite")
        } else {
            cache_root.join("wt.sqlite")
        };
        let state_db = path_value(&paths_raw.state_db, default_state_db, &options.home);
        let log_dir = path_value(&paths_raw.log_dir, cache_root.join("logs"), &options.home);
        let lock_dir = path_value(&paths_raw.lock_dir, cache_root.join("locks"), &options.home);
        let wezterm_default = if cfg!(target_os = "macos") {
            Some(PathBuf::from(
                "/Applications/WezTerm.app/Contents/MacOS/wezterm",
            ))
        } else {
            None
        };
        let wezterm_cli = paths_raw
            .wezterm_cli
            .as_deref()
            .and_then(nonempty_path)
            .map(|path| expand_home(path, &options.home))
            .or(wezterm_default);
        let paths = PathsConfig {
            main_clone,
            worktree_root,
            app_log_dir: log_dir.join("app"),
            log_dir,
            lock_dir,
            cache_root,
            cache_db,
            state_db,
            wezterm_cli,
            wt_source: paths_raw
                .wt_source
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(|value| expand_home(value, &options.home)),
            dotfiles: path_value(
                &paths_raw.dotfiles,
                options.home.join(".dotfiles"),
                &options.home,
            ),
        };

        let instance_raw: InstanceRaw = section(&raw, "instance", &mut errors);
        let harness_raw: HarnessRaw = section(&raw, "harness", &mut errors);
        let mut hidden = BTreeSet::new();
        for value in harness_raw.hidden {
            match parse_harness(&value) {
                Some(id) => { hidden.insert(id); }
                None => errors.push(format!("harness.hidden: unknown harness \"{value}\" (expected one of claude, codex, opencode, pi)")),
            }
        }
        if hidden.len() == HarnessId::ALL.len() {
            errors.push("harness.hidden cannot hide every harness".into());
        }
        if hidden.contains(&harness_raw.primary) {
            errors.push(format!(
                "harness.primary {:?} cannot also appear in harness.hidden",
                harness_raw.primary
            ));
        }

        let mut branch: BranchConfig = section(&raw, "branch", &mut errors);
        required(&branch.prefix, "branch.prefix", &mut errors);
        if branch.base.is_empty() {
            branch.base = "main".into();
        }
        if branch.id_pattern.is_empty() {
            branch.id_pattern = r"^[a-z]+-(\d+)(?:-|$)".into();
        }
        if regex::Regex::new(&branch.id_pattern).is_err() {
            errors.push("branch.id_pattern: invalid regex".into());
        }
        branch.keep_fresh.retain(|value| !value.trim().is_empty());
        branch.production = branch
            .production
            .take()
            .filter(|value| !value.trim().is_empty());
        if let Some(production) = branch.production.as_ref()
            && production != &branch.base
            && !branch.keep_fresh.contains(production)
        {
            errors.push(format!(
                "branch.production \"{production}\" must be [branch] base or in [branch] keep_fresh"
            ));
        }

        let stage_raw: RawStage = section(&raw, "stage", &mut errors);
        let stage = StageConfig {
            prefix: nonempty(stage_raw.prefix).unwrap_or_else(|| format!("{}-", branch.prefix)),
            default_personal: nonempty(stage_raw.default_personal)
                .unwrap_or_else(|| branch.prefix.clone()),
            domain: stage_raw.domain.filter(|value| !value.is_empty()),
        };

        let mut lifecycle: LifecycleConfig = section(&raw, "lifecycle", &mut errors);
        lifecycle.install_command = lifecycle.install_command.filter(|value| !value.is_empty());
        lifecycle.destroy_command = lifecycle.destroy_command.filter(|value| !value.is_empty());
        if lifecycle.env_files_to_copy.is_empty()
            && !field_present(&raw, "lifecycle", "env_files_to_copy")
        {
            lifecycle.env_files_to_copy = vec![".env".into()];
        }
        if lifecycle
            .copy_globs
            .iter()
            .any(|value| unsafe_copy_glob(value))
        {
            errors.push(
                "lifecycle.copy_globs entries must be relative paths without '..' segments".into(),
            );
        }
        let backend_raw: BackendRaw = section(&raw, "backend", &mut errors);

        let sst = optional_section::<RawSst>(&raw, "deploy", "sst", &mut errors).map(|raw_sst| {
            required(
                &raw_sst.state_bucket,
                "deploy.sst.state_bucket",
                &mut errors,
            );
            required(
                &raw_sst.state_prefix,
                "deploy.sst.state_prefix",
                &mut errors,
            );
            required(&raw_sst.aws_profile, "deploy.sst.aws_profile", &mut errors);
            let mut paths = raw_sst.auto_regen_paths;
            if paths.is_empty() && !field_present_nested(&raw, "deploy", "sst", "auto_regen_paths")
            {
                paths = DEFAULT_REGEN.iter().map(|s| (*s).to_owned()).collect();
            }
            SstConfig {
                state_bucket: raw_sst.state_bucket,
                state_prefix: raw_sst.state_prefix,
                aws_profile: raw_sst.aws_profile,
                auto_regen_paths: paths,
            }
        });

        let issue_tracker_raw =
            optional_section::<RawIssueTracker>(&raw, "issue_tracker", "", &mut errors);
        let issue_tracker = issue_tracker_raw.map(|tracker| {
            let linear = tracker.linear;
            let mut tracker = IssueTrackerConfig {
                status_styles: tracker.status_styles,
                status_command: tracker.status_command,
                read_command: tracker.read_command,
                url_template: tracker.url_template,
                prefix: tracker.prefix,
            };
            if tracker.status_command.as_ref().is_some_and(Vec::is_empty) { tracker.status_command = None; }
            if tracker.read_command.as_ref().is_some_and(Vec::is_empty) { tracker.read_command = None; }
            tracker.url_template = tracker.url_template.filter(|value| !value.is_empty());
            tracker.prefix = tracker.prefix.filter(|value| !value.is_empty());
            if tracker.url_template.is_none()
                && let Some(linear) = linear
            {
                required(&linear.workspace, "issue_tracker.linear.workspace", &mut errors);
                tracker.url_template = Some(format!("linear://{}/issue/{{id}}", linear.workspace));
            }
            if let Some(template) = tracker.url_template.as_ref()
                && !template.contains("{id}")
            {
                errors.push("issue_tracker.url_template must contain the {id} placeholder".into());
            }
            if tracker.prefix.as_ref().is_some_and(|value| !regex::Regex::new(r"^[a-z]+$").unwrap().is_match(value)) {
                errors.push("issue_tracker.prefix must be lowercase letters (e.g. \"coz\")".into());
            }
            if let Some(command) = tracker.read_command.as_mut() {
                if command.iter().any(|arg| arg.trim().is_empty() || arg.contains('\0')) {
                    errors.push("issue_tracker.read_command must be an array of nonempty strings (argv, not a shell command)".into());
                } else if !command.is_empty() {
                    if command[0].contains("{id}") || !command.iter().skip(1).any(|arg| arg.contains("{id}")) {
                        errors.push("issue_tracker.read_command must contain {id} in an argument, not the executable".into());
                    }
                    command[0] = expand_home(&command[0], &options.home).to_string_lossy().into_owned();
                }
            }
            if let Some(command) = tracker.status_command.as_mut() {
                if command.iter().any(|arg| arg.trim().is_empty() || arg.contains('\0')) {
                    errors.push("issue_tracker.status_command must be an array of nonempty strings (argv, not a shell command)".into());
                } else if !command.is_empty() {
                    let exact = command.iter().filter(|arg| arg.as_str() == "{ids}").count();
                    if command[0].contains("{ids}") || exact != 1 || command.iter().any(|arg| arg.contains("{ids}") && arg != "{ids}") {
                        errors.push("issue_tracker.status_command must contain exactly one standalone {ids} argument, not the executable".into());
                    }
                    command[0] = expand_home(&command[0], &options.home).to_string_lossy().into_owned();
                }
            }
            let color_pattern = regex::Regex::new(r"^#[0-9a-fA-F]{6}$").unwrap();
            for (label, style) in &tracker.status_styles {
                if label.trim().is_empty() || label.chars().any(|ch| ch.is_control()) || !color_pattern.is_match(&style.color) {
                    errors.push(format!("issue_tracker.status_styles.{label} needs icon (circle|backlog|progress|review|completed|cancelled|blocked) and color (#RRGGBB)"));
                }
            }
            tracker
        });
        let review_bot_raw: RawReviewBot = section(&raw, "review_bot", &mut errors);
        let summary_marker = match review_bot_raw.summary_marker.as_ref() {
            None => Vec::new(),
            Some(toml::Value::String(value)) => {
                if value.is_empty() {
                    Vec::new()
                } else {
                    vec![value.clone()]
                }
            }
            Some(toml::Value::Array(values)) => values
                .iter()
                .filter_map(toml::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
            Some(_) => {
                errors
                    .push("review_bot.summary_marker must be a string or array of strings".into());
                Vec::new()
            }
        };
        let mut review_bot = ReviewBotConfig {
            name: review_bot_raw.name,
            login: review_bot_raw.login,
            check_contexts: review_bot_raw.check_contexts,
            unresolved_via: review_bot_raw.unresolved_via,
            summary_marker,
            pending_marker: review_bot_raw.pending_marker,
            rerun_command: review_bot_raw.rerun_command,
        };
        if review_bot.unresolved_via == ReviewBotMode::Checklist {
            if !field_present(&raw, "review_bot", "login") {
                review_bot.login.clear();
            }
            if !field_present(&raw, "review_bot", "check_contexts") {
                review_bot.check_contexts.clear();
            }
        }
        if review_bot.name.is_empty() {
            review_bot.name = "CodeRabbit".into();
        }
        if review_bot.login.is_empty() && review_bot.unresolved_via == ReviewBotMode::Threads {
            review_bot.login = "coderabbitai".into();
        }
        if review_bot.check_contexts.is_empty()
            && review_bot.unresolved_via == ReviewBotMode::Threads
            && !field_present(&raw, "review_bot", "check_contexts")
        {
            review_bot.check_contexts = vec!["CodeRabbit".into()];
        }
        if review_bot.unresolved_via == ReviewBotMode::Checklist {
            required(&review_bot.login, "review_bot.login", &mut errors);
            if review_bot.summary_marker.is_empty() {
                errors.push(
                    "review_bot.summary_marker is required when unresolved_via = \"checklist\""
                        .into(),
                );
            }
        }
        if review_bot.login.ends_with("[bot]") {
            review_bot.login.truncate(review_bot.login.len() - 5);
        }

        let dev_server = optional_section::<DevServerConfig>(&raw, "dev_server", "", &mut errors)
            .map(|mut dev| {
                if dev.url.is_empty() {
                    dev.url = "http://localhost:{{port}}/".into();
                }
                dev.stop_command = dev.stop_command.take().filter(|value| !value.is_empty());
                dev.reset_command = dev.reset_command.take().filter(|value| !value.is_empty());
                dev.health_command = dev.health_command.take().filter(|value| !value.is_empty());
                required(&dev.command, "dev_server.command", &mut errors);
                if dev.port_base == 0 {
                    errors.push("dev_server.port_base must be a port number (1-65535)".into());
                }
                if dev.port_range == 0 || u32::from(dev.port_base) + dev.port_range > 65_536 {
                    errors.push("dev_server.port_range must keep the range within 1-65535".into());
                }
                if dev.max_concurrent == Some(0) {
                    errors.push(
                        "dev_server.max_concurrent must be a positive integer (omit it for no cap)"
                            .into(),
                    );
                }
                dev
            });

        let remote = optional_section::<RawRemote>(&raw, "remote", "", &mut errors).map(|r| {
            required(&r.host, "remote.host", &mut errors);
            RemoteConfig {
                label: nonempty(r.label).unwrap_or_else(|| r.host.clone()),
                wt_path: nonempty(r.wt_path).unwrap_or_else(|| "~/.wt/bin/wt".into()),
                host: r.host,
            }
        });

        let naming = optional_section::<NamingConfig>(&raw, "naming", "", &mut errors);
        if raw.get("ai").is_some() {
            errors.push(
                "[ai] is no longer supported; use [naming] with a coding-agent harness".into(),
            );
        }

        let mut diff: DiffConfig = section(&raw, "diff", &mut errors);
        if diff.command.is_empty() {
            diff.command = "revdiff --vim-motion --compact {{base}}".into();
        }
        let mut editor: EditorConfig = section(&raw, "editor", &mut errors);
        editor.command = editor.command.filter(|value| !value.is_empty());
        let mut browser: BrowserConfig = section(&raw, "browser", &mut errors);
        browser.chrome_profile = browser.chrome_profile.filter(|value| !value.is_empty());
        let mut tmux: TmuxConfig = section(&raw, "tmux", &mut errors);
        if let Some(socket) =
            env(&options.env, "WT_TMUX_SOCKET").filter(|value| !value.trim().is_empty())
        {
            tmux.socket = socket.trim().to_owned();
        } else if own_repository_config
            && (!field_present(&raw, "tmux", "socket") || tmux.socket.trim().is_empty())
        {
            tmux.socket = format!("wt-{repo_id}");
        } else if tmux.socket.trim().is_empty() {
            tmux.socket = "wt".into();
        }

        let mut ui: UiConfig = section(&raw, "ui", &mut errors);
        if ui.rows.is_empty() && !field_present(&raw, "ui", "rows") {
            ui.rows = DEFAULT_ROWS.iter().map(|s| (*s).to_owned()).collect();
        }
        for badge in &ui.hidden_badges {
            if !BADGE_SLOTS.contains(&badge.as_str()) {
                errors.push(format!(
                    "ui.hidden_badges: unknown badge \"{badge}\" (expected one of {})",
                    BADGE_SLOTS.join(", ")
                ));
            }
        }
        let mut github: GithubConfig = section(&raw, "github", &mut errors);
        github.default_reviewer = github
            .default_reviewer
            .take()
            .filter(|value| !value.is_empty());
        if let Some(events) = github.events.as_mut() {
            if events.port == 0 {
                errors.push("github.events.port must be a port number (1-65535)".into());
            }
            if !events.backstop_poll_ms.is_finite() || events.backstop_poll_ms <= 0.0 {
                errors.push("github.events.backstop_poll_ms must be a positive number".into());
            }
            events.secret_file = events
                .secret_file
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(|value| {
                    expand_home(value, &options.home)
                        .to_string_lossy()
                        .into_owned()
                });
            events.secret = events.secret.take().filter(|value| !value.is_empty());
        }

        let actions = parse_actions(&raw, &mut errors);
        let automations = parse_automations(&raw, &actions, &branch, &mut errors);
        let skills_raw: BoolSection = section(&raw, "skills", &mut errors);
        let manager_raw: ManagerSection = section(&raw, "manager", &mut errors);
        let update_raw: BoolSection = section(&raw, "update", &mut errors);

        if !errors.is_empty() {
            return Err(ConfigError::invalid(source_path, errors));
        }
        Ok(Config {
            repository_config: repository,
            instance: InstanceConfig {
                role: instance_raw.role,
            },
            repo_id,
            repo_path,
            paths,
            tmux,
            branch,
            stage,
            lifecycle,
            backend: BackendConfig {
                kind: backend_raw.kind,
            },
            sst,
            issue_tracker,
            review_bot,
            dev_server,
            remote,
            harness: HarnessConfig {
                primary: harness_raw.primary,
                hidden,
            },
            naming,
            diff,
            editor,
            browser,
            github,
            actions,
            automations,
            ui,
            skills: SkillsConfig {
                startup_check: skills_raw.startup_check,
            },
            manager: ManagerConfig {
                wt_feedback: manager_raw.wt_feedback,
            },
            update: UpdateConfig {
                startup_check: update_raw.startup_check,
            },
        })
    }
}

fn user_config_path(options: &LoadOptions) -> PathBuf {
    if let Some(path) = env(&options.env, "WT_CONFIG").filter(|path| !path.is_empty()) {
        return absolute(&expand_home(path, &options.home), &options.cwd);
    }
    let xdg = env(&options.env, "XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(|p| absolute(&expand_home(p, &options.home), &options.cwd))
        .unwrap_or_else(|| options.home.join(".config"));
    xdg.join("wt/config.toml")
}

fn read_toml(path: &Path) -> Result<toml::Value, ConfigError> {
    let text = fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ConfigError::NotFound {
                path: path.to_path_buf(),
            }
        } else {
            ConfigError::Read {
                path: path.to_path_buf(),
                source: error,
            }
        }
    })?;
    toml::from_str(&text).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

fn section<T: DeserializeOwned + Default>(
    raw: &toml::Value,
    name: &str,
    errors: &mut Vec<String>,
) -> T {
    match raw.as_table().and_then(|table| table.get(name)) {
        None => T::default(),
        Some(value) if value.is_table() => match value.clone().try_into() {
            Ok(parsed) => parsed,
            Err(error) => {
                errors.push(format!("{name}: {error}"));
                T::default()
            }
        },
        Some(_) => {
            errors.push(format!("{name} must be a table"));
            T::default()
        }
    }
}

fn optional_section<T: DeserializeOwned>(
    raw: &toml::Value,
    parent: &str,
    child: &str,
    errors: &mut Vec<String>,
) -> Option<T> {
    let table = raw.as_table()?;
    let value = table.get(parent)?;
    let value = if child.is_empty() {
        value
    } else {
        let Some(nested) = value.as_table() else {
            errors.push(format!("{parent} must be a table"));
            return None;
        };
        nested.get(child)?
    };
    if !value.is_table() {
        errors.push(format!(
            "{parent}{dot}{child} must be a table",
            dot = if child.is_empty() { "" } else { "." }
        ));
        return None;
    }
    match value.clone().try_into() {
        Ok(parsed) => Some(parsed),
        Err(error) => {
            errors.push(format!(
                "{parent}{dot}{child}: {error}",
                dot = if child.is_empty() { "" } else { "." }
            ));
            None
        }
    }
}

fn field_present(raw: &toml::Value, section: &str, field: &str) -> bool {
    raw.get(section)
        .and_then(toml::Value::as_table)
        .is_some_and(|table| table.contains_key(field))
}
fn field_present_nested(raw: &toml::Value, a: &str, b: &str, field: &str) -> bool {
    raw.get(a)
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get(b))
        .and_then(toml::Value::as_table)
        .is_some_and(|table| table.contains_key(field))
}
fn table_string(raw: &toml::Value, section: &str, field: &str) -> Option<String> {
    raw.get(section)?.get(field)?.as_str().map(str::to_owned)
}
fn required(value: &str, label: &str, errors: &mut Vec<String>) {
    if value.is_empty() {
        errors.push(format!("{label} is required"));
    }
}
fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}
fn nonempty_path(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}
fn path_value(value: &Option<String>, default: PathBuf, home: &Path) -> PathBuf {
    value
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| expand_home(value, home))
        .unwrap_or(default)
}
fn path_parent(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf()
}
fn parse_harness(value: &str) -> Option<HarnessId> {
    match value {
        "claude" => Some(HarnessId::Claude),
        "codex" => Some(HarnessId::Codex),
        "opencode" => Some(HarnessId::Opencode),
        _ => None,
    }
}
fn unsafe_copy_glob(value: &str) -> bool {
    value.starts_with('/')
        || value.split(['/', '\\']).any(|component| component == "..")
        || (value.as_bytes().get(1) == Some(&b':')
            && value
                .as_bytes()
                .get(2)
                .is_some_and(|b| *b == b'/' || *b == b'\\'))
}

fn parse_actions(raw: &toml::Value, errors: &mut Vec<String>) -> Vec<ActionDef> {
    let Some(value) = raw.get("actions") else {
        return default_actions();
    };
    let Some(entries) = value.as_array() else {
        errors.push("actions must be a TOML array of [[actions]] tables".into());
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut ids = BTreeSet::new();
    let key_re = regex::Regex::new(r"^[a-z0-9]$").expect("literal regex");
    for (index, value) in entries.iter().enumerate() {
        let tag = format!("actions[{index}]");
        if !value.is_table() {
            errors.push(format!("{tag} must be a table"));
            continue;
        }
        let parsed: RawAction = match value.clone().try_into() {
            Ok(action) => action,
            Err(error) => {
                errors.push(format!("{tag}: {error}"));
                continue;
            }
        };
        let has_prompt = parsed
            .prompt
            .as_ref()
            .is_some_and(|value| !value.is_empty());
        let has_shell = parsed.shell.as_ref().is_some_and(|value| !value.is_empty());
        if parsed.id.is_empty() {
            errors.push(format!("{tag}.id is required"));
        }
        if parsed.name.is_empty() {
            errors.push(format!("{tag}.name is required"));
        }
        if parsed.id.is_empty() || parsed.name.is_empty() {
            continue;
        }
        if !ids.insert(parsed.id.clone()) {
            errors.push(format!("{tag}.id \"{}\" is duplicated", parsed.id));
            continue;
        }
        if parsed.prompt.is_some() && !has_prompt {
            errors.push(format!("{tag}.prompt must be a non-empty string"));
            continue;
        }
        if parsed.shell.is_some() && !has_shell {
            errors.push(format!("{tag}.shell must be a non-empty string"));
            continue;
        }
        if has_prompt == has_shell {
            errors.push(if has_prompt {
                format!("{tag} must set exactly one of \"prompt\" or \"shell\", not both")
            } else {
                format!("{tag} must set one of \"prompt\" or \"shell\"")
            });
            continue;
        }
        if parsed.key.as_ref().is_some_and(|key| !key_re.is_match(key)) {
            errors.push(format!(
                "{tag}.key must be a single lowercase letter or digit (a-z, 0-9)"
            ));
            continue;
        }
        if parsed.group.as_ref().is_some_and(|group| group.is_empty()) {
            errors.push(format!("{tag}.group must be a non-empty string"));
            continue;
        }
        if parsed.issue_status.as_ref().is_some_and(|status| {
            !has_shell || status.trim().is_empty() || status.chars().any(|ch| ch.is_control())
        }) {
            errors.push(format!(
                "{tag}.issue_status must be a nonempty single-line string on a shell action"
            ));
            continue;
        }
        if parsed.issue_status.is_some()
            && !parsed
                .affects
                .as_ref()
                .is_some_and(|tags| tags.contains(&EffectTag::Issue))
        {
            errors.push(format!("{tag}.issue_status requires affects = [\"issue\"]"));
            continue;
        }
        if !has_prompt && value.get("target").is_some() {
            errors.push(format!(
                "{tag}.target is only valid on a claude (prompt) action"
            ));
            continue;
        }
        if parsed
            .label_extract
            .as_ref()
            .is_some_and(|pattern| regex::Regex::new(pattern).is_err())
        {
            errors.push(format!("{tag}.label_extract: invalid regex"));
            continue;
        }
        let mut action = ActionDef {
            kind: if has_prompt {
                ActionKind::Claude
            } else {
                ActionKind::Shell
            },
            id: parsed.id,
            name: parsed.name,
            prompt: parsed.prompt,
            shell: parsed.shell,
            target: parsed.target.unwrap_or_default(),
            affects: parsed.affects,
            requires: parsed.requires.unwrap_or_default(),
            issue_status: parsed.issue_status,
            key: parsed.key,
            group: parsed.group,
            external: parsed.external.unwrap_or(false),
            arg_prompt: parsed
                .arg_prompt
                .filter(|value| !value.is_empty())
                .map(|label| ActionArgPrompt { label }),
            label_extract: parsed.label_extract.filter(|value| !value.is_empty()),
        };
        if action.affects.is_none() {
            action.affects = Some(if has_prompt {
                vec![EffectTag::Git, EffectTag::Github]
            } else {
                Vec::new()
            });
        }
        let mut effects = BTreeSet::new();
        action
            .affects
            .as_mut()
            .unwrap()
            .retain(|tag| effects.insert(*tag));
        let mut requirements = BTreeSet::new();
        action.requires.retain(|tag| requirements.insert(*tag));
        result.push(action);
    }
    result
}

fn default_actions() -> Vec<ActionDef> {
    vec![
        ActionDef { id: "rebase-main".into(), name: "Rebase on base".into(),
            prompt: Some("Please rebase this branch on origin/{{base_branch}} and intelligently resolve any conflicts that come up. Push when you're done.".into()),
            affects: Some(vec![EffectTag::Git, EffectTag::Github]), ..ActionDef::default() },
        ActionDef { id: "address-review".into(), name: "Address PR review".into(),
            prompt: Some("Check the requested changes from the review on the PR for this branch and address them. Push the changes, then resolve the review threads (no reply comments). When done, request a re-review from the original reviewers.".into()),
            target: ActionTarget::Session, affects: Some(vec![EffectTag::Git, EffectTag::Github]),
            requires: vec![RequireTag::PrReady], ..ActionDef::default() },
    ]
}

fn parse_automations(
    raw: &toml::Value,
    actions: &[ActionDef],
    branch: &BranchConfig,
    errors: &mut Vec<String>,
) -> Vec<AutomationDef> {
    let Some(value) = raw.get("automations") else {
        return Vec::new();
    };
    let Some(entries) = value.as_array() else {
        errors.push("automations must be a TOML array of [[automations]] tables".into());
        return Vec::new();
    };
    let action_ids: BTreeSet<&str> = actions.iter().map(|action| action.id.as_str()).collect();
    let builtins = [
        "builtin:restack",
        "builtin:clean",
        "builtin:notify",
        "builtin:close-issue",
        "builtin:delete-branch",
    ];
    let mut result: Vec<AutomationDef> = Vec::new();
    let mut ids = BTreeSet::new();
    for (index, value) in entries.iter().enumerate() {
        let tag = format!("automations[{index}]");
        if !value.is_table() {
            errors.push(format!("{tag} must be a table"));
            continue;
        }
        let input: RawAutomation = match value.clone().try_into() {
            Ok(input) => input,
            Err(error) => {
                errors.push(format!("{tag}: {error}"));
                continue;
            }
        };
        if input.id.is_empty() {
            errors.push(format!("{tag}.id is required"));
            continue;
        }
        if !ids.insert(input.id.clone()) {
            errors.push(format!("{tag}.id \"{}\" is duplicated", input.id));
            continue;
        }
        let Some(on) = parse_trigger(&input.on) else {
            errors.push(format!("{tag}.on must be one of: pr.checks.failed, review_bot.unresolved, review.changes_requested, pr.conflict, wt.merged, wt.created, stack.parent_merged, status.needs_human, status.needs_testing, status.ready, status.verification_overdue, branch.advanced"));
            continue;
        };
        if input.run.is_empty() {
            errors.push(format!("{tag}.run is required"));
            continue;
        }
        if !action_ids.contains(input.run.as_str()) && !builtins.contains(&input.run.as_str()) {
            errors.push(format!(
                "{tag}.run \"{}\" is neither an [[actions]] id nor one of: {}",
                input.run,
                builtins.join(", ")
            ));
            continue;
        }
        if input.run == "builtin:restack" && on != AutomationTrigger::StackParentMerged {
            errors.push(format!(
                "{tag}: run \"builtin:restack\" requires on = \"stack.parent_merged\""
            ));
            continue;
        }
        if input.run == "builtin:clean" && on == AutomationTrigger::StackParentMerged {
            errors.push(format!("{tag}: run \"builtin:clean\" targets one worktree; use \"builtin:restack\" for stack.parent_merged (it cleans merged members first)"));
            continue;
        }
        if input.run == "builtin:close-issue" {
            if on != AutomationTrigger::WtMerged {
                errors.push(format!(
                    "{tag}: run \"builtin:close-issue\" requires on = \"wt.merged\""
                ));
                continue;
            }
            if result.iter().any(|a| a.run == input.run) {
                errors.push(format!(
                    "{tag}: only one builtin:close-issue rule is allowed"
                ));
                continue;
            }
        }
        if input.run == "builtin:delete-branch" {
            if on != AutomationTrigger::WtMerged {
                errors.push(format!(
                    "{tag}: run \"builtin:delete-branch\" requires on = \"wt.merged\""
                ));
                continue;
            }
            if result.iter().any(|a| a.run == input.run) {
                errors.push(format!(
                    "{tag}: only one builtin:delete-branch rule is allowed"
                ));
                continue;
            }
        }
        if input
            .cooldown_minutes
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            errors.push(format!("{tag}.cooldown_minutes must be a positive number"));
            continue;
        }
        if input
            .after_days
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            errors.push(format!("{tag}.after_days must be a non-negative number"));
            continue;
        }
        if input.after_days.is_some() && on != AutomationTrigger::StatusVerificationOverdue {
            errors.push(format!("{tag}.after_days only applies to on = \"status.verification_overdue\" (got \"{}\")", trigger_name(on)));
            continue;
        }
        if input
            .settle_seconds
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            errors.push(format!(
                "{tag}.settle_seconds must be a non-negative number"
            ));
            continue;
        }
        let branch_name = match input.branch {
            Some(value) if value.trim().is_empty() => {
                errors.push(format!("{tag}.branch must be a non-empty string"));
                continue;
            }
            Some(_) if on != AutomationTrigger::BranchAdvanced => {
                errors.push(format!(
                    "{tag}.branch only applies to on = \"branch.advanced\" (got \"{}\")",
                    trigger_name(on)
                ));
                continue;
            }
            Some(value) => Some(value.trim().to_owned()),
            None if on == AutomationTrigger::BranchAdvanced => {
                errors.push(format!(
                    "{tag}: on = \"branch.advanced\" requires branch = \"<name>\""
                ));
                continue;
            }
            None => None,
        };
        if let Some(name) = branch_name.as_ref()
            && name != &branch.base
            && !branch.keep_fresh.contains(name)
        {
            errors.push(format!("{tag}.branch \"{name}\" is neither [branch] base nor in [branch] keep_fresh — nothing would advance it, so the rule could never fire"));
            continue;
        }
        result.push(AutomationDef {
            id: input.id,
            on,
            run: input.run,
            busy: input.busy,
            cooldown_minutes: input.cooldown_minutes,
            after_days: input.after_days.unwrap_or(2.0),
            settle_seconds: input.settle_seconds.unwrap_or_else(|| default_settle(on)),
            branch: branch_name,
        });
    }
    result
}

fn parse_trigger(value: &str) -> Option<AutomationTrigger> {
    let value = match value {
        "rabbit.unresolved" => "review_bot.unresolved",
        "status.needs-human" => "status.needs_human",
        "status.needs-testing" => "status.needs_testing",
        other => other,
    };
    Some(match value {
        "pr.checks.failed" => AutomationTrigger::PrChecksFailed,
        "review_bot.unresolved" => AutomationTrigger::ReviewBotUnresolved,
        "review.changes_requested" => AutomationTrigger::ReviewChangesRequested,
        "pr.conflict" => AutomationTrigger::PrConflict,
        "wt.merged" => AutomationTrigger::WtMerged,
        "wt.created" => AutomationTrigger::WtCreated,
        "stack.parent_merged" => AutomationTrigger::StackParentMerged,
        "status.needs_human" => AutomationTrigger::StatusNeedsHuman,
        "status.needs_testing" => AutomationTrigger::StatusNeedsTesting,
        "status.ready" => AutomationTrigger::StatusReady,
        "status.verification_overdue" => AutomationTrigger::StatusVerificationOverdue,
        "branch.advanced" => AutomationTrigger::BranchAdvanced,
        _ => return None,
    })
}
fn trigger_name(value: AutomationTrigger) -> &'static str {
    match value {
        AutomationTrigger::PrChecksFailed => "pr.checks.failed",
        AutomationTrigger::ReviewBotUnresolved => "review_bot.unresolved",
        AutomationTrigger::ReviewChangesRequested => "review.changes_requested",
        AutomationTrigger::PrConflict => "pr.conflict",
        AutomationTrigger::WtMerged => "wt.merged",
        AutomationTrigger::WtCreated => "wt.created",
        AutomationTrigger::StackParentMerged => "stack.parent_merged",
        AutomationTrigger::StatusNeedsHuman => "status.needs_human",
        AutomationTrigger::StatusNeedsTesting => "status.needs_testing",
        AutomationTrigger::StatusReady => "status.ready",
        AutomationTrigger::StatusVerificationOverdue => "status.verification_overdue",
        AutomationTrigger::BranchAdvanced => "branch.advanced",
    }
}
fn default_settle(on: AutomationTrigger) -> f64 {
    match on {
        AutomationTrigger::WtCreated
        | AutomationTrigger::StatusNeedsHuman
        | AutomationTrigger::StatusNeedsTesting
        | AutomationTrigger::StatusReady
        | AutomationTrigger::StatusVerificationOverdue => 0.0,
        AutomationTrigger::WtMerged
        | AutomationTrigger::StackParentMerged
        | AutomationTrigger::BranchAdvanced => 10.0,
        _ => 120.0,
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawPaths {
    main_clone: String,
    worktree_root: String,
    log_dir: Option<String>,
    lock_dir: Option<String>,
    cache_db: Option<String>,
    state_db: Option<String>,
    wezterm_cli: Option<String>,
    dotfiles: Option<String>,
    wt_source: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct InstanceRaw {
    role: InstanceRole,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct HarnessRaw {
    primary: HarnessId,
    hidden: Vec<String>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawStage {
    prefix: String,
    default_personal: String,
    domain: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct BackendRaw {
    kind: BackendKind,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawSst {
    state_bucket: String,
    state_prefix: String,
    aws_profile: String,
    auto_regen_paths: Vec<String>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawRemote {
    host: String,
    label: String,
    wt_path: String,
}
#[derive(Deserialize)]
#[serde(default)]
struct RawReviewBot {
    name: String,
    login: String,
    check_contexts: Vec<String>,
    unresolved_via: ReviewBotMode,
    summary_marker: Option<toml::Value>,
    pending_marker: Option<String>,
    rerun_command: Option<String>,
}
impl Default for RawReviewBot {
    fn default() -> Self {
        Self {
            name: "CodeRabbit".into(),
            login: "coderabbitai".into(),
            check_contexts: vec!["CodeRabbit".into()],
            unresolved_via: ReviewBotMode::Threads,
            summary_marker: None,
            pending_marker: None,
            rerun_command: None,
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawIssueTracker {
    status_styles: BTreeMap<String, IssueStatusStyle>,
    status_command: Option<Vec<String>>,
    read_command: Option<Vec<String>>,
    url_template: Option<String>,
    prefix: Option<String>,
    linear: Option<RawLinear>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawLinear {
    workspace: String,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct BoolSection {
    startup_check: bool,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct ManagerSection {
    wt_feedback: bool,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawAction {
    id: String,
    name: String,
    prompt: Option<String>,
    shell: Option<String>,
    target: Option<ActionTarget>,
    affects: Option<Vec<EffectTag>>,
    requires: Option<Vec<RequireTag>>,
    issue_status: Option<String>,
    key: Option<String>,
    group: Option<String>,
    external: Option<bool>,
    arg_prompt: Option<String>,
    label_extract: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct RawAutomation {
    id: String,
    on: String,
    run: String,
    busy: AutomationBusyPolicy,
    cooldown_minutes: Option<f64>,
    after_days: Option<f64>,
    settle_seconds: Option<f64>,
    branch: Option<String>,
}
