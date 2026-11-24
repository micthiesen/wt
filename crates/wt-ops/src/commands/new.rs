use std::io::IsTerminal;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Args;
use regex::Regex;
use wt_lifecycle::{CreateOptions, LifecycleService, ServiceConfig};
use wt_store::TitleSource;
use wt_tui::CreationTitle;

use crate::commands::resolve::run_git;
use wt_app_core::context::AppContext;

const USAGE: &str = "usage: wt new <id [title…]|url|branch|slug> [--slug s] [--gh n] [--attach] [--any] [--base ref] [--open|--no-open] [--no-install] [--section name] [--brief file|-] [-m note] [--start] [--wait-install]";

/// Action id of the dependency install a creation leaves running. The board
/// shows a row as installing while a run with this id is live.
pub const CREATE_INSTALL_ACTION_ID: &str = "builtin:create-install";

/// When a new worktree's dependencies are installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstallTiming {
    /// Before creation returns, so the caller finds them present.
    #[default]
    Inline,
    /// Left to the caller, which owes `Created::deferred_install` to
    /// `start_install`.
    Deferred,
}

#[derive(Debug, Clone, Args, Default)]
pub struct NewArgs {
    /// `<ID> [title words…]`, a tracker URL, an existing branch, or a slug.
    /// Pass the bare id, not a URL followed by words.
    #[arg(value_name = "INPUT", num_args = 0..)]
    pub positionals: Vec<String>,
    /// Explicit slug for an issue id (instead of inline title words).
    #[arg(long, value_name = "SLUG")]
    pub slug: Option<String>,
    /// Attach GitHub issue #N as the worktree's secondary id.
    #[arg(long, value_name = "N")]
    pub gh: Option<u64>,
    /// With --attach, match branches by any author.
    #[arg(long, requires = "attach")]
    pub any: bool,
    /// Check out the id's existing branch instead of minting a new one.
    #[arg(long)]
    pub attach: bool,
    /// Stack on this branch or ref instead of the trunk (recorded as the fork base).
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,
    /// Open in the configured editor after creation.
    #[arg(long, conflicts_with = "no_open")]
    pub open: bool,
    /// Do not open in the editor.
    #[arg(long, conflicts_with = "open")]
    pub no_open: bool,
    /// Skip the package-install step.
    #[arg(long)]
    pub no_install: bool,
}

/// A worktree this invocation created (not one that already existed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub slug: String,
    pub path: String,
    /// The dependency install this creation skipped and its caller owes.
    pub deferred_install: Option<wt_core::WorktreeTarget>,
}

pub async fn run(ctx: &AppContext, args: &NewArgs) -> Result<i32> {
    Ok(run_created(ctx, args, InstallTiming::Inline).await?.0)
}

