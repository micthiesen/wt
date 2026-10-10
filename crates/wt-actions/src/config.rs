use std::collections::BTreeMap;

use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wt_config::{ActionDef, ActionKind, ActionTarget, EffectTag, RequireTag};
use wt_core::WorktreeRef;

use crate::{ActionRunKind, service::ActionRequest};

pub type ActionVars = BTreeMap<String, String>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionRow {
    pub slug: String,
    pub issue_id: Option<String>,
    pub issue_prefix: Option<String>,
    pub pr: Option<ActionPr>,
    pub pr_number: Option<String>,
    pub deployed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionPr {
    pub state: String,
    pub is_draft: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionDestination {
    Headless,
    Session,
    Manager,
    Slot,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionContext {
    pub action_key: String,
    pub slug: String,
    pub cwd: std::path::PathBuf,
    pub worktree_ref: Option<WorktreeRef>,
    pub base: String,
    pub base_branch: String,
    pub branch: String,
    pub issue_id: String,
    pub stage: String,
    pub skill_prefix: String,
    pub arg: Option<String>,
    /// Shell selected by the application for `kind = "shell"`; keeping it in
    /// context makes planning independent of process-global environment.
    pub shell: Option<String>,
    /// Optional local-date rendering supplied by the application. If absent,
    /// `today` is rendered in UTC so pure fixtures remain deterministic.
    pub today: Option<String>,
    pub row: ActionRow,
    pub harness: ActionHarness,
    pub auto_fire_keys: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActionHarness {
    #[default]
    Claude,
    Codex,
    OpenCode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedAction {
    pub request: ActionRequest,
    pub destination: ActionDestination,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionPlan {
    Tracked(Box<PreparedAction>),
    Prompt {
        destination: ActionDestination,
        prompt: String,
        slug: String,
        worktree_ref: Option<WorktreeRef>,
    },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ActionPlanError {
    #[error("action {action:?} requires {reason}")]
    Requirement { action: String, reason: String },
    #[error("action {0:?} has no executable command or prompt")]
    MissingPayload(String),
}

pub fn prepare_action(
    def: &ActionDef,
    context: &ActionContext,
    extras: &str,
) -> Result<ActionPlan, ActionPlanError> {
    prepare_action_with_vars(def, context, extras, &ActionVars::new())
}

/// Prepare the usual action plan with caller-supplied variables taking
/// precedence over the values derived from the current row. This is used by
/// queued automations whose inputs were frozen when the fire was evaluated.
pub fn prepare_action_with_vars(
    def: &ActionDef,
    context: &ActionContext,
    extras: &str,
    overrides: &ActionVars,
) -> Result<ActionPlan, ActionPlanError> {
    let availability = evaluate_requirements(&def.requires, &context.row);
    if let Some(reason) = availability {
        return Err(ActionPlanError::Requirement {
            action: def.name.clone(),
            reason,
        });
    }
    let mut vars = context.vars();
    vars.extend(overrides.clone());
    let rendered_extras = apply_vars(extras, &vars).trim().to_owned();
    if def.kind == ActionKind::Claude && def.target != ActionTarget::Headless {
        let prompt = rendered_prompt(def, &vars, &rendered_extras)?;
        return Ok(ActionPlan::Prompt {
            destination: match def.target {
                ActionTarget::Session => ActionDestination::Session,
                ActionTarget::Manager => ActionDestination::Manager,
                ActionTarget::Slot => ActionDestination::Slot,
                ActionTarget::Headless => ActionDestination::Headless,
            },
            prompt,
            slug: context.slug.clone(),
            worktree_ref: context.worktree_ref.clone(),
        });
    }

    let (kind, prompt, argv) = match def.kind {
        ActionKind::Shell => {
            let shell = context.shell.as_deref().unwrap_or("bash").to_owned();
            let command = apply_vars(def.shell.as_deref().unwrap_or_default(), &vars);
            if command.trim().is_empty() {
                return Err(ActionPlanError::MissingPayload(def.name.clone()));
            }
            (
                ActionRunKind::Shell,
                command.clone(),
                vec![shell, "-lc".into(), command],
            )
        }
        ActionKind::Claude => {
            let prompt = rendered_prompt(def, &vars, &rendered_extras)?;
            if prompt.is_empty() {
                return Err(ActionPlanError::MissingPayload(def.name.clone()));
            }
            let (kind, argv) = headless_runner(context.harness, &prompt, &context.cwd);
            (kind, prompt, argv)
        }
    };
    let affects = def.affects.clone().unwrap_or_else(|| match def.kind {
        ActionKind::Claude => vec![EffectTag::Git, EffectTag::Github],
        ActionKind::Shell => Vec::new(),
    });
    Ok(ActionPlan::Tracked(Box::new(PreparedAction {
        request: ActionRequest {
            arg_history: context.arg.as_ref().map(|value| crate::ActionArgHistory {
                value: value.clone(),
                label_extract: def.label_extract.clone(),
                launch_token: None,
            }),
            issue_status: (def.kind == ActionKind::Shell && !context.issue_id.is_empty())
                .then(|| {
                    def.issue_status
                        .as_ref()
                        .map(|status| crate::IssueStatusExpectation {
                            issue_id: context.issue_id.clone(),
                            status: status.clone(),
                        })
                })
                .flatten(),
            action_key: if context.action_key.is_empty() {
                context.slug.clone()
            } else {
                context.action_key.clone()
            },
            slug: context.slug.clone(),
            worktree_ref: context.worktree_ref.clone(),
            action_id: def.id.clone(),
            action_name: def.name.clone(),
            prompt,
            kind,
            command: argv,
            cwd: context.cwd.clone(),
            affects,
            external: def.external,
            auto_fire_keys: context.auto_fire_keys.clone(),
            config_selectors: BTreeMap::new(),
        },
        destination: ActionDestination::Headless,
    })))
}

pub fn evaluate_requirements(requires: &[RequireTag], row: &ActionRow) -> Option<String> {
    for requirement in requires {
        match requirement {
            RequireTag::Pr if row.pr.is_none() => return Some("no PR".into()),
            RequireTag::PrReady => match &row.pr {
                None => return Some("no PR".into()),
                Some(pr) if pr.is_draft => return Some("PR is draft".into()),
                Some(pr) if pr.state != "OPEN" => return Some("PR not open".into()),
                Some(_) => {}
            },
            RequireTag::Deployed if !row.deployed => {
                return Some("no stage deployed".into());
            }
            RequireTag::IssueTracker => {
                let id = row
                    .issue_id
                    .as_deref()
                    .or_else(|| issue_id_for_slug(&row.slug));
                let valid = id.is_some_and(|id| tracker_id_valid(id, row.issue_prefix.as_deref()));
                if !valid {
                    return Some("no tracker id (set one with `#`)".into());
                }
            }
            _ => {}
        }
    }
    None
}

pub fn apply_vars(template: &str, vars: &ActionVars) -> String {
    let today = vars.get("today").cloned().unwrap_or_else(|| {
        OffsetDateTime::now_utc()
            .format(&time::macros::format_description!(
                "[weekday repr:long], [month repr:long] [day], [year]"
            ))
            .unwrap_or_else(|_| {
                OffsetDateTime::now_utc()
                    .format(&Rfc3339)
                    .unwrap_or_else(|_| "unknown date".into())
            })
    });
    let mut result = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        result.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            result.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let key = &after_open[..end];
        let valid = !key.is_empty()
            && key
                .chars()
                .all(|ch| ch == '_' || ch.is_ascii_alphanumeric());
        let value = if valid {
            if key == "today" {
                Some(today.as_str())
            } else {
                vars.get(key).map(String::as_str)
            }
        } else {
            None
        };
        if let Some(value) = value {
            result.push_str(value);
        } else {
            result.push_str(&rest[start..start + end + 4]);
        }
        rest = &after_open[end + 2..];
    }
    result.push_str(rest);
    result
}

impl ActionContext {
    fn vars(&self) -> ActionVars {
        let mut vars = BTreeMap::from([
            ("base".into(), self.base.clone()),
            ("base_branch".into(), self.base_branch.clone()),
            ("branch".into(), self.branch.clone()),
            ("slug".into(), self.slug.clone()),
            ("cwd".into(), self.cwd.to_string_lossy().into_owned()),
            ("issue_id".into(), self.issue_id.clone()),
            ("stage".into(), self.stage.clone()),
            ("skill_prefix".into(), self.skill_prefix.clone()),
        ]);
        if let Some(arg) = &self.arg {
            vars.insert("arg".into(), arg.clone());
        }
        if let Some(pr) = &self.row.pr_number {
            vars.insert("pr".into(), pr.clone());
        }
        if let Some(today) = &self.today {
            vars.insert("today".into(), today.clone());
        }
        vars
    }
}

fn rendered_prompt(
    def: &ActionDef,
    vars: &ActionVars,
    extras: &str,
) -> Result<String, ActionPlanError> {
    let prompt = apply_vars(def.prompt.as_deref().unwrap_or_default(), vars);
    let prompt = if extras.is_empty() {
        prompt
    } else if prompt.is_empty() {
        extras.to_owned()
    } else {
        format!("{prompt}\n\n{extras}")
    };
    if prompt.trim().is_empty() {
        return Err(ActionPlanError::MissingPayload(def.name.clone()));
    }
    Ok(prompt)
}

fn headless_runner(
    harness: ActionHarness,
    prompt: &str,
    cwd: &std::path::Path,
) -> (ActionRunKind, Vec<String>) {
    match harness {
        ActionHarness::Claude => (
            ActionRunKind::Claude,
            vec![
                "claude".into(),
                "-p".into(),
                "--permission-mode".into(),
                "auto".into(),
                "--verbose".into(),
                "--output-format".into(),
                "stream-json".into(),
                prompt.into(),
            ],
        ),
        ActionHarness::Codex => (
            ActionRunKind::Harness,
            vec![
                "codex".into(),
                "exec".into(),
                "--color".into(),
                "never".into(),
                "--".into(),
                prompt.into(),
            ],
        ),
        ActionHarness::OpenCode => (
            ActionRunKind::Harness,
            vec![
                "opencode".into(),
                "run".into(),
                "--dir".into(),
                cwd.to_string_lossy().into_owned(),
                "--".into(),
                prompt.into(),
            ],
        ),
    }
}

fn issue_id_for_slug(slug: &str) -> Option<&str> {
    let bytes = slug.as_bytes();
    for start in 0..bytes.len() {
        if !bytes[start].is_ascii_alphabetic()
            || (start > 0 && bytes[start - 1].is_ascii_alphanumeric())
        {
            continue;
        }
        let mut i = start;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'-' {
            continue;
        }
        i += 1;
        let digits = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits || (i < bytes.len() && bytes[i] != b'-') {
            continue;
        }
        return slug.get(start..i);
    }
    None
}

fn tracker_id_valid(id: &str, prefix: Option<&str>) -> bool {
    let Some((team, number)) = id.split_once('-') else {
        return false;
    };
    !team.is_empty()
        && team.chars().all(|ch| ch.is_ascii_alphabetic())
        && !number.is_empty()
        && number.chars().all(|ch| ch.is_ascii_digit())
        && !id.eq_ignore_ascii_case("GH-0")
        && !id
            .get(..3)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("GH-"))
        && prefix.is_none_or(|prefix| {
            id.to_ascii_uppercase()
                .starts_with(&format!("{}-", prefix.to_ascii_uppercase()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_config::{ActionArgPrompt, ActionTarget};

    fn context() -> ActionContext {
        ActionContext {
            action_key: "wk-1".into(),
            slug: "wk-1".into(),
            cwd: "/tmp/wt-wk-1".into(),
            base: "main".into(),
            base_branch: "origin/main".into(),
            branch: "feature/wk-1".into(),
            issue_id: "WK-1".into(),
            stage: "preview".into(),
            skill_prefix: "wk".into(),
            today: Some("Friday, October 9, 2026".into()),
            row: ActionRow {
                slug: "wk-1".into(),
                issue_id: Some("WK-1".into()),
                issue_prefix: Some("WK".into()),
                pr: Some(ActionPr {
                    state: "OPEN".into(),
                    is_draft: false,
                }),
                pr_number: Some("73".into()),
                deployed: true,
            },
            ..ActionContext::default()
        }
    }

    #[test]
    fn template_replaces_known_values_and_preserves_unknown_tokens() {
        let vars = BTreeMap::from([
            ("slug".into(), "wk-1".into()),
            ("today".into(), "Tuesday".into()),
        ]);
        assert_eq!(
            apply_vars("{{slug}} {{today}} {{missing}} {{not-valid!}}", &vars),
            "wk-1 Tuesday {{missing}} {{not-valid!}}"
        );
    }

    #[test]
    fn prompt_action_target_is_preserved_as_a_routed_prompt() {
        let def = ActionDef {
            id: "review".into(),
            name: "Review {{branch}}".into(),
            prompt: Some("Inspect {{branch}} for {{issue_id}} (PR {{pr}}) on {{today}}".into()),
            target: ActionTarget::Slot,
            arg_prompt: Some(ActionArgPrompt {
                label: "Context".into(),
            }),
            ..ActionDef::default()
        };
        let ActionPlan::Prompt {
            destination,
            prompt,
            slug,
            ..
        } = prepare_action(&def, &context(), "extra {{slug}}").unwrap()
        else {
            panic!("expected a routed prompt");
        };
        assert_eq!(destination, ActionDestination::Slot);
        assert_eq!(slug, "wk-1");
        assert_eq!(
            prompt,
            "Inspect feature/wk-1 for WK-1 (PR 73) on Friday, October 9, 2026\n\nextra wk-1"
        );
    }

    #[test]
    fn requirements_fail_closed_and_issue_tracker_is_not_github() {
        let requirements = [
            RequireTag::PrReady,
            RequireTag::IssueTracker,
            RequireTag::Deployed,
        ];
        let mut row = context().row;
        row.issue_id = Some("GH-73".into());
        assert_eq!(
            evaluate_requirements(&requirements, &row).as_deref(),
            Some("no tracker id (set one with `#`)")
        );
        row.issue_id = Some("WK-73".into());
        row.pr.as_mut().unwrap().is_draft = true;
        assert_eq!(
            evaluate_requirements(&requirements, &row).as_deref(),
            Some("PR is draft")
        );
        row.pr.as_mut().unwrap().is_draft = false;
        assert_eq!(evaluate_requirements(&requirements, &row), None);
    }

    #[test]
    fn headless_runner_uses_harness_specific_argument_order() {
        let claude = ActionDef {
            id: "x".into(),
            name: "x".into(),
            prompt: Some("work {{slug}}".into()),
            ..ActionDef::default()
        };
        let ActionPlan::Tracked(prepared) = prepare_action(&claude, &context(), "").unwrap() else {
            panic!("headless action should be tracked");
        };
        assert_eq!(prepared.request.command.first().unwrap(), "claude");
        assert_eq!(prepared.request.command.last().unwrap(), "work wk-1");
        assert_eq!(prepared.request.config_selectors, BTreeMap::new());
    }

    #[test]
    fn tracked_action_captures_argument_and_label_extractor() {
        let def = ActionDef {
            id: "lookup".into(),
            name: "Lookup".into(),
            prompt: Some("resolve {{arg}}".into()),
            label_extract: Some("Resolved: (.*)".into()),
            ..ActionDef::default()
        };
        let mut context = context();
        context.arg = Some("raw-key".into());
        let ActionPlan::Tracked(prepared) = prepare_action(&def, &context, "").unwrap() else {
            panic!("headless action should be tracked");
        };
        assert_eq!(
            prepared.request.arg_history,
            Some(crate::ActionArgHistory {
                value: "raw-key".into(),
                label_extract: Some("Resolved: (.*)".into()),
                launch_token: None,
            })
        );
    }
}
