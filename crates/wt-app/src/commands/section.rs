use std::collections::BTreeSet;

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use wt_config::InstanceRole;
use wt_core::{ChainMember, build_stack_index};

use crate::{commands::resolve::resolve_from_inventory, context::AppContext};

const INBOX: &str = "\0inbox";
#[derive(Debug, Clone, Args, Default)]
pub struct SectionArgs {
    #[arg(long)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Option<SectionCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum SectionCommand {
    #[command(alias = "ls")]
    List {
        #[arg(long)]
        json: bool,
    },
    #[command(alias = "move")]
    Mv {
        #[arg(required = true, num_args = 2..)]
        positionals: Vec<String>,
        #[arg(long)]
        only: bool,
    },
    Rename {
        old: String,
        new: String,
    },
    #[command(alias = "remove")]
    Rm {
        name: String,
    },
}

pub async fn run(ctx: &AppContext, args: &SectionArgs) -> Result<i32> {
    if ctx.config.instance.role == InstanceRole::Worker {
        eprintln!("sections are controller-owned; run this command on the controller");
        return Ok(1);
    }
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    match &args.command {
        None => list(ctx, &state, args.json).await,
        Some(SectionCommand::List { json }) => list(ctx, &state, *json || args.json).await,
        Some(SectionCommand::Mv { positionals, only }) => {
            move_members(ctx, &state, positionals, *only).await
        }
        Some(SectionCommand::Rename { old, new }) => rename(ctx, &state, old, new).await,
        Some(SectionCommand::Rm { name }) => remove(ctx, &state, name).await,
    }
}

async fn list(ctx: &AppContext, state: &Value, json_output: bool) -> Result<i32> {
    let records = ctx.repository.inventory(&ctx.cancellation).await?;
    let live = records
        .iter()
        .filter(|r| !r.is_main)
        .map(|r| r.target.slug().to_owned())
        .collect::<BTreeSet<_>>();
    let sections = manual_sections(state);
    let folded = state
        .get("foldedSections")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let mut output = sections.iter().map(|name|json!({"name":name,"folded":folded.contains(name.as_str()),"slugs":slugs_in(state,Some(name),&live)})).collect::<Vec<_>>();
    let inbox = slugs_in(state, None, &live);
    if json_output {
        output.push(json!({"name":Value::Null,"folded":folded.contains(INBOX),"slugs":inbox}));
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(0);
    }
    if sections.is_empty() {
        println!("No sections. Create one: wt section mv <slug> <section>");
    }
    for name in sections {
        let rows = slugs_in(state, Some(&name), &live);
        println!(
            "{name} · {}{}",
            rows.len(),
            if folded.contains(name.as_str()) {
                " (folded)"
            } else {
                ""
            }
        );
        for slug in rows {
            println!("  {slug}");
        }
    }
    if !inbox.is_empty() {
        println!("(inbox) · {}", inbox.len());
        for slug in inbox {
            println!("  {slug}");
        }
    }
    Ok(0)
}

pub(crate) fn manual_sections(state: &Value) -> Vec<String> {
    state
        .get("sectionsOrder")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| *name != INBOX && !name.starts_with("\0stack:"))
        .map(str::to_owned)
        .collect()
}

