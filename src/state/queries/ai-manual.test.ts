import { expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

test.each([false, true])("naming observers respect auto_rename = %s", (automatic) => {
  const root = mkdtempSync(join(tmpdir(), "wt-manual-naming-"));
  try {
    const configPath = join(root, ".wt.toml");
    writeFileSync(configPath, `
[paths]
main_clone = "${root}"
worktree_root = "${root}/worktrees"
[branch]
prefix = "test"
base = "main"
[naming]
auto_rename = ${automatic}
`);
    const source = pathToFileURL(join(import.meta.dir, "ai.ts")).href;
    const script = `
      import { QueryClient, QueryObserver } from ${JSON.stringify(import.meta.resolve("@tanstack/react-query"))};
      import { aiSummaryQuery } from ${JSON.stringify(source)};
      const client = new QueryClient();
      let calls = 0;
      const summary = { title: "Fixed name", description: "Fixed description." };
      const first = aiSummaryQuery("selected", { hash: "first", prompt: "first diff" });
      const observer = new QueryObserver(client, { ...first, queryFn: async () => { calls++; return summary; } });
      const unsubscribe = observer.subscribe(() => {});
      await Promise.resolve();
      const initialCalls = calls;
      await client.fetchQuery({ ...first, queryFn: async () => { calls++; return summary; } });
      const second = aiSummaryQuery("selected", { hash: "second", prompt: "second diff" });
      observer.setOptions({ ...second, queryFn: async () => { calls++; return summary; } });
      await client.invalidateQueries();
      console.log(JSON.stringify({ initialCalls, calls, data: observer.getCurrentResult().data,
        firstKey: first.queryKey, secondKey: second.queryKey, otherKey: aiSummaryQuery("other", null).queryKey }));
      unsubscribe(); client.clear();
    `;
    const result = Bun.spawnSync([process.execPath, "-e", script], {
      cwd: root,
      env: { ...process.env, WT_CONFIG: configPath, WT_REPO_CONFIG: configPath },
      stdout: "pipe", stderr: "pipe",
    });
    expect(result.exitCode).toBe(0);
    const output = JSON.parse(result.stdout.toString());
    expect(output.initialCalls).toBe(automatic ? 1 : 0);
    expect(output.data.title).toBe("Fixed name");
    if (automatic) {
      expect(output.firstKey).toEqual(["aiSummary", "first"]);
      expect(output.secondKey).toEqual(["aiSummary", "second"]);
    } else {
      expect(output.calls).toBe(1);
      expect(output.firstKey).toEqual(output.secondKey);
      expect(output.otherKey).not.toEqual(output.firstKey);
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
