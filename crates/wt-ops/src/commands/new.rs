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

const USAGE: &str = "usage: wt new <id [title…]|url|branch|slug> [--slug s] [--gh n] [--attach] [--any] [--base ref] [--open|--no-open] [--no-install]";

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

pub async fn run(ctx: &AppContext, args: &NewArgs) -> Result<i32> {
    if args.positionals.is_empty() {
        eprintln!("{USAGE}");
        return Ok(2);
    }
    if args.any && !args.attach {
        eprintln!("--any only applies with --attach");
        return Ok(2);
    }
    if args.gh == Some(0) {
        eprintln!("--gh requires a positive issue number");
        return Ok(2);
    }
    let raw = args.positionals.join(" ").trim().to_owned();
    let Parsed { branch, title } = match parse_branch(ctx, &raw, args, true).await {
        Ok(parsed) => parsed,
        Err(message) => {
            log_failure(&raw, &message);
            eprintln!("{message}");
            return Ok(1);
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
        return Ok(0);
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
                run_install: !args.no_install,
            },
            &ctx.cancellation,
        )
        .await
    {
        Ok(created) => created,
        Err(error) => {
            log_failure(&branch, &error.to_string());
            eprintln!("{error}");
            return Ok(1);
        }
    };
    if let Some(number) = args.gh {
        set_github_issue(ctx, created.target.slug(), number).await?;
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
        open_editor(ctx, Path::new(&created.target.path)).await?;
    }
    Ok(0)
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

/// Save a new worktree's title: the words it was created with (from
/// `creator`), else the tracker's title, else the bare issue id. A failure
/// only costs the title, so it is logged rather than failing the creation.
pub async fn record_title(
    ctx: &AppContext,
    slug: &str,
    title: CreationTitle,
    creator: TitleSource,
    issue_title: Option<String>,
) {
    let (title, source) = match title {
        CreationTitle::None => return,
        CreationTitle::Given(title) => (title, creator),
        CreationTitle::Issue(id) => match issue_title {
            Some(title) => (title, TitleSource::Issue),
            None => (id, TitleSource::Fallback),
        },
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
        CreationTitle, IssueTitleLookup, NewArgs, TitleSource, parse_branch, record_title, slugify,
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
            Some(("fix calendar week view".into(), TitleSource::Agent))
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

    #[test]
    fn slugifies_pasted_titles_like_the_ts_cli() {
        assert_eq!(
            slugify(" Fix calendar rendering! "),
            "fix-calendar-rendering"
        );
        assert_eq!(slugify("!!!"), "");
    }
}
