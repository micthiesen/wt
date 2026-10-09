use std::io::IsTerminal;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Args;
use regex::Regex;
use wt_lifecycle::{CreateOptions, LifecycleService, ServiceConfig};

use crate::{commands::resolve::run_git, context::AppContext};

const USAGE: &str = "usage: wt new <id [title…]|url|branch|slug> [--slug s] [--gh n] [--attach] [--any] [--base ref] [--open|--no-open] [--no-install]";

#[derive(Debug, Clone, Args, Default)]
pub struct NewArgs {
    #[arg(value_name = "INPUT", num_args = 0..)]
    pub positionals: Vec<String>,
    #[arg(long, value_name = "SLUG")]
    pub slug: Option<String>,
    #[arg(long, value_name = "N")]
    pub gh: Option<u64>,
    #[arg(long, requires = "attach")]
    pub any: bool,
    #[arg(long)]
    pub attach: bool,
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,
    #[arg(long, conflicts_with = "no_open")]
    pub open: bool,
    #[arg(long, conflicts_with = "open")]
    pub no_open: bool,
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
    let branch = match parse_branch(ctx, &raw, args, true).await {
        Ok(branch) => branch,
        Err(message) => {
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
            eprintln!("{error}");
            return Ok(1);
        }
    };
    if let Some(number) = args.gh {
        set_github_issue(ctx, created.target.slug(), number).await?;
    }
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

fn should_open(args: &NewArgs) -> bool {
    if args.no_open {
        return false;
    }
    args.open || std::io::stdin().is_terminal()
}

pub(crate) async fn parse_branch(
    ctx: &AppContext,
    raw: &str,
    args: &NewArgs,
    interactive: bool,
) -> Result<String, String> {
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
                [branch] => return Ok(branch.clone()),
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
                    let index = crate::prompt::pick(
                        &found,
                        &format!("Multiple branches for {id}"),
                        &ctx.cancellation,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    return index
                        .map(|index| found[index].clone())
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
                "{id} can't lead a worktree ([issue_tracker] prefix = \"{required}\"). Use `wt new {required}-NNNN …`, an issue-less slug, or --attach for an existing branch."
            ));
        }
        let title = args
            .slug
            .clone()
            .unwrap_or_else(|| words.iter().skip(1).cloned().collect::<Vec<_>>().join(" "));
        let suffix = slugify(&title);
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
        return Ok(branch);
    }
    if words.len() == 1 && raw.contains('/') {
        return Ok(raw.to_owned());
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
        return Ok(raw.to_owned());
    }
    let slug = slugify(raw);
    if slug.is_empty() {
        return Err("input does not contain a valid branch name or slug".into());
    }
    Ok(format!("{}/{}", ctx.config.branch.prefix, slug))
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
    crate::editor::open(ctx, path).await
}

#[cfg(test)]
mod tests {
    use super::{NewArgs, slugify};
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

    #[test]
    fn slugifies_pasted_titles_like_the_ts_cli() {
        assert_eq!(
            slugify(" Fix calendar rendering! "),
            "fix-calendar-rendering"
        );
        assert_eq!(slugify("!!!"), "");
    }
}
