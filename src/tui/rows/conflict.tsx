import { pluralize } from "../../core/text.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

/**
 * Rebase-conflict pre-flight (a `git merge-tree` dry-run of HEAD against
 * the effective base). Only rendered when the merge actually conflicts —
 * a clean or indeterminate result hides the row entirely, so the details
 * pane stays quiet unless there's something to fix. The list-pane glyph
 * is the ambient signal; this row is the "what it clashes with and
 * where". Names the base and the conflicting files, truncated to fit.
 *
 * merge-tree is a merge, not a rebase replay, so for a multi-commit
 * branch this is a strong hint, not a guarantee — `/restack` (or `R`) is
 * still what actually resolves it.
 */
export const conflictRow: RowModule = {
  id: "conflict",
  label: "rebase",
  sources: ({ row }) => [row.fields.conflict],
  visible: ({ row }) => row.fields.conflict.data?.status === "conflict",
  render: ({ row }) => {
    const data = row.fields.conflict.data;
    if (data?.status !== "conflict") return null;
    // Friendly base: drop the `origin/` prefix on trunk.
    const base = data.base.replace(/^origin\//, "");
    const n = data.files.length;
    const detail =
      n > 0 ? `${base} · ${pluralize(n, "file")}: ${data.files.join(", ")}` : base;
    return (
      <text fg={theme.err} wrapMode="none" truncate>
        {`${NF.conflict}  conflicts with ${detail}`}
      </text>
    );
  },
};
