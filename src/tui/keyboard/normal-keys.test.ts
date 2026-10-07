import { expect, test } from "bun:test";
import type { KeyEvent } from "@opentui/core";
import { setImmediate as settle } from "node:timers/promises";

import type { PullRequest } from "../../core/types.ts";
import type { ReviewRequestPr } from "../../core/github.ts";
import type { WorktreeModel } from "../worktree-model.ts";
import { handleNormalKey, type NormalKeysCtx } from "./normal-keys.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";

const plainKey = (name: string): KeyEvent =>
  ({
    name,
    sequence: name,
    raw: name,
    ctrl: false,
    meta: false,
    shift: false,
    option: false,
    number: false,
    eventType: "press",
    source: "raw",
  }) as KeyEvent;

function remoteModel(pr?: PullRequest, archived = false): WorktreeModel {
  const target = {
    ref: { kind: "remote", host: "dellserver", slug: "remote-task" },
    slug: "remote-task",
    branch: "alex/remote-task",
    path: "/remote/remote-task",
    stage: "remote-task",
    location: {
      kind: "remote",
      endpoint: { host: "dellserver", label: "Dell server", wtPath: "~/bin/wt" },
    },
  } as const;
  return {
    target,
    source: { kind: "remote", row: { hostLabel: "dellserver" } },
    key: "remote:dellserver:remote-task",
    slug: target.slug,
    pr,
    archived,
  } as WorktreeModel;
}

function titleContext(overrides: Partial<NormalKeysCtx>): NormalKeysCtx {
  return {
    focusedOutputId: null,
    consumePrTargetChord: () => false,
    handleGlobalKey: () => false,
    current: { wt: { slug: "local-title" }, status: { kind: "clean" } } as WorktreeRow,
    ...overrides,
  } as NormalKeysCtx;
}

test("t opens manual title editing when AI naming is not configured", () => {
  let opened = 0;
  handleNormalKey(plainKey("t"), titleContext({
    namingConfigured: false,
    openWorktreeTitlePrompt: () => { opened++; },
    refreshAiSummary: async () => { throw new Error("must not generate"); },
    toast: () => { throw new Error("manual titles need no naming configuration"); },
  }));
  expect(opened).toBe(1);
});

test("Shift+T explicitly generates rather than editing and reports completion", async () => {
  const calls: string[] = [];
  handleNormalKey(Object.assign(plainKey("t"), { shift: true, sequence: "T" }), titleContext({
    namingConfigured: true,
    openWorktreeTitlePrompt: () => { throw new Error("must not open editor"); },
    refreshAiSummary: async (slug) => { calls.push(slug); return true; },
    toast: (message) => { calls.push(message); },
    reportActionError: (_label, error) => { throw error; },
  }));
  await settle();
  expect(calls).toEqual(["local-title", "generated worktree title"]);
});

test("remote title keys never edit or generate a same-named local title", () => {
  const messages: string[] = [];
  const ctx = titleContext({
    selectedRemote: { hostLabel: "dellserver" } as NormalKeysCtx["selectedRemote"],
    selectedWorktree: remoteModel(),
    openWorktreeTitlePrompt: () => { throw new Error("must not edit local state"); },
    refreshAiSummary: async () => { throw new Error("must not generate locally"); },
    toast: (message) => messages.push(message),
  });
  handleNormalKey(plainKey("t"), ctx);
  handleNormalKey(Object.assign(plainKey("t"), { shift: true, sequence: "T" }), ctx);
  expect(messages).toEqual([
    "edit or generate this title in wt on its remote host",
    "edit or generate this title in wt on its remote host",
  ]);
});

