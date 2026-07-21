import { Fragment } from "react";

import {
  applyVars,
  type ActionAvailability,
  type ActionVars,
} from "../../core/actions.ts";
import type { ActionDef } from "../../core/config.ts";
import { getHarness } from "../../core/harness/index.ts";
import { Modal } from "../modal.tsx";
import { ScrollableList } from "./scroll-list.tsx";
import { theme } from "../theme.ts";

/** Claude-flavored action def — the only kind that uses the edit modal. */
type ClaudeActionDef = Extract<ActionDef, { kind: "claude" }>;

/**
 * Picker-mode item: one of the configured actions, or the trailing
 * "Custom prompt..." entry that drops you straight into a freeform
 * editor with no template prefix. `availability` reflects the def's
 * `requires` evaluated against the current row state — `ok: false`
 * grays the entry and surfaces the reason as the dim subtitle. The
 * Custom entry is always available.
 */
export type PickerItem =
  | {
      kind: "action";
      def: ActionDef;
      /** Resolved quick-pick letter (see `assignActionKeys`); "" = none. */
      key: string;
      availability: ActionAvailability;
    }
  | { kind: "custom" };

/**
 * Reserved single-char keys inside the action picker: `c` opens the
 * custom-prompt entry, `j`/`k` navigate, `q` cancels. Auto-derived
 * action keys skip these, and an explicit `key` that lands on one is
 * dropped (the action falls back to auto-derivation).
 */
const RESERVED_KEYS = new Set(["c", "j", "k", "q"]);

/**
 * Assign a stable single-letter quick-pick key to each action, returning
 * an id→key map. Two passes so explicit `key`s win regardless of order:
 *   1. Honor each def's explicit `key` when it's a free, non-reserved
 *      letter.
 *   2. Auto-derive the rest from the first free letter of the name, then
 *      any free a–z; leave blank if the alphabet is exhausted (>22
 *      actions), in which case the entry is reachable via j/k only.
 * Reserved keys (`c`/`j`/`k`/`q`) are never assigned.
 */
export function assignActionKeys(
  defs: readonly ActionDef[],
): Map<string, string> {
  const out = new Map<string, string>();
  const taken = new Set<string>(RESERVED_KEYS);
  for (const def of defs) {
    const k = def.key?.toLowerCase();
    if (k && /^[a-z]$/.test(k) && !taken.has(k)) {
      out.set(def.id, k);
      taken.add(k);
    }
  }
  for (const def of defs) {
    if (out.has(def.id)) continue;
    let assigned = "";
    for (const ch of def.name.toLowerCase()) {
      if (/[a-z]/.test(ch) && !taken.has(ch)) {
        assigned = ch;
        break;
      }
    }
    if (!assigned) {
      for (let c = 97; c <= 122; c++) {
        const ch = String.fromCharCode(c);
        if (!taken.has(ch)) {
          assigned = ch;
          break;
        }
      }
    }
    if (assigned) {
      out.set(def.id, assigned);
      taken.add(assigned);
    }
  }
  return out;
}

/**
 * Two-screen state machine. Esc in `edit` pops back to `list` when a
 * pre-built was selected (informative restore point) or cancels out
 * entirely from custom (no list state worth restoring). Only claude-
 * flavored actions reach `edit`; shell actions launch directly from
 * `list`.
 *
 * `items` is deliberately not in the state — it's recomputed at each
 * use site from `buildActionPickerItems(slug)`. That lets `requires`
 * predicates re-evaluate against live row state, so an optimistic
 * patch (or a background refetch) that flips a PR's draft status
 * unblocks/blocks actions in the open picker without requiring a
 * close-and-reopen.
 */
export type ActionPickerState =
  | { mode: "list"; slug: string; index: number }
  | { mode: "edit"; slug: string; def: ClaudeActionDef | null; extras: string };

type Props = {
  slug: string;
  items: PickerItem[];
  selectedIndex: number;
};

