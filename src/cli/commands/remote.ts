import { config } from "../../core/config.ts";
import { runRemoteWt } from "../../core/remote.ts";
import { setWezTermTabTitle } from "../../core/wezterm.ts";
import { NF } from "../../tui/icons.ts";
import { red } from "../colors.ts";

export async function run(argv: string[]): Promise<number> {
  const remote = config.remote;
  if (!remote) {
    console.error(red("[remote] is not configured in config.toml"));
    return 1;
  }
  if (argv.length === 0 && (!process.stdin.isTTY || !process.stdout.isTTY)) {
    console.error(red("interactive remote wt requires a TTY"));
    return 2;
  }
  const interactive = argv.length === 0;
  if (interactive) {
    await setWezTermTabTitle(`${NF.remote} ${remote.label} · wt`, config.paths.weztermCli);
  }
  try {
    return await runRemoteWt(remote, argv, {
      interactive,
      onLine: interactive ? undefined : (line) => console.log(line),
    });
  } finally {
    if (interactive) {
      await setWezTermTabTitle("wt", config.paths.weztermCli);
    }
  }
}