test("p opens the selected remote worktree PR", () => {
  const opened: Array<{ url: string; number: number; logName: string }> = [];
  const remotePr = {
    url: "https://github.com/example/repo/pull/1515",
    number: 1515,
  } as PullRequest;
  const ctx = {
    focusedOutputId: null,
    consumePrTargetChord: () => false,
    handleGlobalKey: () => false,
    current: undefined,
    currentItem: undefined,
    selectedPr: undefined,
    selectedRemote: { hostLabel: "dellserver" },
    selectedWorktree: remoteModel(remotePr),
    selectedSection: undefined,
    openPrUrl: (url: string, number: number, _target: null, logName: string) => {
      opened.push({ url, number, logName });
    },
  } as unknown as NormalKeysCtx;

  handleNormalKey(plainKey("p"), ctx);

  expect(opened).toEqual([
    {
      url: remotePr.url,
      number: remotePr.number,
      logName: "remote-task",
    },
  ]);
});

test("l starts the Linear PR-target chord for a remote worktree", () => {
  const targets: string[] = [];
  const ctx = {
    focusedOutputId: null,
    consumePrTargetChord: () => false,
    handleGlobalKey: () => false,
    current: undefined,
    currentItem: undefined,
    selectedPr: undefined,
    selectedRemote: { hostLabel: "dellserver" },
    selectedWorktree: remoteModel(),
    selectedSection: undefined,
    rememberPrTargetChord: (target: string) => {
      targets.push(target);
      return true;
    },
    openSectionPicker: () => {},
  } as unknown as NormalKeysCtx;

  handleNormalKey(plainKey("l"), ctx);

  expect(targets).toEqual(["linear"]);
});

test("d dismisses a review request and advances the cursor first", async () => {
  const calls: string[] = [];
  const request = {
    url: "https://github.com/example/repo/pull/1515",
    number: 1515,
    updatedAt: "2026-09-09T10:00:00Z",
  } as ReviewRequestPr;
  const item = { kind: "pr" as const, pr: request };
  const ctx = {
    focusedOutputId: null,
    consumePrTargetChord: () => false,
    handleGlobalKey: () => false,
    current: undefined,
    currentItem: item,
    selectedPr: request,
    selectedRemote: undefined,
    selectedWorktree: undefined,
    selectedSection: undefined,
    advanceCursorPast: (keys: readonly string[]) => calls.push(`advance:${keys[0]}`),
    dismissReviewRequest: async (url: string, updatedAt: string) => {
      calls.push(`dismiss:${url}:${updatedAt}`);
    },
    toast: (message: string) => calls.push(`toast:${message}`),
    reportActionError: (label: string, error: unknown) => {
      throw new Error(`${label}: ${String(error)}`);
    },
  } as unknown as NormalKeysCtx;

  handleNormalKey(plainKey("d"), ctx);
  await Bun.sleep(0);

  expect(calls).toEqual([
    `advance:pr:${request.url}`,
    `dismiss:${request.url}:${request.updatedAt}`,
    "toast:review request dismissed",
  ]);
});

test("a folds Archived after archiving a remote worktree", async () => {
  const folds: Array<[string, boolean]> = [];
  const ctx = {
    focusedOutputId: null,
    consumePrTargetChord: () => false,
    handleGlobalKey: () => false,
    current: undefined,
    currentItem: undefined,
    selectedPr: undefined,
    selectedRemote: {
      hostKey: "ssh://dellserver/repo",
      hostLabel: "dellserver",
      slug: "remote-task",
    },
    selectedRemotePr: undefined,
    selectedWorktree: remoteModel(),
    selectedSection: undefined,
    currentTarget: null,
    toggleArchived: async () => ({ archived: true }),
    setSectionFolded: async (key: string, folded: boolean) => {
      folds.push([key, folded]);
      return folded;
    },
    setSel: () => {},
    toast: () => {},
    reportActionError: (label: string, err: unknown) => {
      throw new Error(`${label}: ${String(err)}`);
    },
  } as unknown as NormalKeysCtx;

  handleNormalKey(plainKey("a"), ctx);
  await Bun.sleep(0);

  expect(folds).toEqual([["\0archived", true]]);
});

