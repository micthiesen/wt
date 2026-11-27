//! `wt mark`: add a line to a worktree's history, typed by hand or reported
//! by a harness hook when a skill or slash command is used in its session.

use std::{io::Read, path::PathBuf};

use anyhow::Result;
use clap::Args;
use serde_json::Value;
use wt_app_core::context::AppContext;
use wt_config::HistoryConfig;
use wt_core::WorktreeEventKind;
use wt_ops::commands::resolve::{Inventory, worktree_at_cwd};

use super::watch::{agent, owner_in};

#[derive(Debug, Clone, Args, Default)]
pub struct MarkArgs {
    /// The note. Its first line is kept, cut to 160 characters.
    #[arg(
        value_name = "TEXT",
        required_unless_present_any = ["claude_hook", "stamp"],
        conflicts_with_all = ["claude_hook", "stamp"]
    )]
    pub text: Vec<String>,
    /// Stamp the worktree as having been through a round of this skill, at
    /// its current HEAD, in place of any stamp it has. For harnesses that
    /// report no skill uses: `[history] stamps` does this by itself where
    /// the Claude Code hook is installed.
    #[arg(long, value_name = "NAME", conflicts_with = "claude_hook")]
    pub stamp: Option<String>,
    /// The worktree to mark (default: the current one, then WT_AGENT).
    #[arg(long, value_name = "SLUG", conflicts_with = "claude_hook")]
    pub slug: Option<String>,
    /// Read one Claude Code hook event on stdin and record the skill use it
    /// describes, when `[history] skills` or `stamps` lists that skill.
    /// Always exits 0 and prints nothing.
    #[arg(long)]
    pub claude_hook: bool,
    /// What the hook event described, read before the application starts.
    #[arg(skip)]
    pub hook_use: Option<SkillUse>,
}

/// One recorded use of a skill or slash command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillUse {
    /// Without the leading slash.
    pub name: String,
    pub args: Option<String>,
    /// The agent invoked it itself; otherwise the person typed it.
    pub from_agent: bool,
    /// The session's directory, when the event named one.
    pub cwd: Option<PathBuf>,
    /// The stamp a round of it leaves, as `[history] stamps` names it.
    pub stamp: Option<String>,
}

/// The hook event waiting on stdin, when it is a skill use to record.
/// Nothing is read while no skill is configured.
pub fn read_hook(history: &HistoryConfig) -> Option<SkillUse> {
    if history.is_empty() {
        return None;
    }
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok()?;
    skill_use(&serde_json::from_str(&input).ok()?, history)
}

/// The skill use a Claude Code hook event describes, when the configuration
/// records that skill: a prompt that is a slash command, or the agent's own
/// call of the Skill tool.
pub fn skill_use(event: &Value, history: &HistoryConfig) -> Option<SkillUse> {
    let text = |value: &Value| value.as_str().map(str::to_owned);
    let (name, args, from_agent) = match event.get("hook_event_name")?.as_str()? {
        "UserPromptSubmit" => {
            let line = event.get("prompt")?.as_str()?.trim().lines().next()?;
            let command = line.strip_prefix('/')?;
            let (name, args) = command
                .split_once(char::is_whitespace)
                .unwrap_or((command, ""));
            (name.to_owned(), Some(args.to_owned()), false)
        }
        "PreToolUse" if event.get("tool_name")?.as_str()? == "Skill" => {
            let input = event.get("tool_input")?;
            let name = input.get("skill")?.as_str()?.trim();
            (
                name.strip_prefix('/').unwrap_or(name).to_owned(),
                input.get("args").and_then(text),
                true,
            )
        }
        _ => return None,
    };
    // A path such as /Users/me/notes.txt is not a command name.
    let named = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-'));
    if !named || !history.records_skill(&name) {
        return None;
    }
    Some(SkillUse {
        stamp: history.stamp_for(&name).map(str::to_owned),
        name,
        args: args
            .map(|args| wt_core::event_text(&wt_core::sanitize_terminal_lines(&args)))
            .filter(|args| !args.is_empty()),
        from_agent,
        cwd: event
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .map(PathBuf::from),
    })
}