/// `run`, also naming the worktree when this call created it, so a caller can
/// keep acting on it.
pub async fn run_created(
    ctx: &AppContext,
    args: &NewArgs,
    install: InstallTiming,
) -> Result<(i32, Option<Created>)> {
    if args.positionals.is_empty() {
        eprintln!("{USAGE}");
        return Ok((2, None));
    }
    if args.any && !args.attach {
        eprintln!("--any only applies with --attach");
        return Ok((2, None));
    }
    if args.gh == Some(0) {
        eprintln!("--gh requires a positive issue number");
        return Ok((2, None));
    }
    let raw = args.positionals.join(" ").trim().to_owned();
    let Parsed { branch, title } = match parse_branch(ctx, &raw, args, true).await {
        Ok(parsed) => parsed,
        Err(message) => {
            log_failure(&raw, &message);
            eprintln!("{message}");
            return Ok((1, None));
        }
    };
    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    if let Some(existing) = rows
        .iter()
        .find(|row| !row.is_main && row.target.branch == branch)
    {
        println!(
            "Worktree already exists for {branch}\n  path: {}",
            existing.target.path
        );
        if let Some(number) = args.gh {
            set_github_issue(ctx, existing.target.slug(), number).await?;
            println!("  gh:   #{number}");
        }
        if should_open(args) {
            open_editor(ctx, Path::new(&existing.target.path)).await?;
        }
        return Ok((0, None));
    }
    let service = LifecycleService::new(
        ServiceConfig::from_config(&ctx.config),
        (*ctx.repository).clone(),
        ctx.processes.clone(),
    );
    let lookup = IssueTitleLookup::start(ctx, &title);
    let created = match service
        .create(
            &branch,
            CreateOptions {
                base: args.base.clone(),
                fetch_origin: true,
                run_install: !args.no_install && install == InstallTiming::Inline,
            },
            &ctx.cancellation,
        )
        .await
    {
        Ok(created) => created,
        Err(error) => {
            log_failure(&branch, &error.to_string());
            eprintln!("{error}");
            return Ok((1, None));
        }
    };
    // A step failing from here on must not cost a deferred install its
    // start: the caller only learns of it through the returned `Created`.
    let deferred = install == InstallTiming::Deferred;
    if let Some(number) = args.gh {
        match set_github_issue(ctx, created.target.slug(), number).await {
            Err(error) if deferred => eprintln!("warning: could not record #{number}: {error:#}"),
            result => result?,
        }
    }
    // The CLI is the agents' path; people title worktrees in the TUI.
    let issue_title = lookup.finish().await;
    record_title(
        ctx,
        created.target.slug(),
        title,
        TitleSource::Agent,
        issue_title,
    )
    .await;
    println!(
        "✓ created {}\n  path: {}",
        created.target.slug(),
        created.target.path
    );
    if let Some(number) = args.gh {
        println!("  gh:   #{number}");
    }
    if ctx.config.sst.is_some() {
        println!("  stage: {}", created.target.stage);
    }
    if should_open(args) {
        match open_editor(ctx, Path::new(&created.target.path)).await {
            Err(error) if deferred => eprintln!("warning: could not open the editor: {error:#}"),
            result => result?,
        }
    }
    // A rift clone brings its packages with it; the inline path skips it too.
    let deferred_install = (install == InstallTiming::Deferred
        && !args.no_install
        && !tokio::fs::try_exists(Path::new(&created.target.path).join(".rift"))
            .await
            .unwrap_or(false))
    .then(|| created.target.clone());
    Ok((
        0,
        Some(Created {
            slug: created.target.slug().to_owned(),
            path: created.target.path.to_string(),
            deferred_install,
        }),
    ))
}

/// The background action that installs `target`'s dependencies, or `None`
/// when the checkout has nothing to install.
async fn install_request(
    ctx: &AppContext,
    target: &wt_core::WorktreeTarget,
) -> Result<Option<wt_actions::ActionRequest>> {
    let config = ServiceConfig::from_config(&ctx.config);
    let policy = wt_platform::install::InstallPolicy {
        command: config.install_command,
        shell: config.shell,
    };
    let Some(plan) = policy.resolve(Path::new(&target.path), false).await? else {
        return Ok(None);
    };
    let mut command = vec![plan.spec.program.to_string_lossy().into_owned()];
    command.extend(
        plan.spec
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned()),
    );
    let mut config_selectors = std::collections::BTreeMap::new();
    if let Some(path) = &ctx.config.repository_config {
        config_selectors.insert("WT_REPO_CONFIG".into(), path.to_string_lossy().into_owned());
    }
    Ok(Some(wt_actions::ActionRequest {
        action_key: wt_core::worktree_target_key(target),
        slug: target.slug().to_owned(),
        worktree_ref: Some(wt_core::WorktreeRef::Local {
            slug: target.slug().to_owned(),
        }),
        action_id: CREATE_INSTALL_ACTION_ID.into(),
        action_name: "Install dependencies".into(),
        arg_history: None,
        prompt: plan.label,
        kind: wt_actions::ActionRunKind::Shell,
        command,
        cwd: Path::new(&target.path).to_path_buf(),
        affects: vec![wt_config::EffectTag::Git],
        issue_status: None,
        external: false,
        auto_fire_keys: Vec::new(),
        config_selectors,
    }))
}

/// A dependency install running as a durable background action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedInstall {
    /// The command, as the action's prompt shows it.
    pub label: String,
    pub log: std::path::PathBuf,
}

/// Install a created worktree's dependencies in a background action that
/// outlives this process, as the TUI's creation does. `None` when the
/// checkout has nothing to install.
pub async fn start_install(
    ctx: &AppContext,
    target: &wt_core::WorktreeTarget,
) -> Result<Option<StartedInstall>> {
    let Some(request) = install_request(ctx, target).await? else {
        return Ok(None);
    };
    let label = request.prompt.clone();
    let started = wt_app_core::actions::service(ctx)?
        .start(request, &ctx.cancellation)
        .await?;
    Ok(Some(StartedInstall {
        label,
        log: started.run_dir.join("stream.log"),
    }))
}