test("a restores a remote worktree to Inbox through the shared mutation", async () => {
  const sections: Array<[string, string | null]> = [];
  const model = remoteModel(undefined, true);
  const ctx = {
    focusedOutputId: null,
    consumePrTargetChord: () => false,
    handleGlobalKey: () => false,
    current: undefined,
    currentItem: undefined,
    selectedPr: undefined,
    selectedRemote: { hostLabel: "dellserver", slug: model.slug },
    selectedWorktree: model,
    selectedSection: undefined,
    toggleArchived: async () => ({ archived: false }),
    setWorktreeSection: async (target: WorktreeModel["target"], section: string | null) => {
      sections.push([target.slug, section]);
    },
    setSel: () => {},
    toast: () => {},
    reportActionError: (label: string, err: unknown) => {
      throw new Error(`${label}: ${String(err)}`);
    },
  } as unknown as NormalKeysCtx;

  handleNormalKey(plainKey("a"), ctx);
  await Bun.sleep(0);

  expect(sections).toEqual([["remote-task", null]]);
});

test("async row action failures use the normal action error channel", async () => {
  const reported: Array<{ label: string; error: unknown }> = [];
  const ctx = {
    focusedOutputId: null,
    consumePrTargetChord: () => false,
    handleGlobalKey: () => false,
    current: undefined,
    currentItem: undefined,
    selectedPr: undefined,
    selectedRemote: { hostLabel: "dellserver" },
    selectedWorktree: remoteModel(),
    selectedSection: undefined,
    toggleArchived: async () => { throw new Error("write failed"); },
    setSel: () => {},
    toast: () => {},
    reportActionError: (label: string, error: unknown) => reported.push({ label, error }),
  } as unknown as NormalKeysCtx;

  handleNormalKey(plainKey("a"), ctx);
  await Bun.sleep(0);

  expect(reported[0]?.label).toBe("archive");
  expect(reported[0]?.error).toBeInstanceOf(Error);
});

test("! opens the same action picker for a remote worktree", () => {
  const opened: unknown[] = [];
  const target = {
    ref: { kind: "remote", host: "dellserver", slug: "remote-task" },
    slug: "remote-task",
    branch: "alex/remote-task",
    path: "/remote/remote-task",
    stage: "remote-task",
    location: {
      kind: "remote",
      endpoint: { host: "dellserver", label: "Dell server", wtPath: "~/bin/wt" },
    },
  } as const;
  const ctx = {
    focusedOutputId: null,
    consumePrTargetChord: () => false,
    handleGlobalKey: () => false,
    current: undefined,
    currentItem: undefined,
    currentTarget: target,
    selectedPr: undefined,
    selectedRemote: { hostLabel: "Dell server" },
    selectedRemotePr: undefined,
    selectedWorktree: remoteModel(),
    selectedSection: undefined,
    openActionPicker: (picked: unknown) => opened.push(picked),
  } as unknown as NormalKeysCtx;

  handleNormalKey({ ...plainKey("!"), sequence: "!" } as KeyEvent, ctx);

  expect(opened).toEqual([target]);
});

test("F12 queues one agent session on a remote creation row", () => {
  const messages: string[] = [];
  const creation = {
    remote: { host: "worker", label: "Worker", wtPath: "~/bin/wt" },
    hostKey: "worker",
    hostLabel: "Worker",
    input: "new-task",
    previousKeys: [],
    status: "creating" as const,
  };
  const ctx = {
    focusedOutputId: null,
    consumePrTargetChord: () => false,
    handleGlobalKey: () => false,
    current: undefined,
    selectedWorktree: undefined,
    selectedRemote: creation,
    primaryHarness: "codex",
    toast: (message: string) => messages.push(message),
    doEnterWorktreeSession: () => { throw new Error("checkout is not ready"); },
  } as unknown as NormalKeysCtx;
  handleNormalKey(plainKey("f12"), ctx);
  handleNormalKey(plainKey("f12"), ctx);
  expect((creation as { requestedHarness?: string }).requestedHarness).toBe("codex");
  expect(messages).toEqual([
    "agent session will open when creation finishes",
    "agent session will open when creation finishes",
  ]);
});
