import { closeSync, mkdirSync, openSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { existsSync } from "node:fs";
import { hostname } from "node:os";
import { join } from "node:path";
import { dlopen, FFIType, suffix } from "bun:ffi";

import { config } from "./config.ts";
import { createLogger } from "./logger.ts";
import type { LockMeta } from "./types.ts";

const log = createLogger("[locks]");

// flock(2) via libc. Matches Python's fcntl.flock semantics: the
// kernel releases the lock automatically on fd close / process death,
// so a crashed process can't wedge other workers.
// Bun resolves the unversioned `libc.so` as a relative path on Linux, and
// glibc's `/usr/lib/libc.so` is a linker script rather than a loadable shared
// object anyway. Use glibc's stable runtime soname there; Darwin continues to
// use Bun's platform suffix (`libc.dylib`).
const libcName = process.platform === "linux" ? "libc.so.6" : `libc.${suffix}`;
const lib = dlopen(libcName, {
  flock: { args: [FFIType.i32, FFIType.i32], returns: FFIType.i32 },
});

const LOCK_EX = 2;
const LOCK_NB = 4;
const LOCK_UN = 8;

function flock(fd: number, op: number): number {
  return lib.symbols.flock(fd, op);
}

function lockPath(slug: string): string {
  return join(config.paths.lockDir, `${slug}.lock`);
}

function ensureLockDir(): void {
  mkdirSync(config.paths.lockDir, { recursive: true });
}

export function readLockMeta(path: string): Partial<LockMeta> {
  try {
    const raw = readFileSync(path, "utf8");
    if (!raw.trim()) return {};
    return JSON.parse(raw) as Partial<LockMeta>;
  } catch {
    return {};
  }
}

/**
 * Return lock metadata if a live wt lock is held, else null. Uses a
 * non-blocking try-acquire to distinguish stale files (crashed holder)
 * from live holders.
 */
export function lockStatus(slug: string): Partial<LockMeta> | null {
  const path = lockPath(slug);
  if (!existsSync(path)) return null;
  let fd: number;
  try {
    fd = openSync(path, "a+");
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { slug, path });
    return null;
  }
  try {
    const rc = flock(fd, LOCK_EX | LOCK_NB);
    if (rc === 0) {
      // We acquired it → previous holder was dead; file is stale.
      flock(fd, LOCK_UN);
      return null;
    }
    return readLockMeta(path);
  } finally {
    closeSync(fd);
  }
}

export type LockHandle = {
  path: string;
  fd: number;
  phase(description: string): void;
  release(): void;
};

/**
 * Try to acquire the per-slug lock. Returns a handle if acquired,
 * null if another process already holds it.
 */
export function tryAcquireLock(
  slug: string,
  op: string,
  opts: { phase?: string } = {},
): LockHandle | null {
  ensureLockDir();
  const path = lockPath(slug);
  const fd = openSync(path, "a+");
  const rc = flock(fd, LOCK_EX | LOCK_NB);
  if (rc !== 0) {
    closeSync(fd);
    return null;
  }
  // Known window (audited, accepted): between this acquire and the
  // first meta write below, a concurrent `lockStatus` reader sees the
  // previous holder's payload (or an empty file) and renders a generic
  // "busy" with no age. Purely cosmetic — the flock itself is already
  // held — and closing it would mean writing meta before knowing the
  // acquire succeeded.

  const started = new Date().toISOString();
  let currentPhase = opts.phase ?? "";
  let phaseStarted = started;

  function write(): void {
    const payload: LockMeta = {
      op,
      phase: currentPhase,
      pid: process.pid,
      host: hostname(),
      startedAt: started,
      phase_started: phaseStarted,
    };
    writeFileSync(path, JSON.stringify(payload));
  }
  write();

  return {
    path,
    fd,
    phase(description) {
      if (description !== currentPhase) {
        currentPhase = description;
        phaseStarted = new Date().toISOString();
      }
      write();
    },
    release() {
      try {
        unlinkSync(path);
      } catch (err) {
        // Lock file may already be gone (race with another cleanup).
        // The flock release below is the authoritative step; unlink is
        // just bookkeeping.
        void err;
      }
      try {
        flock(fd, LOCK_UN);
      } catch (err) {
        // flock returns rc rather than throwing, but guard anyway.
        void err;
      }
      closeSync(fd);
    },
  };
}

/**
 * Run `fn` while holding an exclusive flock on `<lockDir>/<name>.lock`,
 * BLOCKING until the lock is free. For short synchronous critical
 * sections only (e.g. a state-file read-modify-write): the kernel wait
 * blocks this thread, which is fine when every holder releases in
 * sub-millisecond time, and fd-close-on-crash means a dead holder can't
 * wedge anyone. Long-held operation locks keep using `tryAcquireLock`.
 * The lock file is never unlinked — competing processes reopen the same
 * inode, which is what makes the flock handoff race-free.
 */
export function withFileLock<T>(name: string, fn: () => T): T {
  ensureLockDir();
  const fd = openSync(lockPath(name), "a+");
  try {
    flock(fd, LOCK_EX);
    return fn();
  } finally {
    flock(fd, LOCK_UN);
    closeSync(fd);
  }
}

export function humanAge(seconds: number): string {
  if (seconds < 60) return `${Math.floor(seconds)}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}

export function lockAge(info: Partial<LockMeta>): string | null {
  const started = info.phase_started ?? info.startedAt ?? info.started;
  if (!started) return null;
  const t = Date.parse(started);
  if (Number.isNaN(t)) return null;
  return humanAge((Date.now() - t) / 1000);
}

export function lockLabel(info: Partial<LockMeta>): string {
  const { phase, op } = info;
  if (phase && op && phase !== op) return `${op}: ${phase}`;
  return phase || op || "busy";
}