pub async fn run(ctx: &AppContext, args: &MarkArgs) -> Result<i32> {
    let agent = agent();
    if args.claude_hook {
        if let Some(used) = &args.hook_use {
            record_skill(ctx, used, agent.as_deref()).await;
        }
        return Ok(0);
    }
    if let Some(name) = &args.stamp {
        return stamp(ctx, name, args.slug.as_deref(), agent.as_deref()).await;
    }
    let text = wt_core::event_text(&wt_core::sanitize_terminal_text(&args.text.join(" ")));
    if text.is_empty() {
        eprintln!("a mark needs some text");
        return Ok(2);
    }
    let slug = match target(ctx, args.slug.as_deref(), agent.as_deref()).await {
        Ok((slug, _)) => slug,
        Err(message) => {
            eprintln!("{message}");
            return Ok(1);
        }
    };
    let event = WorktreeEventKind::Mark {
        text: text.clone(),
        by: agent,
    };
    let (owner, at_ms) = (slug.clone(), wt_app_core::feeds::epoch_ms());
    ctx.database
        .call(move |store| Ok(store.append_worktree_event(&owner, at_ms, &event)?))
        .await?;
    println!("✓ marked {slug}: {text}");
    Ok(0)
}

/// `wt mark --stamp`: stamp the worktree now, at its HEAD.
async fn stamp(
    ctx: &AppContext,
    name: &str,
    slug: Option<&str>,
    agent: Option<&str>,
) -> Result<i32> {
    let name = name.strip_prefix('/').unwrap_or(name).to_owned();
    if !wt_core::is_stamp_name(&name) {
        eprintln!("a stamp is named like a skill: letters, digits, '_', ':', '.', '-'");
        return Ok(2);
    }
    let (slug, inventory) = match target(ctx, slug, agent).await {
        Ok(found) => found,
        Err(message) => {
            eprintln!("{message}");
            return Ok(1);
        }
    };
    let head = inventory
        .worktree(&slug)
        .and_then(|row| row.head_sha.clone());
    let (owner, skill, by) = (slug.clone(), name.clone(), agent.map(str::to_owned));
    let stamped = ctx
        .database
        .call(move |store| Ok(store.stamp_now(&owner, &skill, head.as_deref(), by.as_deref())?))
        .await?;
    match stamped {
        Some(stamp) => println!(
            "{} stamped {slug}: {name}{}",
            wt_core::STAMP_CURRENT,
            stamp
                .sha
                .as_deref()
                .map(|sha| format!(" at {}", &sha[..sha.len().min(7)]))
                .unwrap_or_default()
        ),
        None => println!(
            "{slug} has no status to carry a stamp: {name} is pending and is earned by its next wt status"
        ),
    }
    Ok(0)
}

/// The named worktree, the current one, or else the agent's own, with the
/// listing it was found in.
async fn target(
    ctx: &AppContext,
    slug: Option<&str>,
    agent: Option<&str>,
) -> Result<(String, Inventory), String> {
    let inventory = super::watch::inventory(ctx).await?;
    let found = match (owner_in(&inventory, slug, "a mark"), slug, agent) {
        (Err(error), None, Some(agent)) if agent != "manager" => {
            owner_in(&inventory, Some(agent), "a mark").map_err(|_| error)
        }
        (found, ..) => found,
    };
    Ok((found?, inventory))
}

