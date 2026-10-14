import { test, expect } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createConnection } from "node:net";
import { createHash } from "node:crypto";
import wtPi from "./extension";

async function fixture(initialReceipts?: any[]) {
  const root = mkdtempSync(join(tmpdir(), "wt-pi-test-"));
  const socket = join(root, "control");
  const journal = `${socket}.receipts-${createHash("sha256").update("one").digest("hex")}`;
  if (initialReceipts) writeFileSync(journal, JSON.stringify(initialReceipts), { mode: 0o600 });
  const handlers = new Map<string, Function[]>();
  const sent: any[] = [];
  let compact: any;
  let busy = false;
  let sessionId = "one";
  const ctx = { cwd: "/fixture", sessionManager: { getSessionId: () => sessionId, getSessionFile: () => "/fixture/session" },
    isIdle: () => !busy, hasPendingMessages: () => busy, getContextUsage: () => ({ percent: 37 }),
    compact: (options: any) => { compact = options; } };
  wtPi({ registerFlag() {}, getFlag: (name: string) => name === "wt-control" ? socket : undefined,
    on(name: string, callback: Function) { handlers.set(name, [...handlers.get(name) ?? [], callback]); },
    sendUserMessage(text: string, options: any) { sent.push({ text, options }); } });
  for (const handler of handlers.get("session_start")!) await handler({}, ctx);
  async function request(value: any) {
    return await new Promise<any>((resolve, reject) => {
      const conn = createConnection(socket);
      let data = "";
      conn.on("connect", () => conn.write(JSON.stringify({ id: "a", cwd: "/fixture", session_id: sessionId, ...value }) + "\n"));
      conn.on("data", chunk => { data += chunk; });
      conn.on("end", () => { try { resolve(JSON.parse(data)); } catch (e) { reject(e); } });
      conn.on("error", reject);
    });
  }
  return { request, sent, root, socket, journal, busy: () => { busy = true; }, complete: () => compact.onComplete({}),
    async switchSession(id: string) { sessionId = id; for (const handler of handlers.get("session_start")!) await handler({}, ctx); },
    fail: () => compact.onError(Error("model unavailable")),
    close() { for (const handler of handlers.get("session_shutdown")!) handler(); rmSync(root, { recursive: true, force: true }); } };
}

test("identity pinned status and busy enqueue with skill expansion and no duplicate", async () => {
  const f = await fixture();
  try {
    expect((await f.request({ op: "status" })).result.context_percent).toBe(37);
    expect((await f.request({ op: "send", text: "wrong", session_id: "other" })).ok).toBe(false);
    f.busy();
    expect((await f.request({ op: "send", text: "/skill:start" })).result.status).toBe("accepted");
    await f.request({ op: "send", text: "/skill:start" });
    expect(f.sent).toEqual([{ text: "/skill:start", options: { deliverAs: "followUp", expandPromptTemplates: true } }]);
    expect(JSON.parse(readFileSync(f.journal, "utf8"))[0][1].status).toBe("accepted");
  } finally { f.close(); }
});

test("compaction is a native operation and records asynchronous completion or error", async () => {
  const f = await fixture();
  try {
    expect((await f.request({ op: "compact" })).result.status).toBe("accepted");
    expect((await f.request({ op: "status" })).result.compacting).toBe(true);
    f.complete();
    expect((await f.request({ op: "receipt", request_id: "a" })).result.status).toBe("completed");
    await f.request({ op: "compact", id: "b" });
    f.fail();
    expect((await f.request({ op: "receipt", request_id: "b" })).result).toEqual({ status: "failed", error: "model unavailable" });
    expect(f.sent.length).toBe(0);
  } finally { f.close(); }
});

test("busy compaction returns feedback without creating uncertain receipt", async () => {
  const f = await fixture();
  try {
    f.busy();
    expect((await f.request({ op: "compact" })).error).toContain("Pi is busy");
    expect((await f.request({ op: "receipt", request_id: "a" })).result).toBeNull();
  } finally { f.close(); }
});

test("crash-boundary uncertain receipt survives restart and forbids replay", async () => {
  const f = await fixture([["prior", { status: "uncertain" }]]);
  try {
    expect((await f.request({ op: "send", id: "prior", text: "never repeat" })).result.status).toBe("uncertain");
    expect(f.sent).toEqual([]);
  } finally { f.close(); }
});

test("journal capacity belongs to exact conversation and resume retains receipts", async () => {
  const f = await fixture(Array.from({ length: 4096 }, (_, i) => [`old-${i}`, { status: "accepted" }]));
  try {
    expect((await f.request({ op: "send", id: "new", text: "hello" })).error).toContain("new conversation");
    await f.switchSession("two");
    expect((await f.request({ op: "send", id: "new", text: "hello" })).result.status).toBe("accepted");
    await f.switchSession("one");
    expect((await f.request({ op: "send", id: "old-0", text: "no replay" })).result.status).toBe("accepted");
    expect(f.sent.length).toBe(1);
  } finally { f.close(); }
});