fn slugs_in(state: &Value, section: Option<&str>, live: &BTreeSet<String>) -> Vec<String> {
    let mut entries = live
        .iter()
        .filter_map(|slug| {
            let entry = state.get("slugs").and_then(|slugs| slugs.get(slug));
            let section_matches = match (section, entry.and_then(|entry| entry.get("section"))) {
                (Some(name), Some(Value::String(actual))) => name == actual,
                (None, None | Some(Value::Null)) => true,
                _ => false,
            };
            section_matches.then(|| {
                (
                    entry
                        .and_then(|entry| entry.get("order"))
                        .and_then(Value::as_f64)
                        .unwrap_or(f64::INFINITY),
                    slug.clone(),
                )
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    entries.into_iter().map(|(_, slug)| slug).collect()
}

async fn move_members(
    ctx: &AppContext,
    state: &Value,
    positionals: &[String],
    only: bool,
) -> Result<i32> {
    let section_arg = positionals
        .last()
        .expect("clap requires at least two positionals");
    let to_inbox = section_arg == "-";
    let section = if to_inbox {
        None
    } else {
        if let Some(error) = invalid_name(section_arg) {
            eprintln!("{error}");
            return Ok(2);
        }
        Some(resolve_section(state, section_arg).unwrap_or_else(|| section_arg.trim().to_owned()))
    };
    let records = ctx.repository.inventory(&ctx.cancellation).await?;
    let worktrees = records.iter().filter(|w| !w.is_main).collect::<Vec<_>>();
    let members = worktrees
        .iter()
        .map(|w| {
            ChainMember::new(
                w.target.slug(),
                &w.target.branch,
                state
                    .get("slugs")
                    .and_then(|s| s.get(w.target.slug()))
                    .and_then(|e| e.get("baseBranch"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            )
        })
        .collect::<Vec<_>>();
    let stacks = build_stack_index(&members, &ctx.config.branch.base);
    let mut named = Vec::new();
    let mut unresolved = Vec::new();
    for arg in &positionals[..positionals.len() - 1] {
        match resolve_from_inventory(&records, arg, true) {
            Ok(w) => named.push(w.target.slug().to_owned()),
            Err(error) => unresolved.push(format!("{arg} ({error})")),
        }
    }
    if named.is_empty() {
        eprintln!("no worktrees could be resolved: {}", unresolved.join(", "));
        return Ok(1);
    }
    let mut moving = BTreeSet::new();
    for slug in &named {
        let Some(record) = worktrees.iter().find(|w| w.target.slug() == slug) else {
            moving.insert(slug.clone());
            continue;
        };
        if only {
            moving.insert(slug.clone());
            continue;
        }
        if let Some(entry) = stacks.by_branch.get(&record.target.branch) {
            for node in &stacks.layouts[entry.layout_index].nodes {
                if let Some(member) = members.iter().find(|m| m.branch == node.branch) {
                    moving.insert(member.slug.clone());
                }
            }
        } else {
            moving.insert(slug.clone());
        }
    }
    let to = section.clone();
    let changed = ctx
        .database
        .call(move |store| {
            Ok(store.move_worktrees_to_section(
                &moving.into_iter().collect::<Vec<_>>(),
                to.as_deref(),
            )?)
        })
        .await?;
    if changed.is_empty() {
        println!(
            "{} already in {}",
            named.join(", "),
            section.as_deref().unwrap_or("the inbox")
        );
    } else {
        println!(
            "✓ {} → {}{}",
            named.join(", "),
            section.as_deref().unwrap_or("the inbox"),
            if section
                .as_deref()
                .is_some_and(|name| !manual_sections(state).contains(&name.to_owned()))
            {
                " (new)"
            } else {
                ""
            }
        );
        let pulled = changed
            .iter()
            .filter(|slug| !named.contains(slug))
            .cloned()
            .collect::<Vec<_>>();
        if !pulled.is_empty() {
            println!(
                "  moved {} (stack), also {}",
                changed.len(),
                pulled.join(", ")
            );
        }
    }
    if !unresolved.is_empty() {
        eprintln!("no such worktree: {}", unresolved.join(", "));
    }
    Ok(if unresolved.is_empty() { 0 } else { 1 })
}

async fn rename(ctx: &AppContext, state: &Value, old: &str, new: &str) -> Result<i32> {
    if let Some(error) = invalid_name(new) {
        eprintln!("{error}");
        return Ok(2);
    }
    let Some(from) = resolve_section(state, old) else {
        eprintln!("no such section: {old}");
        return Ok(1);
    };
    let to = new.trim().to_owned();
    if from == to {
        println!("already named {to}");
        return Ok(0);
    }
    let merging = resolve_section(state, &to).is_some();
    let db_from = from.clone();
    let db_to = to.clone();
    ctx.database
        .call(move |store| {
            store.rename_section(&db_from, &db_to)?;
            Ok(())
        })
        .await?;
    println!(
        "✓ {}",
        if merging {
            format!("merged {from} into {to}")
        } else {
            format!("{from} → {to}")
        }
    );
    Ok(0)
}

async fn remove(ctx: &AppContext, state: &Value, input: &str) -> Result<i32> {
    let Some(name) = resolve_section(state, input) else {
        eprintln!("no such section: {input}");
        return Ok(1);
    };
    let row_count = ctx
        .database
        .call(move |store| Ok(store.remove_section(&name)?))
        .await?;
    println!(
        "✓ dropped {}{}",
        input,
        if row_count > 0 {
            format!(" · {row_count} rows → inbox")
        } else {
            String::new()
        }
    );
    Ok(0)
}

pub(crate) fn resolve_section(state: &Value, input: &str) -> Option<String> {
    let names = manual_sections(state);
    if names.iter().any(|name| name == input) {
        return Some(input.to_owned());
    }
    let matches = names
        .into_iter()
        .filter(|name| name.eq_ignore_ascii_case(input))
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| matches[0].clone())
}

pub(crate) fn invalid_name(name: &str) -> Option<&'static str> {
    if name.trim().is_empty() {
        Some("a section name can't be empty")
    } else if name.starts_with('\0') {
        Some("section names can't start with NUL (reserved for derived groups)")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn section_names_and_case_lookup_preserve_reserved_inbox() {
        assert_eq!(invalid_name(" "), Some("a section name can't be empty"));
        assert!(invalid_name("\0stack:abc").is_some());
        let state = json!({"sectionsOrder":[INBOX,"Release"]});
        assert_eq!(
            resolve_section(&state, "release").as_deref(),
            Some("Release")
        );
        assert_eq!(manual_sections(&state), ["Release"]);
    }

    #[tokio::test]
    async fn moves_whole_stack_and_only_mode_splits_it() {
        use crate::commands::test_support::CommandFixture;
        let fixture = CommandFixture::new().await.unwrap();
        fixture
            .ctx
            .database
            .call(|store| {
                store.set_slug_base("two", Some(("feature/one", None)))?;
                Ok(())
            })
            .await
            .unwrap();
        let move_stack = SectionArgs {
            json: false,
            command: Some(SectionCommand::Mv {
                positionals: vec!["one".into(), "Release".into()],
                only: false,
            }),
        };
        assert_eq!(run(&fixture.ctx, &move_stack).await.unwrap(), 0);
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(state["slugs"]["one"]["section"], "Release");
        assert_eq!(state["slugs"]["two"]["section"], "Release");

        let split = SectionArgs {
            json: false,
            command: Some(SectionCommand::Mv {
                positionals: vec!["two".into(), "Hold".into()],
                only: true,
            }),
        };
        assert_eq!(run(&fixture.ctx, &split).await.unwrap(), 0);
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(state["slugs"]["one"]["section"], "Release");
        assert_eq!(state["slugs"]["two"]["section"], "Hold");
        fixture.close().await.unwrap();
    }
}
