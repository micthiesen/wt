import { homedir } from "node:os";

import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

const HOME = homedir();

function tildify(p: string): string {
  return p === HOME || p.startsWith(`${HOME}/`) ? `~${p.slice(HOME.length)}` : p;
}

export const pathRow: RowModule = {
  id: "path",
  label: "path",
  render: ({ row }) => (
    <text fg={theme.fg} wrapMode="none" truncate>
      {tildify(row.wt.path)}
    </text>
  ),
};