/// CLI creations (often run by agents) otherwise report failures only on
/// their own stderr; record them in the app log like TUI action failures.
fn log_failure(request: &str, message: &str) {
    tracing::error!(
        target: "wt::creation",
        request = %wt_core::sanitize_terminal_text(request),
        error = %wt_core::sanitize_terminal_text(message),
        "wt new failed"
    );
}

fn should_open(args: &NewArgs) -> bool {
    if args.no_open {
        return false;
    }
    args.open || std::io::stdin().is_terminal()
}

/// A creation request's branch and the title it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub branch: String,
    pub title: CreationTitle,
}

impl From<String> for Parsed {
    fn from(branch: String) -> Self {
        Self {
            branch,
            title: CreationTitle::None,
        }
    }
}

/// The tracker's title for a bare issue id, read while the worktree is
/// created. Dropping it abandons the read.
pub struct IssueTitleLookup(Option<tokio::task::JoinHandle<Option<String>>>);

impl IssueTitleLookup {
    /// Once creation is done, the read gets this much longer before the
    /// worktree is titled with the bare id instead.
    const AFTER_CREATION: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn start(ctx: &AppContext, title: &CreationTitle) -> Self {
        let CreationTitle::Issue(id) = title else {
            return Self(None);
        };
        let (ctx, id) = (ctx.clone(), id.clone());
        Self(Some(tokio::spawn(async move {
            wt_app_core::issue_tracker::title(&ctx, &id, &ctx.cancellation).await
        })))
    }

    pub async fn finish(mut self) -> Option<String> {
        let read = self.0.as_mut()?;
        match tokio::time::timeout(Self::AFTER_CREATION, read).await {
            Ok(title) => title.ok().flatten(),
            Err(_) => {
                tracing::warn!("issue title read outlasted creation; titled with the id");
                None
            }
        }
    }
}

impl Drop for IssueTitleLookup {
    fn drop(&mut self) {
        if let Some(read) = &self.0 {
            read.abort();
        }
    }
}

/// The title a worktree starts from: the words it was given (from `creator`),
/// else the tracker's title, else the bare issue id.
pub fn initial_title(
    title: CreationTitle,
    creator: TitleSource,
    issue_title: Option<String>,
) -> Option<(String, TitleSource)> {
    match title {
        CreationTitle::None => None,
        CreationTitle::Given(title) => Some((wt_store::sentence_case(&title), creator)),
        CreationTitle::Issue(id) => Some(match issue_title {
            Some(title) => (title, TitleSource::Issue),
            None => (id, TitleSource::Fallback),
        }),
    }
}

/// Save a new worktree's title. A failure only costs the title, so it is
/// logged rather than failing the creation.
pub async fn record_title(
    ctx: &AppContext,
    slug: &str,
    title: CreationTitle,
    creator: TitleSource,
    issue_title: Option<String>,
) {
    let Some((title, source)) = initial_title(title, creator, issue_title) else {
        return;
    };
    let db_slug = slug.to_owned();
    if let Err(error) = ctx
        .database
        .call(move |store| Ok(store.set_slug_title(&db_slug, &title, source, None)?))
        .await
    {
        tracing::warn!(slug, error = %format!("{error:#}"), "could not save the new worktree's title");
    }
}

