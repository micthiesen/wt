//! Code-owned action definitions shared by the palette and dispatcher.

use wt_config::{ActionDef, ActionKind, ActionTarget, Config, EffectTag, RequireTag};
use wt_tui::ActionSurface;

pub const CUSTOM_ID: &str = "__custom__";

pub fn definitions(config: &Config, surface: &ActionSurface) -> Vec<ActionDef> {
    let mut out = match surface {
        ActionSurface::Row { .. } => {
            let mut values = pinned(config);
            values.extend(
                config
                    .actions
                    .iter()
                    .filter(|def| def.target != ActionTarget::Slot)
                    .cloned(),
            );
            values.extend(review_bot(config));
            values
        }
        ActionSurface::Manager { .. } => {
            let mut values = manager();
            values.extend(
                config
                    .actions
                    .iter()
                    .filter(|d| d.kind == ActionKind::Claude && d.target == ActionTarget::Manager)
                    .cloned(),
            );
            values
        }
        ActionSurface::Slot { .. } => slot(),
    };
    out.push(custom(surface));
    out
}

pub fn is_fleet(id: &str) -> bool {
    matches!(
        id,
        "manager-digest"
            | "manager-triage"
            | "manager-merge-order"
            | "manager-nudge"
            | "manager-audit"
            | "manager-start-next"
            | "manager-compact"
            | "slot-continue"
            | "slot-compact"
            | CUSTOM_ID
    )
}

pub fn is_direct(id: &str) -> bool {
    matches!(id, "manager-compact" | "slot-compact")
}

fn custom(surface: &ActionSurface) -> ActionDef {
    let target = match surface {
        ActionSurface::Row { .. } => ActionTarget::Headless,
        ActionSurface::Manager { .. } => ActionTarget::Manager,
        ActionSurface::Slot { .. } => ActionTarget::Slot,
    };
    ActionDef {
        kind: ActionKind::Claude,
        id: CUSTOM_ID.into(),
        name: "Custom prompt…".into(),
        prompt: None,
        target,
        affects: None,
        requires: Vec::new(),
        shell: None,
        issue_status: None,
        key: Some("c".into()),
        group: None,
        external: false,
        arg_prompt: None,
        label_extract: None,
    }
}

fn pinned(config: &Config) -> Vec<ActionDef> {
    let mut values = vec![
        prompt(
            "agent-status-sync",
            "Agent: update work status",
            "Reassess this worktree's work status right now and assert it with `wt status`. Check the actual state — tree, recent commits, PR/CI, your own conversation context — rather than trusting the recorded status. Then run the matching `wt status <state>` (bare `wt status` prints the vocabulary and rules; use --risk and -m per those rules). If the recorded status is already accurate, re-assert it anyway so the timestamp reflects this check. Reply with one line: the state you asserted and why.",
            ActionTarget::Session,
            "u",
            "agent",
        ),
        prompt(
            "agent-continue",
            "Agent: continue work",
            "Pick this worktree back up and continue from its current work status. First recheck the actual state (tree, PR/CI, review findings) rather than trusting the recorded status blindly. Then do the next real unit of work: unfinished implementation → keep building; review findings pending → address them; needs-testing → run the manual testing yourself; everything genuinely done → verify and assert `wt status ready` with an honest --risk. Assert status transitions as you go, and escalate with `wt status needs-human -m \"...\"` only if truly blocked on the human.",
            ActionTarget::Session,
            "g",
            "agent",
        ),
    ];
    if config.dev_server.is_some() {
        let bin = std::env::current_exe()
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "wt".into());
        values.push(shell(
            "dev-server-start",
            "Start/restart dev server",
            format!("{} dev start {{{{slug}}}}", quote(&bin)),
            vec![EffectTag::Dev],
            "d",
            "dev server",
        ));
        values.push(shell(
            "dev-server-stop",
            "Stop dev server",
            format!("{} dev stop {{{{slug}}}}", quote(&bin)),
            vec![EffectTag::Dev],
            "s",
            "dev server",
        ));
    }
    values
}

fn review_bot(config: &Config) -> Vec<ActionDef> {
    config
        .review_bot
        .rerun_command
        .as_ref()
        .map(|command| {
            vec![ActionDef {
                kind: ActionKind::Shell,
                id: "review-bot-rerun".into(),
                name: format!("Re-run {} review", config.review_bot.name),
                prompt: None,
                shell: Some(format!(
                    "gh pr comment {{{{pr}}}} --body {}",
                    quote(command)
                )),
                target: ActionTarget::Headless,
                affects: Some(vec![EffectTag::Github]),
                requires: vec![RequireTag::Pr],
                issue_status: None,
                key: None,
                group: None,
                external: false,
                arg_prompt: None,
                label_extract: None,
            }]
        })
        .unwrap_or_default()
}

