import { afterEach, describe, expect, test } from "bun:test";
import { appendFileSync, mkdirSync, mkdtempSync, rmSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { codexHarness, discoverCodexSessionsSync, latestRolloutForCwd } from "./harness.ts";
import { CODEX_MAIN_PROMPT, CODEX_MANAGER_PROMPT, codexRolloutBelongsToSlot, codexSlotFromPrefix } from "./slot.ts";

const dirs: string[] = [];
afterEach(() => { for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true }); });
const message = (role: string, text: string) => JSON.stringify({
  type: "response_item", payload: { type: "message", role, content: [{ type: "input_text", text }] },
}) + "\n";

function fixture() {
  const root = mkdtempSync(join(tmpdir(), "wt-codex-slot-"));
  dirs.push(root);
  const day = join(root, "2026", "09", "05");
  mkdirSync(day, { recursive: true });
  const cwd = join(root, "repo");
  function rollout(id: string, prompt: string) {
    const path = join(day, `rollout-${id}.jsonl`);
    writeFileSync(path, JSON.stringify({ type: "session_meta", payload: {
      id, cwd, originator: "codex-tui", thread_source: "user",
    } }) + "\n" + message("user", prompt) + message("assistant", "Ready."));
    return path;
  }
  return { root, cwd, rollout };
}

describe("Codex main/manager ownership", () => {
  test("same cwd, distinct discovery and output even when the other slot is newer", () => {
    const { root, cwd, rollout } = fixture();
    const main = rollout("main-id", "Fix the app.");
    const manager = rollout("manager-id", CODEX_MANAGER_PROMPT);
    utimesSync(main, 100, 100);
    utimesSync(manager, 200, 200);
    expect(discoverCodexSessionsSync("main", cwd, root).map(s => s.sessionId)).toEqual(["main-id"]);
    expect(discoverCodexSessionsSync("manager", cwd, root).map(s => s.sessionId)).toEqual(["manager-id"]);
    expect(latestRolloutForCwd(cwd, "main", root)?.path).toBe(main);
    expect(latestRolloutForCwd(cwd, "manager", root)?.path).toBe(manager);
    utimesSync(main, 300, 300);
    expect(latestRolloutForCwd(cwd, "manager", root)?.path).toBe(manager);
  });

  test("legacy unmarked conversations remain main-only", () => {
    const { root, cwd, rollout } = fixture();
    rollout("legacy-id", "$manager");
    expect(discoverCodexSessionsSync("manager", cwd, root)).toEqual([]);
    expect(discoverCodexSessionsSync("main", cwd, root).map(s => s.sessionId)).toEqual(["legacy-id"]);
  });

  test("a partial opening message is not cached as main", () => {
    const { rollout } = fixture();
    const path = rollout("partial", "unused");
    const opening = message("user", CODEX_MANAGER_PROMPT);
    writeFileSync(path, opening.slice(0, -8));
    expect(codexRolloutBelongsToSlot(path, statSync(path).size, "main")).toBe(false);
    appendFileSync(path, opening.slice(-8));
    expect(codexRolloutBelongsToSlot(path, statSync(path).size, "manager")).toBe(true);
  });

  test("quoted or later manager prompts cannot reassign a conversation", () => {
    expect(codexSlotFromPrefix(message("user", `Please review: ${CODEX_MANAGER_PROMPT}`) + message("assistant", "OK"))).toBe("main");
    expect(codexSlotFromPrefix(message("assistant", "OK") + message("user", CODEX_MANAGER_PROMPT))).toBe("main");
  });

  test("fresh shared-cwd slots get a stamp; resumes use the exact id", () => {
    const args = { wtPath: "/repo", managedName: null, resumeSessionId: null };
    expect(codexHarness.buildArgs({ ...args, slug: "manager" })).toEqual([
      "codex",
      CODEX_MANAGER_PROMPT,
    ]);
    expect(codexHarness.buildArgs({ ...args, slug: "main" })).toEqual([
      "codex",
      CODEX_MAIN_PROMPT,
    ]);
    expect(codexHarness.buildArgs({ ...args, slug: "worktree" })).toEqual(["codex"]);
    expect(codexHarness.buildArgs({
      ...args,
      slug: "manager",
      resumeSessionId: "manager-id",
    })).toEqual(["codex", "resume", "manager-id"]);
  });
});

test("wt-originated root sessions remain discoverable while child sessions stay hidden", () => {
  const { root, cwd } = fixture();
  const day = join(root, "2026", "09", "05");
  const write = (id: string, threadSource: string) => {
    writeFileSync(join(day, `rollout-${id}.jsonl`), `${JSON.stringify({
      type: "session_meta",
      payload: {
        id,
        cwd,
        originator: "wt",
        thread_source: threadSource,
      },
    })}\n${message("user", "Do the work.")}${message("assistant", "Ready.")}`);
  };
  write("root-id", "user");
  write("guardian-id", "subagent");

  expect(
    discoverCodexSessionsSync("originator-compat", cwd, root).map((session) => session.sessionId),
  ).toEqual(["root-id"]);
});

test("fresh main is identifiable before its response; worktrees need no stamp", () => {
  expect(codexSlotFromPrefix(message("user", CODEX_MAIN_PROMPT))).toBe("main");
  expect(codexRolloutBelongsToSlot("not-a-file", 0, "worktree")).toBe(true);
});

test("an exact live UUID restores a special session outside the picker window", () => {
  const root = mkdtempSync(join(tmpdir(), "wt-codex-old-live-"));
  dirs.push(root);
  const cwd = join(root, "repo");
  for (let day = 1; day <= 31; day++) {
    mkdirSync(join(root, "2026", "09", String(day).padStart(2, "0")), {
      recursive: true,
    });
  }
  const oldDay = join(root, "2025", "01", "01");
  mkdirSync(oldDay, { recursive: true });
  writeFileSync(
    join(oldDay, "rollout-old-manager.jsonl"),
    `${JSON.stringify({
      type: "session_meta",
      payload: {
        id: "old-manager",
        cwd,
        originator: "codex-tui",
        thread_source: "user",
      },
    })}\n${message("user", CODEX_MANAGER_PROMPT)}${message("assistant", "Ready.")}`,
  );

  expect(discoverCodexSessionsSync("manager", cwd, root)).toEqual([]);
  expect(
    discoverCodexSessionsSync("manager", cwd, root, "old-manager")
      .map((session) => session.sessionId),
  ).toEqual(["old-manager"]);
});