pub async fn parse_branch(
    ctx: &AppContext,
    raw: &str,
    args: &NewArgs,
    interactive: bool,
) -> Result<Parsed, String> {
    if raw.is_empty() {
        return Err("empty input".into());
    }
    let mut words = raw
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let issue_url = Regex::new(r"(?i)linear\.app/[^/]+/issue/([A-Z]+-\d+)")
        .expect("constant regex")
        .captures(&words[0])
        .and_then(|captures| {
            captures
                .get(1)
                .map(|value| value.as_str().to_ascii_uppercase())
        });
    if let Some(id) = issue_url {
        words[0] = id;
    }
    let issue_re = Regex::new(r"(?i)^([A-Z]+-\d+)$").expect("constant regex");
    if let Some(captures) = issue_re.captures(&words[0]) {
        let id = captures[1].to_ascii_uppercase();
        let prefix = id
            .split('-')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if args.attach {
            let refs = run_git(
                ctx,
                &ctx.config.paths.main_clone,
                ["branch", "--all", "--format=%(refname:short)"],
            )
            .await
            .map_err(|error| format!("failed to inspect branches: {error}"))?;
            let lower = id.to_ascii_lowercase();
            let issue_pattern = Regex::new(&format!(
                r"(?i)(?:^|[^a-z0-9]){}(?:-|$)",
                regex::escape(&lower)
            ))
            .expect("escaped issue regex");
            let wanted = format!("{}/{}", ctx.config.branch.prefix, lower);
            let mut found = Vec::<String>::new();
            for raw_ref in String::from_utf8_lossy(&refs.stdout).lines() {
                let branch = raw_ref.strip_prefix("origin/").unwrap_or(raw_ref);
                let matches = if args.any {
                    issue_pattern.is_match(branch)
                } else {
                    branch == wanted || branch.starts_with(&format!("{wanted}-"))
                };
                if matches && !found.iter().any(|existing| existing == branch) {
                    found.push(branch.to_owned());
                }
            }
            match found.as_slice() {
                [branch] => return Ok(branch.clone().into()),
                [] => {
                    return Err(if args.any {
                        format!("No existing branch for {id} to attach to.")
                    } else {
                        format!(
                            "No {}/ branch for {id} to attach to. Add --any to search every author's branches.",
                            ctx.config.branch.prefix
                        )
                    });
                }
                _ if interactive && std::io::stdin().is_terminal() => {
                    let index = wt_app_core::prompt::pick(
                        &found,
                        &format!("Multiple branches for {id}"),
                        &ctx.cancellation,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    return index
                        .map(|index| found[index].clone().into())
                        .ok_or_else(|| format!("no branch chosen for {id}"));
                }
                _ => {
                    return Err(format!(
                        "Multiple branches for {id}: {}. Pass the branch explicitly.",
                        found.join(", ")
                    ));
                }
            }
        }
        if ctx
            .config
            .issue_tracker
            .as_ref()
            .and_then(|tracker| tracker.prefix.as_deref())
            .is_some_and(|required| required != prefix)
        {
            let required = ctx
                .config
                .issue_tracker
                .as_ref()
                .and_then(|tracker| tracker.prefix.as_deref())
                .unwrap_or_default();
            return Err(format!(
                "{id} can't lead a worktree ([issue_tracker] prefix = \"{required}\"). Use `wt new {required}-NNNN …`, an issue-less slug, or --attach for an existing branch. To link a GitHub issue, use an issue-less slug with `--gh <n>`."
            ));
        }
        let words = words.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
        let suffix = slugify(args.slug.as_deref().unwrap_or(&words));
        let title = if words.is_empty() {
            CreationTitle::Issue(id.clone())
        } else {
            CreationTitle::Given(words)
        };
        let mut branch = format!("{}/{}", ctx.config.branch.prefix, id.to_ascii_lowercase());
        if suffix.is_empty() {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            branch.push_str(&format!("-task-{:x}", nanos & 0x00ff_ffff_ffff));
        } else {
            branch.push('-');
            branch.push_str(&suffix);
        }
        return Ok(Parsed { branch, title });
    }
    if words.len() == 1 && raw.contains('/') {
        return Ok(raw.to_owned().into());
    }
    let existing = run_git(
        ctx,
        &ctx.config.paths.main_clone,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{raw}"),
        ],
    )
    .await
    .map_err(|error| format!("failed to inspect branches: {error}"))?;
    if existing.status.success() {
        return Ok(raw.to_owned().into());
    }
    let slug = slugify(raw);
    if slug.is_empty() {
        return Err("input does not contain a valid branch name or slug".into());
    }
    // A single word is a slug; several are a name the creator chose.
    let title = if words.len() > 1 {
        CreationTitle::Given(raw.to_owned())
    } else {
        CreationTitle::None
    };
    Ok(Parsed {
        branch: format!("{}/{}", ctx.config.branch.prefix, slug),
        title,
    })
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut separator = false;
    for ch in value.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            separator = false;
        } else if !separator && !out.is_empty() {
            out.push('-');
            separator = true;
        }
    }
    out.trim_end_matches('-').to_owned()
}

async fn set_github_issue(ctx: &AppContext, slug: &str, issue: u64) -> Result<()> {
    let slug = slug.to_owned();
    ctx.database
        .call(move |store| {
            store.set_slug_github_issue(&slug, Some(issue))?;
            Ok(())
        })
        .await
        .context("record GitHub issue")
}

