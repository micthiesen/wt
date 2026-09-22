import { describe, expect, test } from "bun:test";

import type { AutomationDef } from "../core/config.ts";
import { StatusKind } from "../core/types.ts";
import { cancellableAutomationFires } from "./automation-queue.ts";
import { evaluateAutomations, type AutomationEvalCtx } from "./automation-rules.ts";
import type { WorktreeRow } from "./hooks/useWorktreeRows.ts";

const rule: AutomationDef = {
  id: "address-review",
  on: "review_bot.unresolved",
  run: "address-review",
  busy: "queue",
  cooldownMinutes: null,
  afterDays: 2,
  settleSeconds: 120,
  branch: null,
};

const evalCtx: AutomationEvalCtx = {
  githubFresh: true,
  isPausedSlug: () => false,
  audienceOf: () => null,
  externalOf: () => false,
  varsFor: () => ({} as never),
  branchTips: new Map(),
  nowMs: 0,
};

function row(slug: string, head = "sha1"): WorktreeRow {
  return {
    wt: { slug, branch: `topic/${slug}`, path: `/tmp/${slug}`, isMain: false },
    status: { kind: StatusKind.Clean, label: "clean" },
    archived: false,
    pr: {
      number: 1,
      state: "OPEN",
      headRefOid: head,
      reviewBot: { state: "unresolved", unresolved: 1 },
    },
  } as WorktreeRow;
}

function options() {
  return {
    paused: true,
    stateReady: true,
    rules: [rule],
    rows: [row("a"), row("b")],
    evalCtx,
    pending: [],
    executing: new Set<string>(),
    handled: (_key: string) => false,
  };
}

describe("paused automation cancellation", () => {
  test("reconstructs eligible unseen instances after pause dropped the memory queue", () => {
    const opts = options();
    const fires = cancellableAutomationFires(opts);
    expect(fires.map((fire) => fire.fireKeys)).toEqual([
      ["address-review:rabbit:a:sha1"],
      ["address-review:rabbit:b:sha1"],
    ]);
    expect(opts.pending).toEqual([]);
    expect(opts.paused).toBe(true);
    const handled = new Set(fires.flatMap((fire) => fire.fireKeys));
    expect(cancellableAutomationFires({ ...opts, handled: (key) => handled.has(key) })).toEqual([]);
    expect(cancellableAutomationFires({
      ...opts,
      rows: [row("a", "sha2")],
      handled: (key) => handled.has(key),
    })[0]?.fireKeys).toEqual(["address-review:rabbit:a:sha2"]);
  });

  test("never reconstructs before pause flags or live GitHub data are known", () => {
    expect(cancellableAutomationFires({ ...options(), stateReady: false })).toEqual([]);
    expect(cancellableAutomationFires({
      ...options(),
      evalCtx: { ...evalCtx, githubFresh: false },
    })).toEqual([]);
  });

  test("preserves per-worktree pause and ineligible-row guards", () => {
    expect(cancellableAutomationFires({
      ...options(),
      rows: [
        row("paused"),
        { ...row("archived"), archived: true },
        { ...row("busy"), status: { kind: StatusKind.Busy, label: "destroying" } },
      ],
      evalCtx: { ...evalCtx, isPausedSlug: (slug) => slug === "paused" },
    })).toEqual([]);
  });

  test("excludes executing identities and already handled keys", () => {
    expect(cancellableAutomationFires({
      ...options(),
      executing: new Set(["address-review|a"]),
      handled: (key) => key === "address-review:rabbit:b:sha1",
    })).toEqual([]);
  });

  test("unpaused clear cancels only its memory queue, including rowless fires", () => {
    const pending = evaluateAutomations([rule], [row("gone")], evalCtx);
    const fires = cancellableAutomationFires({ ...options(), paused: false, pending });
    expect(fires.map((fire) => fire.slug)).toEqual(["gone"]);
    expect(cancellableAutomationFires({ ...options(), paused: false })).toEqual([]);
  });

  test("dedupes memory and reconstructed instances while preserving both exact key sets", () => {
    const pending = evaluateAutomations([rule], [row("a", "prior-head")], evalCtx);
    const fires = cancellableAutomationFires({ ...options(), rows: [row("a")], pending });
    expect(fires).toHaveLength(1);
    expect(fires[0]?.fireKeys).toEqual([
      "address-review:rabbit:a:prior-head",
      "address-review:rabbit:a:sha1",
    ]);
    expect(pending[0]?.fireKeys).toEqual(["address-review:rabbit:a:prior-head"]);
  });

  test("a multi-key pending fire cancels only unseen keys", () => {
    const fire = evaluateAutomations([rule], [row("a")], evalCtx)[0]!;
    const pending = [{ ...fire, fireKeys: ["delivered", "cancelled", "unseen"] }];
    const fires = cancellableAutomationFires({
      ...options(),
      paused: false,
      pending,
      handled: (key) => key !== "unseen",
    });
    expect(fires[0]?.fireKeys).toEqual(["unseen"]);
    expect(pending[0]?.fireKeys).toEqual(["delivered", "cancelled", "unseen"]);
  });
});
