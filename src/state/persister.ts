import { existsSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { Database } from "bun:sqlite";
import type { Persister } from "@tanstack/query-persist-client-core";
import type { PersistedClient } from "@tanstack/query-persist-client-core";

import { createLogger } from "../core/logger.ts";

const log = createLogger("[cache]");

/**
 * SQLite persister for TanStack Query. Stores the entire dehydrated
 * client as a single JSON blob keyed by `CLIENT_KEY`. Synchronous
 * reads/writes — fine since bun:sqlite is fast and the blob is small
 * (a few kB per worktree).
 */
const CLIENT_KEY = "wt.cache.v1";

/**
 * Stateless helper to wipe the persisted cache blob without needing
 * access to the long-lived persister instance. Used by the TUI's
 * "clear all data" action.
 */
export function clearPersistedCache(dbPath: string): void {
  if (!existsSync(dbPath)) return;
  // bun:sqlite requires at least one of READONLY / READWRITE when any
  // options are passed, so we set `readwrite: true` explicitly. We
  // already gated on existsSync so `create: false` is safe.
  const db = new Database(dbPath, { readwrite: true, create: false });
  try {
    db.exec("DELETE FROM cache");
  } catch (err) {
    // Schema drift between versions shouldn't crash the action.
    log.error(err instanceof Error ? err : String(err), { dbPath });
  } finally {
    db.close();
  }
}

export type ClosablePersister = Persister & { close(): void };

export function createSqlitePersister(dbPath: string): ClosablePersister {
  mkdirSync(dirname(dbPath), { recursive: true });
  const db = new Database(dbPath, { create: true });
  db.exec(`
    CREATE TABLE IF NOT EXISTS cache (
      id TEXT PRIMARY KEY,
      data TEXT NOT NULL,
      updated_at INTEGER NOT NULL
    );
  `);

  const readStmt = db.query<
    { data: string },
    [string]
  >("SELECT data FROM cache WHERE id = ? LIMIT 1");

  const writeStmt = db.prepare(
    "INSERT OR REPLACE INTO cache (id, data, updated_at) VALUES (?, ?, ?)",
  );

  const deleteStmt = db.prepare("DELETE FROM cache WHERE id = ?");

  return {
    persistClient(client: PersistedClient): void {
      writeStmt.run(CLIENT_KEY, JSON.stringify(client), Date.now());
    },
    restoreClient(): PersistedClient | undefined {
      const row = readStmt.get(CLIENT_KEY);
      if (!row) return undefined;
      try {
        return JSON.parse(row.data) as PersistedClient;
      } catch (err) {
        log.error(err instanceof Error ? err : String(err), { stage: "restoreClient" });
        return undefined;
      }
    },
    removeClient(): void {
      deleteStmt.run(CLIENT_KEY);
    },
    close(): void {
      db.close();
    },
  };
}