fn manager() -> Vec<ActionDef> {
    let report = "When finished, run `wt manager report [--ok|--warn|--err] \"...\"` with a one-or-two-line result — that line lands in wt's attention feed, so keep it terse and information-dense. Reply in your own conversation only with what the report doesn't carry.";
    let mut values = vec![
        prompt(
            "manager-digest",
            "Digest: what needs me",
            &format!(
                "Produce a fleet digest for the human. Read `wt status --all --json` and check PR/CI state (`gh`) where it matters. Reply with at most five bullets covering: what needs the human RIGHT NOW (and exactly what for), what is mergeable and in what order, and what looks stalled or abandoned. No restating the board — only what's actionable or surprising. {report}"
            ),
            ActionTarget::Manager,
            "d",
            "manager",
        ),
        prompt(
            "manager-triage",
            "Triage needs-human rows",
            &format!(
                "Triage every worktree currently asserting needs-human (`wt status --all --json`). For each: first try to unblock it yourself — gh operations, answering the worker's question from fleet knowledge, nudging its session with `wt agent send <slug> \"...\"` — and re-assert its status on the worker's behalf when you do. Distill whatever genuinely remains into one short ask per row. {report}"
            ),
            ActionTarget::Manager,
            "t",
            "manager",
        ),
        prompt(
            "manager-merge-order",
            "Plan merge order",
            &format!(
                "Plan the merge order. Look at the ready rows and open PRs (`wt status --all --json`, `gh pr list`), stack relationships, and overlapping files across branches. Propose a concrete merge order with the conflict risks, which restacks each merge will force, and anything that should NOT merge yet and why. {report}"
            ),
            ActionTarget::Manager,
            "o",
            "manager",
        ),
        prompt(
            "manager-nudge",
            "Nudge stalled workers",
            &format!(
                "Find stalled workers: rows asserting working/review whose sessions have gone quiet or whose status timestamps are old (`wt status --all --json`). Nudge each live-but-idle one with a pointed `wt agent send <slug> \"...\"` naming what it should do next; note the ones that are genuinely blocked rather than stalled. {report}"
            ),
            ActionTarget::Manager,
            "n",
            "manager",
        ),
        prompt(
            "manager-audit",
            "Audit work statuses",
            &format!(
                "Audit every asserted work status against reality (`wt status --all --json`, `gh`, session liveness): PR merged but row not cleaned? CI red under a ready? New commits after the assertion? Session dead mid working? Fix drifted records by asserting the true state on the row's behalf (`wt status <slug> <state> ...`). {report}"
            ),
            ActionTarget::Manager,
            "a",
            "manager",
        ),
        prompt(
            "manager-start-next",
            "Start next todo",
            &format!(
                "Pick the next todo work to start. From rows asserting todo (`wt status --all --json`), choose the highest-value one(s) given current fleet load (don't flood — a couple at most), and kick each off with `wt agent start <slug>`, which invokes its prepared prompt.txt through the bundled start skill and makes that agent own its status transitions. {report}"
            ),
            ActionTarget::Manager,
            "s",
            "manager",
        ),
        prompt(
            "manager-ask-row",
            "Ask about selected row",
            "Question about {{slug}} (branch {{branch}}) — answer from fleet knowledge, `wt status`, and `gh`; delegate to the row's own session only if it requires the worktree's conversation context:",
            ActionTarget::Manager,
            "r",
            "manager",
        ),
    ];
    values.push(ActionDef { kind: ActionKind::Claude, id: "manager-compact".into(), name: "Compact manager context".into(), prompt: Some("/compact Today is {{today}}. State that date verbatim in the summary you produce — a compaction summary is otherwise undated, and everything after it reasons from a stale one. Preserve: current fleet state (per-slug statuses, in-flight nudges, pending merge order, unresolved escalations) and any standing briefs from the config. You are the wt manager session; immediately after this compaction, re-run /manager to reload your playbook before doing anything else.".into()), shell: None, target: ActionTarget::Manager, affects: Some(Vec::new()), requires: Vec::new(), issue_status: None, key: Some("m".into()), group: Some("manager".into()), external: false, arg_prompt: None, label_extract: None });
    values
}

fn slot() -> Vec<ActionDef> {
    vec![
        prompt("slot-continue", "Continue current work", "Pick your current work back up and continue. Recheck actual state first (tree, git status, your own conversation context) rather than assuming; then do the next real unit of work. If nothing is pending, reply with one line saying so and stop.", ActionTarget::Slot, "g", ""),
        ActionDef { kind: ActionKind::Claude, id: "slot-compact".into(), name: "Compact context".into(), prompt: Some("/compact Today is {{today}}. State that date verbatim in the summary you produce, so nothing after this compaction has to guess it.".into()), shell: None, target: ActionTarget::Slot, affects: Some(Vec::new()), requires: Vec::new(), issue_status: None, key: Some("m".into()), group: None, external: false, arg_prompt: None, label_extract: None },
    ]
}

fn prompt(
    id: &str,
    name: &str,
    body: &str,
    target: ActionTarget,
    key: &str,
    group: &str,
) -> ActionDef {
    ActionDef {
        kind: ActionKind::Claude,
        id: id.into(),
        name: name.into(),
        prompt: Some(body.into()),
        shell: None,
        target,
        affects: Some(Vec::new()),
        requires: Vec::new(),
        issue_status: None,
        key: Some(key.into()),
        group: Some(group.into()),
        external: false,
        arg_prompt: None,
        label_extract: None,
    }
}

fn shell(
    id: &str,
    name: &str,
    command: String,
    affects: Vec<EffectTag>,
    key: &str,
    group: &str,
) -> ActionDef {
    ActionDef {
        kind: ActionKind::Shell,
        id: id.into(),
        name: name.into(),
        prompt: None,
        shell: Some(command),
        target: ActionTarget::Headless,
        affects: Some(affects),
        requires: Vec::new(),
        issue_status: None,
        key: Some(key.into()),
        group: Some(group.into()),
        external: false,
        arg_prompt: None,
        label_extract: None,
    }
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
