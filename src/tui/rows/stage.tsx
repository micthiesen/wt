import { config } from "../../core/config.ts";
import { stageUrl } from "../../core/stage.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

export const stageRow: RowModule = {
  id: "stage",
  label: "stage",
  // No SST source configured = no preview-env concept. Hide.
  visible: () => config.sst !== null,
  sources: ({ row }) => [row.fields.deploy],
  render: ({ row }) => {
    const deployed = row.fields.deploy.data ?? false;
    const url = stageUrl(row.wt.stage);
    return deployed ? (
      // The stage name is the URL's subdomain, so the URL alone already
      // carries it — show just the URL and reclaim the space. Fall back to
      // the bare stage name when no domain is configured (url === null).
      <text fg={theme.warn} wrapMode="none" truncate>
        {NF.bolt}  {url ?? row.wt.stage}
      </text>
    ) : (
      <text fg={theme.fgDim} wrapMode="none" truncate>
        {NF.boltOff}  {row.wt.stage} (not deployed)
      </text>
    );
  },
};
