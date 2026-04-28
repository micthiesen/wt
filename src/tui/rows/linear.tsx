import { config } from "../../core/config.ts";
import { linearUrlForSlug } from "../../core/linear.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

export const linearRow: RowModule = {
  id: "linear",
  label: "linear",
  // No workspace configured = no Linear integration. Hide the row
  // rather than render a permanent "—".
  visible: () => config.linear !== null,
  render: ({ row }) => {
    const url = linearUrlForSlug(row.wt.slug);
    return url ? (
      <text fg={theme.accentAlt} wrapMode="none" truncate>
        {url}
      </text>
    ) : (
      <text fg={theme.fgDim}>—</text>
    );
  },
};
