import {
  issueUrlForId,
  resolveIssueId,
  preferredIssueUrl,
} from "../../core/issue-tracker.ts";
import { stageUrl } from "../../core/stage.ts";
import { Modal } from "../modal.tsx";
import { ScrollableList } from "./scroll-list.tsx";
import { theme } from "../theme.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import type { SelectedSection } from "../hooks/useVisualItems.ts";
import { isRemoteSummary, remoteEntryLabel } from "../remote-creation.ts";

export type Item = { key: string; label: string; value: string | null };

type Props = { items: readonly Item[]; selectedIndex: number };

/**
 * `padEnd` budget for the label column, one char past the longest
 * label ("stage url", 9) so there's always a gap before the value.
 */
const LABEL_WIDTH = 10;

/**
 * Items the `y` chord can yank. Order matches their key letters; the
 * modal renders this list verbatim. A null `value` shows as a dim "—"
 * and committing it (Enter/digit/chord) errors with a "nothing to
 * yank" toast instead of copying the placeholder.
 */
export function yankItemsFor(row: WorktreeRow): Item[] {
  const stageUrlValue =
    row.fields.deploy.data === true ? stageUrl(row.wt.stage) : null;
  const dev = row.fields.dev.data;
  const prUrlValue = row.pr ? row.pr.url : null;
  return [
    { key: "b", label: "branch", value: row.wt.branch || null },
    { key: "s", label: "stage", value: row.wt.stage },
    { key: "S", label: "stage url", value: stageUrlValue },
    { key: "d", label: "dev url", value: dev?.running ? dev.url : null },
    { key: "p", label: "path", value: row.wt.path },
    { key: "n", label: "slug", value: row.wt.slug },
    // `i` mirrors the open key: tracker URL first, then an attached
    // GitHub issue, then the bare resolved id. `I` is always
    // the primary tracker issue.
    {
      key: "i",
      label: "issue",
      value:
        preferredIssueUrl(row.wt.slug, row.githubIssue, row.issueId) ??
        resolveIssueId(row.wt.slug, row.issueId),
    },
    {
      key: "I",
      label: "primary",
      value:
        issueUrlForId(resolveIssueId(row.wt.slug, row.issueId)) ??
        resolveIssueId(row.wt.slug, row.issueId),
    },
    { key: "r", label: "pr url", value: prUrlValue },
  ];
}

/**
 * What the `y` chord can yank off a SECTION header — the batch, not a
 * row. These exist because a section is how the human talks about work
 * to the manager ("Blocked: Real Dev Env is waiting on you"), and
 * naming its members meant reading them off the screen and retyping
 * slugs by hand.
 *
 * `slugs` and `branches` are space-joined so they paste straight into a
 * command line; `list` is the prose form, name first, one member per
 * line. An empty section yields nulls rather than an empty string, so
 * committing one toasts "nothing to yank" instead of silently clearing
 * the clipboard.
 */
export function sectionYankItems(section: SelectedSection): Item[] {
  const members = section.members.map((member) =>
    member.kind === "wt"
      ? {
          slug: member.row.wt.slug,
          branch: member.row.wt.branch,
          title: member.row.title,
        }
      : {
          slug: remoteEntryLabel(member.entry),
          branch: isRemoteSummary(member.entry) ? member.entry.branch : null,
          title: remoteEntryLabel(member.entry),
        },
  );
  const join = (values: readonly (string | null)[]): string | null => {
    const present = values.filter((v): v is string => Boolean(v));
    return present.length > 0 ? present.join(" ") : null;
  };
  const list =
    members.length > 0
      ? [
          `${section.label} (${members.length})`,
          ...members.map((member) => `- ${member.slug}: ${member.title}`),
        ].join("\n")
      : null;
  return [
    { key: "n", label: "name", value: section.label || null },
    { key: "s", label: "slugs", value: join(members.map((member) => member.slug)) },
    { key: "b", label: "branches", value: join(members.map((member) => member.branch || null)) },
    { key: "l", label: "list", value: list },
  ];
}

/** Index of the first row with a real value, for the modal's initial cursor. */
export function firstYankIndex(items: readonly Item[]): number {
  const i = items.findIndex((it) => it.value !== null);
  return i === -1 ? 0 : i;
}

export function YankModal({ items, selectedIndex }: Props) {
  return (
    <Modal
      title="yank · pick what to copy"
      inset={{ top: "25%", right: "20%", bottom: "20%", left: "20%" }}
      hints={[
        ["j/k", "move"],
        ["1-9", "quick pick"],
        ["letter", "direct"],
        ["y / ⏎", "pick"],
        ["esc / q", "cancel"],
      ]}
    >
      <ScrollableList
        selectedId={items[selectedIndex] ? `yank:${items[selectedIndex]!.key}` : undefined}
        revision={items}
      >
        {items.map((it, i) => {
          const selected = i === selectedIndex;
          const bg = selected ? theme.rowSelectedBg : undefined;
          return (
            <box
              id={`yank:${it.key}`}
              key={it.key}
              flexDirection="row"
              backgroundColor={bg}
              paddingLeft={1}
              paddingRight={1}
            >
              {/* Cursor + chord + label as one text node (spans, padEnd
                  alignment), wrapped in a flexShrink={0} box so it
                  never gives up width to the value column — without
                  it, long values pressure the row and eat the padEnd
                  padding first (row-cell.tsx's Row applies the same
                  flexShrink={0} to its label box for the same reason).
                  Only the value gets flexShrink/overflow="hidden" —
                  the part that must clip instead of pushing the row
                  wider than the modal. */}
              <box flexShrink={0}>
                <text wrapMode="none">
                  <span fg={selected ? theme.accent : theme.fgDim}>
                    {selected ? "▸ " : "  "}
                  </span>
                  <span fg={theme.accent} attributes={1}>
                    {it.key.padEnd(2)}
                  </span>
                  <span fg={selected ? theme.fgBright : theme.fg}>
                    {it.label.padEnd(LABEL_WIDTH)}
                  </span>
                </text>
              </box>
              <box flexShrink={1} overflow="hidden">
                {/* Preview only: a multi-line value (the section's
                    member list) is flattened, because a real newline
                    in a `wrapMode="none"` text grows the row and
                    collides with the one below it. The clipboard gets
                    the unflattened value. */}
                <text fg={theme.fgDim} wrapMode="none" truncate>
                  {it.value?.replace(/\n/g, " · ") ?? "—"}
                </text>
              </box>
            </box>
          );
        })}
      </ScrollableList>
    </Modal>
  );
}