/// Record a skill use on the worktree that holds the session's directory.
/// The main clone and a directory outside every worktree record nothing.
async fn record_skill(ctx: &AppContext, used: &SkillUse, agent: Option<&str>) -> Option<String> {
    let records = ctx.repository.inventory(&ctx.cancellation).await.ok()?;
    let cwd = used.cwd.as_deref().unwrap_or(&ctx.cwd);
    let slug = worktree_at_cwd(&records, cwd)
        .filter(|record| !record.is_main)?
        .target
        .slug()
        .to_owned();
    let event = WorktreeEventKind::Skill {
        name: used.name.clone(),
        args: used.args.clone(),
        by: used.from_agent.then(|| agent.unwrap_or(&slug).to_owned()),
    };
    wt_app_core::worktree_history::record(ctx, &slug, event).await;
    if let Some(stamp) = used.stamp.clone() {
        let (owner, by) = (slug.clone(), agent.map(str::to_owned));
        let pending = ctx
            .database
            .call(move |store| Ok(store.set_pending_stamp(&owner, &stamp, by.as_deref())?))
            .await;
        if let Err(error) = pending {
            tracing::debug!(%error, "could not record a pending stamp");
        }
    }
    Some(slug)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use clap::Parser;
    use serde_json::json;

    use super::*;

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: MarkArgs,
    }

    fn listed(skills: &[&str]) -> HistoryConfig {
        HistoryConfig {
            skills: skills.iter().map(|skill| (*skill).to_owned()).collect(),
            ..HistoryConfig::default()
        }
    }

    fn typed(prompt: &str) -> Value {
        json!({"hook_event_name": "UserPromptSubmit", "cwd": "/w/one", "prompt": prompt, "session_id": "s"})
    }

    fn called(tool: &str, input: Value) -> Value {
        json!({"hook_event_name": "PreToolUse", "tool_name": tool, "cwd": "/w/one", "tool_input": input})
    }

    /// The name, arguments and whether the agent did it.
    fn used(event: &Value, skills: &[&str]) -> Option<(String, Option<String>, bool)> {
        skill_use(event, &listed(skills)).map(|used| (used.name, used.args, used.from_agent))
    }

    fn name(
        name: &str,
        args: Option<&str>,
        from_agent: bool,
    ) -> Option<(String, Option<String>, bool)> {
        Some((name.to_owned(), args.map(str::to_owned), from_agent))
    }

    #[test]
    fn the_parser_takes_text_or_the_hook_never_both() {
        let parsed =
            TestCli::try_parse_from(["wt", "handed", "to", "QA", "--slug", "one"]).unwrap();
        assert_eq!(parsed.args.text, ["handed", "to", "QA"]);
        assert_eq!(parsed.args.slug.as_deref(), Some("one"));
        assert!(
            TestCli::try_parse_from(["wt", "--claude-hook"])
                .unwrap()
                .args
                .claude_hook
        );
        assert!(TestCli::try_parse_from(["wt"]).is_err());
        assert!(TestCli::try_parse_from(["wt", "--claude-hook", "text"]).is_err());
    }

    #[test]
    fn a_typed_slash_command_is_a_skill_use_only_when_it_is_listed() {
        let skills = ["fable-review", "check"];
        assert_eq!(
            used(&typed("/fable-review"), &skills),
            name("fable-review", None, false)
        );
        assert_eq!(
            used(
                &typed("  /check src/foo  --deep \nand then more\n"),
                &skills
            ),
            name("check", Some("src/foo --deep"), false)
        );
        assert_eq!(
            skill_use(&typed("/check"), &listed(&skills)).unwrap().cwd,
            Some(PathBuf::from("/w/one"))
        );
        // Not a command, a path that only starts like one, and a command
        // nobody listed.
        assert_eq!(used(&typed("please run /check"), &skills), None);
        assert_eq!(used(&typed("/Users/me/file.txt is broken"), &["*"]), None);
        assert_eq!(used(&typed("/ check"), &["*"]), None);
        assert_eq!(used(&typed("/compact"), &skills), None);
        assert_eq!(used(&typed("/check"), &[]), None);
        // Names are exact.
        assert_eq!(used(&typed("/Check"), &skills), None);
        assert_eq!(used(&typed("/checks"), &skills), None);
        assert_eq!(
            used(&typed("/compact now"), &["*"]),
            name("compact", Some("now"), false)
        );
        // A plugin's skill is listed whole or by its own name.
        let plugin = typed("/tools:check");
        assert_eq!(used(&plugin, &skills), name("tools:check", None, false));
        assert_eq!(
            used(&plugin, &["tools:check"]),
            name("tools:check", None, false)
        );
        assert_eq!(used(&plugin, &["tools"]), None);
    }

    #[test]
    fn the_agents_own_skill_call_is_a_skill_use_and_other_events_are_not() {
        let skills = ["check"];
        assert_eq!(
            used(
                &called(
                    "Skill",
                    json!({"skill": "check", "args": "src/foo\nsecond line"})
                ),
                &skills
            ),
            name("check", Some("src/foo"), true)
        );
        assert_eq!(
            used(&called("Skill", json!({"skill": "/check"})), &skills),
            name("check", None, true)
        );
        assert_eq!(
            used(&called("Skill", json!({"skill": "other"})), &skills),
            None
        );
        assert_eq!(
            used(&called("Bash", json!({"command": "/check"})), &["*"]),
            None
        );
        assert_eq!(used(&called("Skill", json!({"args": "x"})), &["*"]), None);
        // Another hook, and input that is not a hook event at all.
        assert_eq!(
            used(
                &json!({"hook_event_name": "Stop", "prompt": "/check"}),
                &["*"]
            ),
            None
        );
        for garbage in [
            json!(null),
            json!("/check"),
            json!([1, 2]),
            json!({"prompt": "/check"}),
            json!({"hook_event_name": 3}),
        ] {
            assert_eq!(used(&garbage, &["*"]), None, "{garbage}");
        }
        assert!(serde_json::from_str::<Value>("{not json").is_err());
    }

    async fn events(ctx: &AppContext, slug: &'static str) -> Vec<WorktreeEventKind> {
        ctx.database
            .call(move |store| Ok(store.read_worktree_events(slug, 80)?))
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.event)
            .collect()
    }

    #[tokio::test]
    async fn the_hook_records_a_listed_skill_once_on_the_sessions_worktree() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let mut ctx = fixture.ctx.clone();
        let mut config = (*ctx.config).clone();
        config.history = listed(&["fable-review", "check"]);
        ctx.config = Arc::new(config);
        let root = ctx.config.paths.worktree_root.clone();
        let hook = |event: Value| MarkArgs {
            claude_hook: true,
            hook_use: skill_use(&event, &ctx.config.history),
            ..MarkArgs::default()
        };
        let prompt = |cwd: &std::path::Path, prompt: &str| json!({"hook_event_name": "UserPromptSubmit", "cwd": cwd, "prompt": prompt});

        // An unlisted command, a listed one typed in the main clone, and one
        // typed outside every worktree: nothing.
        for event in [
            prompt(&root.join("one"), "/compact"),
            prompt(&ctx.config.paths.main_clone, "/fable-review"),
            prompt(fixture._root.path(), "/fable-review"),
        ] {
            assert_eq!(run(&ctx, &hook(event)).await.unwrap(), 0);
        }
        assert!(events(&ctx, "one").await.is_empty());
        assert!(events(&ctx, "main").await.is_empty());

        assert_eq!(
            run(
                &ctx,
                &hook(prompt(&root.join("one"), "/fable-review the diff\nplease"))
            )
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            events(&ctx, "one").await,
            [WorktreeEventKind::Skill {
                name: "fable-review".into(),
                args: Some("the diff".into()),
                by: None,
            }]
        );

        // The agent's own call, from a subdirectory of another worktree
        // than the one this process started in, is that worktree's.
        let nested = root.join("two/src");
        std::fs::create_dir_all(&nested).unwrap();
        let call = json!({"hook_event_name": "PreToolUse", "tool_name": "Skill", "cwd": nested, "tool_input": {"skill": "check"}});
        let used = skill_use(&call, &ctx.config.history).unwrap();
        assert_eq!(
            record_skill(&ctx, &used, None).await.as_deref(),
            Some("two")
        );
        assert_eq!(
            record_skill(&ctx, &used, Some("manager")).await.as_deref(),
            Some("two")
        );
        let by = |by: &str| WorktreeEventKind::Skill {
            name: "check".into(),
            args: None,
            by: Some(by.into()),
        };
        assert_eq!(events(&ctx, "two").await, [by("two"), by("manager")]);
        assert_eq!(events(&ctx, "one").await.len(), 1);
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_stamping_skill_leaves_a_stamp_for_the_next_status_to_earn() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let mut ctx = fixture.ctx.clone();
        let mut config = (*ctx.config).clone();
        // Listed as a stamp only: it is recorded all the same.
        config.history = HistoryConfig {
            stamps: vec!["fable-review".into()],
            ..HistoryConfig::default()
        };
        ctx.config = Arc::new(config);
        let root = ctx.config.paths.worktree_root.clone();
        let event = json!({"hook_event_name": "UserPromptSubmit", "cwd": root.join("one"), "prompt": "/fable-review"});
        let hook = MarkArgs {
            claude_hook: true,
            hook_use: skill_use(&event, &ctx.config.history),
            ..MarkArgs::default()
        };
        assert_eq!(run(&ctx, &hook).await.unwrap(), 0);
        assert_eq!(events(&ctx, "one").await.len(), 1);
        let state = |ctx: &AppContext| {
            let database = ctx.database.clone();
            async move {
                database
                    .call(|store| Ok(store.read_wt_state()?))
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            wt_store::stored_pending_stamp(&state(&ctx).await, "one").as_deref(),
            Some("fable-review")
        );
        assert_eq!(wt_store::stored_stamp(&state(&ctx).await, "one"), None);

        // By hand, with no status yet: it waits for one too.
        let by_hand = |name: &str| MarkArgs {
            stamp: Some(name.into()),
            slug: Some("two".into()),
            ..MarkArgs::default()
        };
        assert_eq!(run(&ctx, &by_hand("/check")).await.unwrap(), 0);
        assert_eq!(
            wt_store::stored_pending_stamp(&state(&ctx).await, "two").as_deref(),
            Some("check")
        );
        assert_eq!(run(&ctx, &by_hand("not a name")).await.unwrap(), 2);
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_mark_is_one_bounded_line_on_a_worktree_and_never_on_the_main_clone() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        let mark = |text: &str, slug: Option<&str>| MarkArgs {
            text: vec![text.to_owned()],
            slug: slug.map(str::to_owned),
            ..MarkArgs::default()
        };
        assert_eq!(
            run(ctx, &mark("handed to \u{1b}[31mQA\nsecond line", None))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            run(ctx, &mark(&"x".repeat(400), Some("two")))
                .await
                .unwrap(),
            0
        );
        assert_eq!(run(ctx, &mark(" \n ", None)).await.unwrap(), 2);
        assert_eq!(run(ctx, &mark("lost", Some("nowhere"))).await.unwrap(), 1);
        assert_eq!(events(ctx, "one").await.len(), 1);
        let WorktreeEventKind::Mark { text, .. } = &events(ctx, "one").await[0] else {
            panic!("not a mark");
        };
        assert!(
            text.starts_with("handed to") && !text.contains('\u{1b}') && !text.contains('\n'),
            "{text}"
        );
        let WorktreeEventKind::Mark { text, .. } = &events(ctx, "two").await[0] else {
            panic!("not a mark");
        };
        assert_eq!(text.chars().count(), wt_core::EVENT_TEXT_MAX);

        let mut main = ctx.clone();
        main.cwd = ctx.config.paths.main_clone.clone();
        assert_eq!(
            target(&main, None, None).await.unwrap_err(),
            "a mark belongs to a worktree and the main clone is not one: pass --slug <slug>"
        );
        assert_eq!(run(&main, &mark("in main", None)).await.unwrap(), 1);
        // An agent outside its checkout still marks its own worktree.
        assert_eq!(target(&main, None, Some("two")).await.unwrap().0, "two");
        assert!(target(&main, None, Some("manager")).await.is_err());
        fixture.close().await.unwrap();
    }
}
