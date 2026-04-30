import { linearUrlForSlug } from "../../core/linear.ts";
import { stageUrl } from "../../core/stage.ts";
import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";

type Item = { key: string; label: string; value: string | null };

type Props = { row: WorktreeRow };

/**
 * Items the `y` chord can yank. Order matches their key letters; the
 * modal renders this list verbatim. A null `value` shows as a dim "—"
 * and the keystroke errors with a "nothing to yank" toast.
 */
export function yankItemsFor(row: WorktreeRow): Item[] {
  const stageUrlValue =
    row.fields.deploy.data === true ? stageUrl(row.wt.stage) : null;
  return [
    { key: "b", label: "branch", value: row.wt.branch || null },
    { key: "s", label: "stage", value: row.wt.stage },
    { key: "S", label: "stage url", value: stageUrlValue },
    { key: "p", label: "path", value: row.wt.path },
    { key: "n", label: "slug", value: row.wt.slug },
    { key: "i", label: "issue url", value: linearUrlForSlug(row.wt.slug) },
  ];
}

export function YankModal({ row }: Props) {
  const items = yankItemsFor(row);
  return (
    <Modal
      title="yank · pick what to copy"
      inset={{ top: "35%", right: "20%", bottom: "40%", left: "20%" }}
      hints={[["esc / q / y", "cancel"]]}
    >
      {items.map((it) => (
        <box key={it.key} flexDirection="row">
          <box width={3} flexShrink={0}>
            <text fg={theme.accent} attributes={1}>
              {it.key}
            </text>
          </box>
          <box width={11} flexShrink={0}>
            <text fg={theme.fg}>{it.label}</text>
          </box>
          <box flexShrink={1} overflow="hidden">
            <text
              fg={it.value ? theme.fgDim : theme.warn}
              wrapMode="none"
              truncate
            >
              {it.value ?? "—"}
            </text>
          </box>
        </box>
      ))}
    </Modal>
  );
}
