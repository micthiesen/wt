import { expect, test } from "bun:test";
import { navigationBindings, targetIdentity, workspaceConfig, type WorkspaceTarget } from "./workspace.ts";
const target: WorkspaceTarget = { slug: "task", cwd: "/tmp/task", initial: "harness", diffBase: "main", harness: { harnessId: "codex", resumeSessionId: "one" } };
test("identity separates host, harness, conversation and target kind", () => {
  const variants = [target, { ...target, remote: { host: "worker", label: "Worker", wtPath: "wt" } },
    { ...target, harness: { harnessId: "claude" as const } },
    { ...target, harness: { ...target.harness, resumeSessionId: "two" } },
    { ...target, initial: "shell" as const }];
  expect(new Set(variants.map(targetIdentity)).size).toBe(variants.length);
  expect(targetIdentity({ ...target })).toBe(targetIdentity(target));
});
test("copies only plain prefix and directional pane-navigation bindings", () => {
  expect(navigationBindings(`set-option -g prefix C-Space
bind-key C-h select-pane -L
bind-key -n M-l select-pane -R
source-file ~/.tmux-extra.conf
run-shell 'something'
bind-key C-x select-pane -L ; kill-server
set -g base-index 1`)).toEqual([
    "set-option -g prefix C-Space", "bind-key C-h select-pane -L", "bind-key -n M-l select-pane -R",
  ]);
});
test("workspace F-keys focus the explorer and do not detach inner sessions", () => {
  const conf = workspaceConfig([]);
  expect(conf).toContain("select-pane -t %0");
  expect(conf).toContain("send-keys F12");
  expect(conf).not.toContain("detach-client");
});

test("a live primary activation focuses a displayed resumed conversation", async () => {
  const { sameDisplayedTarget } = await import("./workspace.ts");
  const live = { ...target, harness: { harnessId: "codex" as const } };
  expect(sameDisplayedTarget(live, target)).toBe(true);
  expect(sameDisplayedTarget({ ...target, harness: { ...target.harness, resumeSessionId: "other" } }, target)).toBe(false);
  expect(sameDisplayedTarget({ ...live, harness: { ...live.harness, freshSlot: true } }, target)).toBe(false);
  expect(sameDisplayedTarget({ ...live, initial: "shell" }, target)).toBe(false);
});