export function ActionPickerModal({ slug, items, selectedIndex }: Props) {
  // Claude Code's robot glyph, reused verbatim from the harness registry
  // so the action-kind marker matches the session badges.
  const claudeGlyph = getHarness("claude").glyph;
  const rowId = (item: PickerItem): string =>
    item.kind === "custom" ? "action:__custom__" : `action:${item.def.id}`;
  const selectedId = items[selectedIndex]
    ? rowId(items[selectedIndex]!)
    : undefined;
  return (
    <Modal
      title={`action · ${slug}`}
      inset={{ top: "12%", right: "18%", bottom: "12%", left: "18%" }}
      hints={[
        ["j/k", "move"],
        ["a-z", "quick pick"],
        ["c", "custom prompt"],
        ["! / ⏎", "select"],
        ["esc / q", "cancel"],
      ]}
    >
      <ScrollableList selectedId={selectedId} revision={items}>
      {items.map((item, i) => {
        const selected = i === selectedIndex;
        const bg = selected ? theme.rowSelectedBg : undefined;
        const isCustom = item.kind === "custom";
        const blocked = !isCustom && !item.availability.ok;
        // Group header: rendered once above the first item of each group
        // (groups are pre-clustered in `buildActionPickerItems`). The
        // custom entry has no group, so it sits below the last section.
        const group = isCustom ? null : item.def.group ?? null;
        const prevGroup =
          i === 0
            ? null
            : items[i - 1]!.kind === "action"
              ? (items[i - 1] as Extract<PickerItem, { kind: "action" }>).def
                  .group ?? null
              : null;
        const showHeader = group !== null && group !== prevGroup;
        // Custom entry gets the `c` chord prefix (mirrors `n` for "+ new
        // section"); configured actions get their assigned quick-pick
        // letter (blank when the alphabet ran out — j/k still reaches it).
        const prefix = isCustom ? "c" : item.key || " ";
        const prefixFg = isCustom
          ? theme.accent
          : blocked
            ? theme.fgDim
            : theme.accent;
        // Blocked actions: dim label even when selected. Mirrors the
        // disabled-but-discoverable convention used for grayed sections
        // elsewhere — entry stays visible (so the user knows it exists)
        // but reads as inactive at a glance.
        const fg = blocked
          ? theme.fgDim
          : selected
            ? theme.fgBright
            : theme.fg;
        const labelFg = isCustom ? theme.accent : fg;
        const label = isCustom ? "Custom prompt…" : item.def.name;
        // Trailing hint: a kind/target marker plus the action id. `$` for
        // shell commands; the Claude robot glyph for claude prompts (two
        // spaces: the nerd-font glyph renders wide and reads cramped with
        // one). Session-target claude actions add a `↪` to mark that they
        // inject into the live F12 session instead of spawning a headless
        // tracked headless run. All stay muted like the id. Unavailable items
        // show the block reason instead; the custom entry shows "freeform".
        const hint = isCustom
          ? "freeform"
          : blocked
            ? `(${(item.availability as { reason: string }).reason})`
            : item.def.kind === "shell"
              ? `$ ${item.def.id}`
              : item.def.target === "session"
                ? `${claudeGlyph}  ↪ ${item.def.id}`
                : `${claudeGlyph}  ${item.def.id}`;
        return (
          <Fragment key={isCustom ? "__custom__" : item.def.id}>
            {showHeader ? (
              <box flexDirection="row" paddingLeft={1} marginTop={i > 0 ? 1 : 0}>
                <text fg={theme.fgDim} attributes={1} wrapMode="none" truncate>
                  {group}
                </text>
              </box>
            ) : null}
            <box
              id={rowId(item)}
              flexDirection="row"
              backgroundColor={bg}
              paddingLeft={1}
              paddingRight={1}
            >
              <text fg={selected ? theme.accent : theme.fgDim}>
                {selected ? "▸ " : "  "}
              </text>
              <box width={2} flexShrink={0}>
                <text fg={prefixFg}>{prefix}</text>
              </box>
              <box flexGrow={1} flexShrink={1} overflow="hidden">
                <text fg={labelFg} wrapMode="none" truncate>
                  {label}
                </text>
              </box>
              <text fg={theme.fgDim} wrapMode="none">
                {hint}
              </text>
            </box>
          </Fragment>
        );
      })}
      </ScrollableList>
    </Modal>
  );
}

type EditProps = {
  slug: string;
  /** `null` = custom prompt (extras IS the entire prompt). */
  def: ClaudeActionDef | null;
  extras: string;
  /**
   * Substitutions for `{{name}}` in `def.prompt`. Mirrors what gets
   * applied at launch, so the preview matches what claude actually
   * receives.
   */
  vars: ActionVars;
};

export function ActionEditModal({ slug, def, extras, vars }: EditProps) {
  const title = def ? `action · ${def.name} · ${slug}` : `action · custom · ${slug}`;
  const renderedPrompt = def ? applyVars(def.prompt, vars) : "";
  return (
    <Modal
      title={title}
      inset={{ top: "8%", right: "12%", bottom: "8%", left: "12%" }}
      hints={[
        ["⏎", "launch"],
        ["esc", def ? "back" : "cancel"],
        ["^C", "cancel"],
      ]}
    >
      {/* A long rendered prompt plus freeform extras can outgrow the
          modal; scroll the region and keep the input (always at the end
          of `extras`) in view as the user types. */}
      <ScrollableList selectedId="edit:input" revision={extras}>
        {def ? (
          <box flexDirection="column" marginBottom={1}>
            <text fg={theme.fgDim} attributes={1}>
              prompt
            </text>
            <box flexDirection="column" marginTop={0}>
              {renderedPrompt.split("\n").map((line, i) => (
                <text key={i} fg={theme.fg} wrapMode="word">
                  {line || " "}
                </text>
              ))}
            </box>
          </box>
        ) : null}
        <box id="edit:input" flexDirection="column">
          <text fg={theme.fgDim} attributes={1}>
            {def ? "additional instructions" : "prompt"}
          </text>
          <box flexDirection="column" marginTop={0}>
            {extras.length === 0 ? (
              <text fg={theme.fgDim}>
                <span fg={theme.accent}>█</span>
                {def
                  ? " (optional — type to append, ⏎ to launch)"
                  : " (type your prompt, ⏎ to launch)"}
              </text>
            ) : (
              <ExtrasView text={extras} />
            )}
          </box>
        </box>
      </ScrollableList>
    </Modal>
  );
}

function ExtrasView({ text }: { text: string }) {
  const lines = text.split("\n");
  return (
    <>
      {lines.map((line, i) => {
        const isLast = i === lines.length - 1;
        return (
          <text key={i} fg={theme.fgBright} wrapMode="word">
            {line}
            {isLast ? <span fg={theme.accent}>█</span> : null}
          </text>
        );
      })}
    </>
  );
}