async fn open_editor(ctx: &AppContext, path: &Path) -> Result<()> {
    wt_app_core::editor::open(ctx, path).await
}

#[cfg(test)]
mod tests {
    use super::{
        CREATE_INSTALL_ACTION_ID, CreationTitle, InstallTiming, IssueTitleLookup, NewArgs,
        TitleSource, install_request, parse_branch, record_title, run_created, slugify,
    };
    use clap::{Args as _, Command, FromArgMatches};

    #[test]
    fn flags_after_issue_title_are_parsed_as_options() {
        let command = NewArgs::augment_args(Command::new("new"));
        let matches = command
            .try_get_matches_from([
                "new",
                "ENG-42",
                "fix",
                "calendar",
                "--no-open",
                "--no-install",
                "--gh",
                "17",
            ])
            .unwrap();
        let args = NewArgs::from_arg_matches(&matches).unwrap();
        assert_eq!(args.positionals, ["ENG-42", "fix", "calendar"]);
        assert!(args.no_open && args.no_install);
        assert_eq!(args.gh, Some(17));
    }

    #[test]
    fn unknown_flags_fail_and_hyphen_text_requires_separator() {
        let command = NewArgs::augment_args(Command::new("new"));
        assert!(
            command
                .clone()
                .try_get_matches_from(["new", "ENG-42", "--typo"])
                .is_err()
        );
        let matches = command
            .try_get_matches_from(["new", "ENG-42", "--", "--literal"])
            .unwrap();
        let args = NewArgs::from_arg_matches(&matches).unwrap();
        assert_eq!(args.positionals, ["ENG-42", "--literal"]);
    }

    async fn title_after(
        fixture: &wt_app_core::test_support::CommandFixture,
        input: &str,
    ) -> Option<(String, TitleSource)> {
        let args = NewArgs::default();
        let parsed = parse_branch(&fixture.ctx, input, &args, false)
            .await
            .unwrap();
        let looked_up = IssueTitleLookup::start(&fixture.ctx, &parsed.title)
            .finish()
            .await;
        record_title(
            &fixture.ctx,
            "one",
            parsed.title,
            TitleSource::Agent,
            looked_up,
        )
        .await;
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        wt_store::stored_title(&state, "one")
    }

