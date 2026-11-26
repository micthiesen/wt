use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::Args;
use wt_app_core::context::AppContext;
use wt_ops::commands::{
    agent::{AgentArgs, AgentCommand},
    new::{Created, InstallTiming, NewArgs},
    section::{SectionArgs, SectionCommand},
};

use super::status::StatusArgs;

const MAX_BRIEF_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Args, Default)]
pub struct NewCommandArgs {
    #[command(flatten)]
    pub new: NewArgs,
    #[command(flatten)]
    pub launch: LaunchArgs,
}

/// What to do with a worktree once it exists, so launching a task is one
/// command. Separate invocations are safe to run in parallel.
#[derive(Debug, Clone, Args, Default)]
pub struct LaunchArgs {
    /// Put the new worktree in this section (created if new; `-` is the inbox).
    #[arg(long, value_name = "NAME")]
    pub section: Option<String>,
    /// Write this file (`-` for stdin) as the new worktree's prompt.txt brief.
    #[arg(long, value_name = "FILE")]
    pub brief: Option<String>,
    /// Queue the new worktree as `todo` with this note.
    #[arg(short = 'm', long = "note", allow_hyphen_values = true)]
    pub note: Option<String>,
    /// Start the worktree's agent on the brief once everything else is set.
    /// Dependencies then install in the background.
    #[arg(long)]
    pub start: bool,
    /// With --start, finish the dependency install before anything else.
    #[arg(long, requires = "start")]
    pub wait_install: bool,
    /// With --start, start the Claude Code session on this model family,
    /// such as `fable` or `opus`. Only when a specific model was asked for.
    #[arg(long, value_name = "NAME", requires = "start")]
    pub model: Option<String>,
}

impl LaunchArgs {
    fn any(&self) -> bool {
        self.section.is_some() || self.brief.is_some() || self.note.is_some() || self.start
    }

    /// Only a launch that starts an agent defers the install: a script that
    /// creates a worktree and returns relies on finding dependencies present.
    fn install_timing(&self) -> InstallTiming {
        if self.start && !self.wait_install {
            InstallTiming::Deferred
        } else {
            InstallTiming::Inline
        }
    }
}

pub async fn run(ctx: &AppContext, args: &NewCommandArgs) -> Result<i32> {
    let launch = &args.launch;
    // Refuse a bad model name, and read the brief, before anything exists.
    if let Some(model) = &launch.model {
        wt_ops::commands::agent::claude_model_alias(model)?;
    }
    // Read the brief first: a missing file must not leave a worktree behind.
    let brief = match &launch.brief {
        Some(source) => Some(read_brief(source).await?),
        None => None,
    };
    let (code, created) =
        wt_ops::commands::new::run_created(ctx, &args.new, launch.install_timing()).await?;
    let Some(created) = created else {
        if code == 0 && launch.any() {
            eprintln!(
                "the worktree already existed, so --section, --brief, --note and --start were not applied"
            );
            return Ok(1);
        }
        return Ok(code);
    };
    let mut owed_install = created.deferred_install.as_ref();
    for (index, step) in steps(launch).iter().enumerate() {
        let done = match step {
            Step::Brief => write_brief(&created, brief.as_deref().unwrap_or_default()).await,
            Step::Section(name) => section(ctx, &created, name).await,
            Step::Todo(note) => todo(ctx, &created, note).await,
            Step::Start(model) => {
                if let Some(target) = owed_install.take() {
                    background_install(ctx, target).await;
                }
                start(ctx, &created, *model).await
            }
        };
        let failure = match done {
            Ok(0) => continue,
            Ok(code) => format!("exit {code}"),
            Err(error) => format!("{error:#}"),
        };
        // A launch that stops early still owes the worktree its dependencies.
        if let Some(target) = owed_install.take() {
            background_install(ctx, target).await;
        }
        eprintln!(
            "{} failed for {}: {failure}\nThe worktree was created. Finish with:",
            step.label(),
            created.slug
        );
        for owed in &steps(launch)[index..] {
            eprintln!("  {}", owed.manual(&created));
        }
        return Ok(1);
    }
    Ok(code)
}

