/**
 * Built-in row modules. The set is fixed at compile time; users
 * select a subset and order via `[ui] rows = [...]` in `wt.toml`.
 *
 * Adding a new row: write the module in this directory and append it
 * to `REGISTRY`. The default order in `config.ts` decides where it
 * sits when the user hasn't customized `ui.rows`.
 */
import { branchRow } from "./branch.tsx";
import { claudeRow } from "./claude.tsx";
import { gitRow } from "./git.tsx";
import { linearRow } from "./linear.tsx";
import { pathRow } from "./path.tsx";
import { prRow } from "./pr.tsx";
import { stageRow } from "./stage.tsx";
import type { RowModule } from "./types.ts";

const REGISTRY: readonly RowModule[] = [
  branchRow,
  pathRow,
  linearRow,
  stageRow,
  prRow,
  claudeRow,
  gitRow,
];

const BY_ID = new Map(REGISTRY.map((m) => [m.id, m]));

/**
 * Resolve configured ids to modules in the user's chosen order.
 * Unknown ids are dropped silently — strict validation belongs at
 * config-load time, not in the render path.
 */
export function resolveRows(ids: readonly string[]): RowModule[] {
  const out: RowModule[] = [];
  for (const id of ids) {
    const m = BY_ID.get(id);
    if (m) out.push(m);
  }
  return out;
}

export type { RowModule } from "./types.ts";