    fn tracker(fixture: &mut wt_app_core::test_support::CommandFixture, reply: &str) {
        std::sync::Arc::make_mut(&mut fixture.ctx.config).issue_tracker =
            Some(wt_config::IssueTrackerConfig {
                status_command: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    format!("printf '%s' '{reply}'"),
                    "reader".into(),
                    "{ids}".into(),
                ]),
                ..Default::default()
            });
    }

    #[tokio::test]
    async fn a_bare_issue_id_takes_the_trackers_title() {
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        tracker(
            &mut fixture,
            r#"{"issues":[{"id":"ENG-42","status":"Todo","title":"Calendar renders twice"}]}"#,
        );
        assert_eq!(
            title_after(&fixture, "ENG-42").await,
            Some(("Calendar renders twice".into(), TitleSource::Issue))
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn an_explicit_slug_keeps_the_trackers_title() {
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        tracker(
            &mut fixture,
            r#"{"issues":[{"id":"ENG-42","status":"Todo","title":"Calendar renders twice"}]}"#,
        );
        let args = NewArgs {
            slug: Some("calendar-week".into()),
            ..Default::default()
        };
        let parsed = parse_branch(&fixture.ctx, "ENG-42", &args, false)
            .await
            .unwrap();
        assert!(
            parsed.branch.ends_with("eng-42-calendar-week"),
            "{}",
            parsed.branch
        );
        assert_eq!(parsed.title, CreationTitle::Issue("ENG-42".into()));
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_or_titleless_lookup_falls_back_to_the_id() {
        for reply in [
            r#"{"issues":[{"id":"ENG-42","status":"Todo"}]}"#,
            "not json",
        ] {
            let mut fixture = wt_app_core::test_support::CommandFixture::new()
                .await
                .unwrap();
            tracker(&mut fixture, reply);
            assert_eq!(
                title_after(&fixture, "ENG-42").await,
                Some(("ENG-42".into(), TitleSource::Fallback)),
                "{reply}"
            );
            fixture.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn title_words_are_the_creators_and_a_slug_sets_none() {
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        tracker(
            &mut fixture,
            r#"{"issues":[{"id":"ENG-42","status":"Todo","title":"Ignored"}]}"#,
        );
        assert_eq!(
            title_after(&fixture, "ENG-42 fix calendar week view").await,
            Some(("Fix calendar week view".into(), TitleSource::Agent))
        );
        let fresh = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        assert_eq!(title_after(&fresh, "some-slug").await, None);
        assert_eq!(
            title_after(&fresh, "Tidy the release notes").await,
            Some(("Tidy the release notes".into(), TitleSource::Agent))
        );
        fixture.close().await.unwrap();
        fresh.close().await.unwrap();
    }

    fn git(main: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(main)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    }

    /// Creation fetches `origin`, which the bare command fixture lacks.
    fn add_origin(fixture: &wt_app_core::test_support::CommandFixture) {
        let main = &fixture.ctx.config.paths.main_clone;
        let origin = fixture._root.path().join("origin.git");
        git(main, &["clone", "--bare", ".", origin.to_str().unwrap()]);
        git(main, &["remote", "add", "origin", origin.to_str().unwrap()]);
    }

    /// A fixture whose main clone has a lockfile and whose install command
    /// leaves a marker in the checkout it runs in.
    async fn installing_fixture() -> wt_app_core::test_support::CommandFixture {
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let main = fixture.ctx.config.paths.main_clone.clone();
        std::fs::write(main.join("bun.lock"), "").unwrap();
        git(&main, &["add", "bun.lock"]);
        git(&main, &["commit", "-m", "lockfile"]);
        add_origin(&fixture);
        std::sync::Arc::make_mut(&mut fixture.ctx.config)
            .lifecycle
            .install_command = Some("touch installed.marker".into());
        fixture
    }

    fn new_args(branch: &str) -> NewArgs {
        NewArgs {
            positionals: vec![branch.into()],
            no_open: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_deferred_install_returns_before_installing_and_names_the_action_it_owes() {
        let fixture = installing_fixture().await;

        let (code, created) = run_created(
            &fixture.ctx,
            &new_args("feature/deferred"),
            InstallTiming::Deferred,
        )
        .await
        .unwrap();
        let created = created.unwrap();
        assert_eq!(code, 0);
        let path = std::path::Path::new(&created.path);
        assert!(path.join("bun.lock").exists());
        assert!(
            !path.join("installed.marker").exists(),
            "creation must not wait for the install"
        );
        let target = created.deferred_install.expect("the install is owed");
        let request = install_request(&fixture.ctx, &target)
            .await
            .unwrap()
            .expect("a lockfile means an install");
        // The board marks a row as installing by this id and key.
        assert_eq!(request.action_id, CREATE_INSTALL_ACTION_ID);
        assert_eq!(request.action_key, wt_core::worktree_target_key(&target));
        assert_eq!(request.cwd, path);
        assert_eq!(
            request.command.last().map(String::as_str),
            Some("touch installed.marker")
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn an_inline_or_skipped_install_owes_nothing() {
        let fixture = installing_fixture().await;

        let (_, inline) = run_created(
            &fixture.ctx,
            &new_args("feature/inline"),
            InstallTiming::Inline,
        )
        .await
        .unwrap();
        let inline = inline.unwrap();
        assert!(
            std::path::Path::new(&inline.path)
                .join("installed.marker")
                .exists()
        );
        assert_eq!(inline.deferred_install, None);

        let skipped = NewArgs {
            no_install: true,
            ..new_args("feature/skipped")
        };
        let (_, skipped) = run_created(&fixture.ctx, &skipped, InstallTiming::Deferred)
            .await
            .unwrap();
        assert_eq!(skipped.unwrap().deferred_install, None);
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_checkout_without_a_lockfile_has_no_install_to_start() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        add_origin(&fixture);
        let (_, created) = run_created(
            &fixture.ctx,
            &new_args("feature/plain"),
            InstallTiming::Deferred,
        )
        .await
        .unwrap();
        let target = created.unwrap().deferred_install.unwrap();
        assert!(
            install_request(&fixture.ctx, &target)
                .await
                .unwrap()
                .is_none()
        );
        fixture.close().await.unwrap();
    }

    #[test]
    fn slugifies_pasted_titles_like_the_ts_cli() {
        assert_eq!(
            slugify(" Fix calendar rendering! "),
            "fix-calendar-rendering"
        );
        assert_eq!(slugify("!!!"), "");
    }
}