#[derive(Debug, Eq, PartialEq)]
enum Step<'a> {
    Brief,
    Section(&'a str),
    Todo(&'a str),
    Start(Option<&'a str>),
}

/// The agent starts last, so it finds its brief, section and status in place.
fn steps(launch: &LaunchArgs) -> Vec<Step<'_>> {
    let mut steps = Vec::new();
    if launch.brief.is_some() {
        steps.push(Step::Brief);
    }
    if let Some(name) = &launch.section {
        steps.push(Step::Section(name));
    }
    if let Some(note) = &launch.note {
        steps.push(Step::Todo(note));
    }
    if launch.start {
        steps.push(Step::Start(launch.model.as_deref()));
    }
    steps
}

impl Step<'_> {
    fn label(&self) -> &'static str {
        match self {
            Step::Brief => "writing the brief",
            Step::Section(_) => "moving to the section",
            Step::Todo(_) => "setting todo",
            Step::Start(_) => "starting the agent",
        }
    }

    fn manual(&self, created: &Created) -> String {
        match self {
            Step::Brief => format!("write the brief to {}/prompt.txt", created.path),
            Step::Section(name) => {
                format!("wt section mv --only -- {} {name:?}", created.slug)
            }
            Step::Todo(note) => format!("wt status {} todo -m {note:?}", created.slug),
            Step::Start(None) => format!("wt agent start {}", created.slug),
            Step::Start(Some(model)) => {
                format!("wt agent start {} --model {model}", created.slug)
            }
        }
    }
}

async fn read_brief(source: &str) -> Result<String> {
    let source = source.to_owned();
    // Blocking reads, off the async workers: stdin may be a slow pipe.
    let text = tokio::task::spawn_blocking(move || -> Result<String> {
        use std::io::Read;
        let mut text = String::new();
        if source == "-" {
            std::io::stdin()
                .take(MAX_BRIEF_BYTES + 1)
                .read_to_string(&mut text)
                .context("read the brief from stdin")?;
        } else {
            std::fs::File::open(&source)
                .with_context(|| format!("open the brief {source}"))?
                .take(MAX_BRIEF_BYTES + 1)
                .read_to_string(&mut text)
                .with_context(|| format!("read the brief {source}"))?;
        }
        Ok(text)
    })
    .await
    .context("brief reader failed")??;
    if text.len() as u64 > MAX_BRIEF_BYTES {
        bail!("the brief exceeds {MAX_BRIEF_BYTES} bytes");
    }
    if text.trim().is_empty() {
        bail!("the brief is empty");
    }
    Ok(text)
}

async fn write_brief(created: &Created, brief: &str) -> Result<i32> {
    let path = Path::new(&created.path).join("prompt.txt");
    tokio::fs::write(&path, brief)
        .await
        .with_context(|| format!("write {}", path.display()))?;
    println!("  brief: {}", path.display());
    Ok(0)
}

async fn section(ctx: &AppContext, created: &Created, name: &str) -> Result<i32> {
    wt_ops::commands::section::run(
        ctx,
        &SectionArgs {
            json: false,
            command: Some(SectionCommand::Mv {
                positionals: vec![created.slug.clone(), name.to_owned()],
                only: true,
            }),
        },
        // Only the new worktree moves: no stack to split.
        &std::collections::BTreeSet::new(),
    )
    .await
}

async fn todo(ctx: &AppContext, created: &Created, note: &str) -> Result<i32> {
    super::status::run(
        ctx,
        &StatusArgs {
            positionals: vec![created.slug.clone(), "todo".into()],
            note: Some(note.to_owned()),
            ..StatusArgs::default()
        },
    )
    .await
}

