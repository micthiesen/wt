import { existsSync, mkdirSync, realpathSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { Database } from "bun:sqlite";

import { config } from "./config.ts";

type SqlMigration = { version: number; up: (db: Database) => void };

const MIGRATIONS: readonly SqlMigration[] = [
  {
    version: 1,
    up(db) {
      db.exec(`
        CREATE TABLE IF NOT EXISTS repositories (
          repo_id TEXT PRIMARY KEY,
          repo_path TEXT NOT NULL UNIQUE,
          created_at INTEGER NOT NULL,
          updated_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS repository_state (
          repo_id TEXT PRIMARY KEY REFERENCES repositories(repo_id) ON DELETE CASCADE,
          data TEXT NOT NULL,
          updated_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS archived_worktrees (
          repo_id TEXT NOT NULL REFERENCES repositories(repo_id) ON DELETE CASCADE,
          worktree_key TEXT NOT NULL,
          archived_at INTEGER NOT NULL,
          PRIMARY KEY (repo_id, worktree_key)
        );
      `);
    },
  },
];

let handle: Database | null = null;
let readHandle: Database | null = null;

function canonicalRepoPath(): string {
  const path = resolve(config.repoPath);
  try {
    return realpathSync(path);
  } catch {
    return path;
  }
}

function database(): Database {
  if (handle) return handle;
  mkdirSync(dirname(config.paths.stateDb), { recursive: true });
  const db = new Database(config.paths.stateDb, { create: true });
  db.exec("PRAGMA journal_mode = WAL;");
  db.exec("PRAGMA busy_timeout = 3000;");
  db.exec("PRAGMA foreign_keys = ON;");
  db.exec("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);");

  const applied = new Set(
    db.query<{ version: number }, []>("SELECT version FROM schema_migrations").all()
      .map((row) => row.version),
  );
  for (const migration of MIGRATIONS) {
    if (applied.has(migration.version)) continue;
    db.transaction(() => {
      migration.up(db);
      db.prepare("INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?, ?)")
        .run(migration.version, Date.now());
    })();
  }

  const repoPath = canonicalRepoPath();
  const now = Date.now();
  db.prepare(`
    INSERT INTO repositories (repo_id, repo_path, created_at, updated_at)
    VALUES (?, ?, ?, ?)
    ON CONFLICT(repo_id) DO UPDATE SET updated_at = excluded.updated_at
  `).run(config.repoId, repoPath, now, now);
  const existing = db.query<{ repo_path: string }, [string]>(
    "SELECT repo_path FROM repositories WHERE repo_id = ?",
  ).get(config.repoId);
  if (existing?.repo_path !== repoPath) {
    db.close();
    throw new Error(
      `repository namespace collision: ${config.repoId} belongs to ${existing?.repo_path ?? "an unknown path"}, not ${repoPath}`,
    );
  }
  handle = db;
  return db;
}

/** Existing state can be read from restricted agent sandboxes without
 * attempting WAL setup, schema DDL, or the repository timestamp upsert. */
function readDatabase(): Database | null {
  if (handle) return handle;
  if (readHandle) return readHandle;
  if (!existsSync(config.paths.stateDb)) return null;
  const db = new Database(config.paths.stateDb, { readonly: true });
  try {
    const existing = db.query<{ repo_path: string }, [string]>(
      "SELECT repo_path FROM repositories WHERE repo_id = ?",
    ).get(config.repoId);
    const repoPath = canonicalRepoPath();
    if (existing && existing.repo_path !== repoPath) {
      throw new Error(
        `repository namespace collision: ${config.repoId} belongs to ${existing.repo_path}, not ${repoPath}`,
      );
    }
  } catch (err) {
    db.close();
    // A database from an older build still needs its forward-only SQL
    // migration. Ordinary current-schema reads never take this write path.
    if (err instanceof Error && err.message.includes("no such table")) return database();
    throw err;
  }
  readHandle = db;
  return db;
}

export function hasRepositoryState(): boolean {
  return (readDatabase()?.query<{ found: number }, [string]>(
    "SELECT 1 AS found FROM repository_state WHERE repo_id = ? LIMIT 1",
  ).get(config.repoId) ?? null) !== null;
}

export function readRepositoryStateJson(): string | null {
  return readDatabase()?.query<{ data: string }, [string]>(
    "SELECT data FROM repository_state WHERE repo_id = ? LIMIT 1",
  ).get(config.repoId)?.data ?? null;
}

export function writeRepositoryStateJson(data: string): void {
  database().prepare(`
    INSERT INTO repository_state (repo_id, data, updated_at)
    VALUES (?, ?, ?)
    ON CONFLICT(repo_id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at
  `).run(config.repoId, data, Date.now());
}

export function readArchivedKeys(): Set<string> {
  const rows = readDatabase()?.query<{ worktree_key: string }, [string]>(
    "SELECT worktree_key FROM archived_worktrees WHERE repo_id = ? ORDER BY worktree_key",
  ).all(config.repoId) ?? [];
  return new Set(rows.map((row) => row.worktree_key));
}

export function writeArchivedKeys(keys: ReadonlySet<string>): void {
  const db = database();
  db.transaction(() => replaceArchivedKeys(db, keys))();
}

function replaceArchivedKeys(db: Database, keys: ReadonlySet<string>): void {
  db.prepare("DELETE FROM archived_worktrees WHERE repo_id = ?").run(config.repoId);
  const insert = db.prepare(`
    INSERT INTO archived_worktrees (repo_id, worktree_key, archived_at)
    VALUES (?, ?, ?)
  `);
  const now = Date.now();
  for (const key of [...keys].sort()) insert.run(config.repoId, key, now);
}

/** Test/command seam for importing a complete repository snapshot atomically. */
export function importRepositorySnapshot(stateJson: string, archived: ReadonlySet<string>): void {
  const db = database();
  db.transaction(() => {
    db.prepare(`
      INSERT INTO repository_state (repo_id, data, updated_at)
      VALUES (?, ?, ?)
      ON CONFLICT(repo_id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at
    `).run(config.repoId, stateJson, Date.now());
    const insert = db.prepare(`
      INSERT OR IGNORE INTO archived_worktrees (repo_id, worktree_key, archived_at)
      VALUES (?, ?, ?)
    `);
    const now = Date.now();
    for (const key of [...archived].sort()) insert.run(config.repoId, key, now);
  })();
}

/**
 * One repository's durable records, read out of an ARBITRARY state database.
 *
 * Exists for repair rather than for normal reads: a build that derived the
 * repository namespace from the caller's cwd wrote this repository's records
 * under a per-worktree id, and sometimes into a second database file
 * altogether. Nothing in the normal read path can see those rows — the id and
 * the file are both wrong — so `wt state migrate` opens the candidates itself
 * and adopts what belongs here. Read-only and non-mutating: adoption must not
 * be able to damage a database it may have misidentified.
 */
export type ForeignRepositoryRow = {
  repoId: string;
  repoPath: string;
  data: string;
  updatedAt: number;
  archived: Set<string>;
};

export function readForeignRepositoryRows(dbPath: string): ForeignRepositoryRow[] {
  if (!existsSync(dbPath)) return [];
  let db: Database;
  try {
    db = new Database(dbPath, { readonly: true });
  } catch {
    return [];
  }
  try {
    const rows = db.query<
      { repo_id: string; repo_path: string; data: string; updated_at: number },
      []
    >(`
      SELECT r.repo_id, r.repo_path, s.data, s.updated_at
      FROM repositories r
      JOIN repository_state s ON s.repo_id = r.repo_id
    `).all();
    return rows.map((row) => ({
      repoId: row.repo_id,
      repoPath: row.repo_path,
      data: row.data,
      updatedAt: row.updated_at,
      archived: new Set(
        db.query<{ worktree_key: string }, [string]>(
          "SELECT worktree_key FROM archived_worktrees WHERE repo_id = ?",
        ).all(row.repo_id).map((archived) => archived.worktree_key),
      ),
    }));
  } catch {
    // Not a wt state database, or written by a schema this build predates.
    return [];
  } finally {
    db.close();
  }
}
