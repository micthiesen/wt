import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

export const branchRow: RowModule = {
  id: "branch",
  label: "branch",
  render: ({ row }) => (
    <text fg={theme.fg} wrapMode="none" truncate>
      {row.wt.branch || "(none)"}
    </text>
  ),
};