/// A failure to start costs only the install, which the agent or the user can
/// run, so it is reported and the launch goes on.
async fn background_install(ctx: &AppContext, target: &wt_core::WorktreeTarget) {
    match wt_ops::commands::new::start_install(ctx, target).await {
        Ok(Some(install)) => println!(
            "  install: {} continues in the background; watch it with: tail -f {}",
            wt_core::sanitize_terminal_text(&install.label),
            install.log.display()
        ),
        Ok(None) => {}
        Err(error) => eprintln!(
            "the dependency install did not start ({error:#}); run it in {}",
            target.path
        ),
    }
}

async fn start(ctx: &AppContext, created: &Created, model: Option<&str>) -> Result<i32> {
    wt_ops::commands::agent::run(
        ctx,
        &AgentArgs {
            command: AgentCommand::Start {
                target: created.slug.clone(),
                model: model.map(str::to_owned),
            },
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Command, FromArgMatches};

    fn parse(args: &[&str]) -> NewCommandArgs {
        let command = NewCommandArgs::augment_args(Command::new("new"));
        let matches = command
            .try_get_matches_from(std::iter::once("new").chain(args.iter().copied()))
            .unwrap();
        NewCommandArgs::from_arg_matches(&matches).unwrap()
    }

    #[test]
    fn launch_steps_run_in_order_and_the_agent_starts_last() {
        let args = parse(&[
            "ENG-1",
            "--slug",
            "fix",
            "--start",
            "-m",
            "ship it",
            "--section",
            "Wave",
            "--brief",
            "-",
        ]);
        assert_eq!(args.new.positionals, ["ENG-1"]);
        assert_eq!(
            steps(&args.launch),
            [
                Step::Brief,
                Step::Section("Wave"),
                Step::Todo("ship it"),
                Step::Start(None)
            ]
        );
        assert_eq!(
            steps(&parse(&["ENG-1", "--start", "--model", "fable"]).launch),
            [Step::Start(Some("fable"))]
        );
        assert!(steps(&parse(&["ENG-1"]).launch).is_empty());
    }

    #[test]
    fn only_a_launch_that_starts_an_agent_defers_the_install() {
        let timing = |args: &[&str]| parse(args).launch.install_timing();
        assert_eq!(timing(&["ENG-1"]), InstallTiming::Inline);
        assert_eq!(
            timing(&["ENG-1", "--brief", "-", "--section", "Wave"]),
            InstallTiming::Inline
        );
        assert_eq!(timing(&["ENG-1", "--start"]), InstallTiming::Deferred);
        assert_eq!(
            timing(&["ENG-1", "--start", "--wait-install"]),
            InstallTiming::Inline
        );
        let command = NewCommandArgs::augment_args(Command::new("new"));
        assert!(
            command
                .try_get_matches_from(["new", "ENG-1", "--wait-install"])
                .is_err(),
            "--wait-install without --start changes nothing, so it is refused"
        );
    }

    #[test]
    fn a_failed_step_names_the_commands_that_finish_the_launch() {
        let created = Created {
            slug: "eng-1-fix".into(),
            path: "/wt/eng-1-fix".into(),
            deferred_install: None,
        };
        assert_eq!(
            Step::Section("Wave one").manual(&created),
            "wt section mv --only -- eng-1-fix \"Wave one\""
        );
        assert_eq!(
            Step::Start(None).manual(&created),
            "wt agent start eng-1-fix"
        );
        assert_eq!(
            Step::Start(Some("fable")).manual(&created),
            "wt agent start eng-1-fix --model fable"
        );
    }

    #[tokio::test]
    async fn a_missing_or_empty_brief_fails_before_anything_is_created() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            read_brief(dir.path().join("none").to_str().unwrap())
                .await
                .is_err()
        );
        let empty = dir.path().join("empty.txt");
        std::fs::write(&empty, "  \n").unwrap();
        assert!(read_brief(empty.to_str().unwrap()).await.is_err());
        let brief = dir.path().join("brief.txt");
        std::fs::write(&brief, "Objective\nfix it\n").unwrap();
        assert_eq!(
            read_brief(brief.to_str().unwrap()).await.unwrap(),
            "Objective\nfix it\n"
        );
    }
}
