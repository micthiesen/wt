// Loaded explicitly by wt. No package imports: this runs in Pi's native TUI.
import { createServer } from "node:net";
import { chmodSync, existsSync, lstatSync, readFileSync, unlinkSync, writeFileSync, renameSync } from "node:fs";
import { randomUUID, createHash } from "node:crypto";
import { execFile } from "node:child_process";

const LIMIT = 1024 * 1024;

export default function wtPi(pi: any) {
  pi.registerFlag("wt-control", { description: "Private wt control socket", type: "string" });
  pi.registerFlag("wt-tmux-session", { description: "Owning wt tmux session", type: "string" });
  let ctx: any;
  let server: ReturnType<typeof createServer> | undefined;
  let socketPath: string | undefined;
  let journalPath: string | undefined;
  let compacting = false;
  let started = Date.now();
  const connections = new Set<any>();
  const receipts = new Map<string, any>();

  function state() {
    return { session_id: ctx.sessionManager.getSessionId(), cwd: ctx.cwd,
      state: compacting || !ctx.isIdle() ? "working" : "waiting",
      queued: ctx.hasPendingMessages() ? 1 : 0, compacting,
      context_percent: ctx.getContextUsage()?.percent ?? null,
      status_since: started, session_file: ctx.sessionManager.getSessionFile() ?? null };
  }
  function persist() {
    if (!journalPath) throw Error("Pi journal is unavailable");
    const path = journalPath;
    const temp = `${path}.${process.pid}.${randomUUID()}`;
    writeFileSync(temp, JSON.stringify([...receipts]), { mode: 0o600 });
    renameSync(temp, path);
  }
  function remember(id: string, value: any) {
    receipts.set(id, value);
    // Receipts live as long as this slot. Never evict an accepted request and
    // accidentally permit its replay in a long-running conversation.
    persist();
  }
  function finish(id: string, value: any) {
    try { remember(id, value); }
    catch (error: any) { receipts.set(id, { status: "uncertain", error: `Cannot persist Pi receipt: ${error.message}` }); }
  }
  async function dispatch(req: any) {
    if (!req || typeof req.id !== "string" || req.id.length > 100) throw Error("Invalid request id");
    const current = state();
    if (req.session_id !== current.session_id || req.cwd !== current.cwd) throw Error("Pi session identity changed; reopen the session picker");
    if (req.op === "status") return current;
    if (req.op === "receipt") return receipts.get(req.request_id) ?? null;
    if (!["send", "compact"].includes(req.op)) throw Error("Unsupported Pi operation");
    if (receipts.has(req.id)) return receipts.get(req.id);
    if (receipts.size >= 4096) throw Error("Pi delivery journal is full; start a new conversation before sending more");
    if (req.op === "send" && (typeof req.text !== "string" || !req.text.trim() || Buffer.byteLength(req.text) > LIMIT / 2)) throw Error("Invalid message");
    if (req.op === "compact" && (!ctx.isIdle() || compacting)) throw Error("Pi is busy; compact after the current turn finishes");
    // Write before calling the non-idempotent API. A crash at this boundary is
    // uncertain, never permission to inject the same message a second time.
    remember(req.id, { status: "uncertain" });
    try { if (req.op === "send") {
      pi.sendUserMessage(req.text, { deliverAs: "followUp", expandPromptTemplates: true });
      finish(req.id, { status: "accepted" });
    } else {
      compacting = true;
      started = Date.now();
      ctx.compact({
        customInstructions: typeof req.instructions === "string" ? req.instructions : undefined,
        onComplete: () => { compacting = false; finish(req.id, { status: "completed" }); },
        onError: (error: Error) => { compacting = false; finish(req.id, { status: "failed", error: error.message }); },
      });
      // A synchronous completion callback must not be overwritten.
      if (receipts.get(req.id)?.status === "uncertain") finish(req.id, { status: "accepted" });
    } } catch (error: any) {
      compacting = false;
      finish(req.id, { status: "uncertain", error: String(error.message ?? error) });
    }
    return receipts.get(req.id);
  }
  function stop() {
    for (const conn of connections) conn.destroy();
    connections.clear();
    if (server) { server.close(); server = undefined; }
    // Only our bound socket can be removed; startup never removes a foreign one.
    if (socketPath) { try { unlinkSync(socketPath); } catch {} socketPath = undefined; }
  }
  pi.on("session_start", async (_event: any, context: any) => {
    ctx = context;
    const requested = pi.getFlag("wt-control");
    if (typeof requested !== "string" || !requested) return;
    const journal = `${requested}.receipts-${createHash("sha256").update(ctx.sessionManager.getSessionId()).digest("hex")}`;
    if (journalPath !== journal) {
      receipts.clear();
      journalPath = journal;
      if (existsSync(journal)) {
        if (!lstatSync(journal).isFile() || lstatSync(journal).size > LIMIT) throw Error("Invalid wt delivery journal");
        const stored = JSON.parse(readFileSync(journal, "utf8"));
        if (!Array.isArray(stored) || stored.length > 4096) throw Error("Invalid wt delivery journal");
        for (const [id, receipt] of stored) receipts.set(id, receipt);
      }
    }
    // /new and /resume inside Pi change the conversation without changing its
    // tmux slot. Publish the identity of the process that actually owns it.
    const tmuxSession = pi.getFlag("wt-tmux-session");
    if (typeof tmuxSession === "string" && tmuxSession) {
      await new Promise<void>((resolve, reject) => execFile("tmux", ["set-option", "-t", `=${tmuxSession}:`,
        "@wt-harness-session-id", ctx.sessionManager.getSessionId()], { timeout: 3000 }, error => error ? reject(error) : resolve()));
    }
    if (server) return;
    if (existsSync(requested)) throw Error("wt Pi control socket already exists");
    const candidate = createServer(conn => {
      connections.add(conn);
      let chunks: Buffer[] = [], size = 0, handled = false;
      conn.setTimeout(5000, () => conn.destroy());
      conn.on("close", () => connections.delete(conn));
      conn.on("error", () => {});
      conn.on("data", chunk => {
        if (handled) return;
        size += chunk.length;
        if (size > LIMIT) { handled = true; conn.destroy(); return; }
        chunks.push(chunk);
        if (!chunk.includes(10)) return;
        handled = true;
        const raw = Buffer.concat(chunks); chunks = [];
        let req: any;
        Promise.resolve().then(() => {
          req = JSON.parse(raw.subarray(0, raw.indexOf(10)).toString("utf8"));
          return dispatch(req);
        }).then(result => conn.end(JSON.stringify({ id: req.id, ok: true, result }) + "\n"),
          error => conn.end(JSON.stringify({ id: req?.id, ok: false, error: String(error.message ?? error) }) + "\n"));
      });
    });
    await new Promise<void>((resolve, reject) => {
      candidate.once("error", reject);
      candidate.listen(requested, () => { candidate.removeListener("error", reject); resolve(); });
    });
    socketPath = requested;
    server = candidate;
    chmodSync(requested, 0o600);
  });
  pi.on("session_shutdown", stop);
  for (const event of ["agent_start", "agent_settled", "session_compact", "session_compact_failed"]) {
    pi.on(event as any, (_event: any, context: any) => { ctx = context; started = Date.now(); });
  }
}
