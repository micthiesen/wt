import { createHash, randomUUID } from "node:crypto";
import { existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readlinkSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Effect, Semaphore } from "effect";

import type { RemoteConfig } from "./config.ts";
import { operationErrors } from "./errors.ts";
import { run } from "./proc.ts";
import { setRemoteRuntimePath } from "./remote-protocol.ts";
import { WT_REPO_ROOT, wtVersion } from "./update.ts";
import { WORKER_PROTOCOL_VERSION } from "./worker-protocol.ts";

const io = operationErrors("remote runtime");
const gate = Semaphore.makeUnsafe(1);
const prepared = new Set<string>();

export function runtimePath(hash: string): string {
  if (!/^[a-f0-9]{64}$/.test(hash)) throw new Error("invalid runtime hash");
  return `~/.cache/wt/runtimes/${hash}/bin/wt`;
}

/** Hash file names, modes, and bytes. Metadata times do not change identity. */
export const runtimeHash = Effect.fn("runtimeHash")(function* (root: string, files: readonly string[], version: string) {
  return yield* io.sync("hash runtime files", () => {
    const hash = createHash("sha256").update(JSON.stringify(version));
    for (const name of [...files].sort()) {
      const path = join(root, name);
      const stat = lstatSync(path);
      const bytes = stat.isSymbolicLink() ? Buffer.from(readlinkSync(path)) : readFileSync(path);
      hash.update(JSON.stringify([name, stat.mode & 0o777, bytes.length, stat.isSymbolicLink()]));
      hash.update(bytes);
    }
    return hash.digest("hex");
  });
});

const checked = Effect.fnUntraced(function* (argv: string[], input?: string) {
  const result = yield* run(argv, { cwd: WT_REPO_ROOT, input, timeoutMs: 300_000 });
  if (result.exitCode !== 0) {
    return yield* io.sync("prepare remote runtime", () => {
      throw new Error(result.stderr.trim() || result.stdout.trim() || `${argv[0]} exited ${result.exitCode}`);
    });
  }
  return result.stdout;
});

const ssh = (remote: RemoteConfig, command: string) => [
  "ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5",
  "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3", remote.host, command,
];

/** Build and validate a separate package. Never replace an existing install. */
export const ensureRemoteRuntime = Effect.fn("ensureRemoteRuntime")(function* (
  remote: RemoteConfig,
  onLine?: (line: string) => void,
) {
  yield* gate.withPermit(Effect.gen(function* () {
    const key = `${remote.host}\0${remote.wtPath}`;
    if (prepared.has(key)) return;
    const scratch = yield* io.sync("create runtime scratch directory", () => mkdtempSync(join(tmpdir(), "wt-runtime-")));
    yield* Effect.gen(function* () {
      const listing = yield* checked(["git", "ls-files", "-co", "--exclude-standard", "-z"]);
      const files = yield* io.sync("select runtime files", () => [...new Set(listing.split("\0"))]
        .filter((name) => name && name !== ".wt-runtime.json" && existsSync(join(WT_REPO_ROOT, name)))
        .sort());
      const version = wtVersion();
      const hash = yield* runtimeHash(WT_REPO_ROOT, files, version);
      const path = runtimePath(hash);
      const probe = yield* checked(ssh(remote,
        `if test -f "$HOME/.cache/wt/runtimes/${hash}/.ready"; then printf ready; fi`));
      if (probe !== "ready") {
        yield* io.sync("report runtime setup", () => onLine?.(`preparing wt runtime on ${remote.label}`));
        const list = join(scratch, "files");
        const archive = join(scratch, "runtime.tgz");
        yield* io.sync("write runtime file list", () => writeFileSync(list, files.join("\0") + "\0"));
        const tarArgs = process.platform === "darwin" ? ["--no-mac-metadata", "--no-xattrs"] : [];
        yield* checked(["tar", ...tarArgs, "-czf", archive, "-C", WT_REPO_ROOT, "--null", "-T", list]);
        const snapshot = join(scratch, "snapshot");
        yield* io.sync("create snapshot check directory", () => mkdirSync(snapshot));
        yield* checked(["tar", "-xzpf", archive, "-C", snapshot]);
        // Check the archive itself, not a second read of the live source tree.
        const archivedHash = yield* runtimeHash(snapshot, files, version);
        yield* io.sync("check runtime snapshot", () => {
          if (archivedHash !== hash) throw new Error("wt source changed during packaging; retry the command");
        });
        const checksum = yield* io.sync("hash runtime archive", () => createHash("sha256").update(readFileSync(archive)).digest("hex"));
        const upload = `.cache/wt/runtimes/.upload-${randomUUID()}.tgz`;
        yield* checked(ssh(remote, 'mkdir -p "$HOME/.cache/wt/runtimes"'));
        yield* checked(["scp", "-q", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", archive, `${remote.host}:${upload}`]);
        const script = yield* io.sync("read runtime installer", () => readFileSync(join(WT_REPO_ROOT, "scripts/remote-runtime-install.sh"), "utf8"));
        const metadata = Buffer.from(JSON.stringify({ build: version, hash, protocol: WORKER_PROTOCOL_VERSION })).toString("base64");
        yield* checked(ssh(remote, `bash -s -- '${upload}' '${hash}' '${checksum}' '${metadata}'`), script);
      }
      yield* io.sync("select remote runtime", () => {
        setRemoteRuntimePath(remote, path);
        prepared.add(key);
      });
    }).pipe(Effect.ensuring(io.sync("remove runtime scratch directory", () => rmSync(scratch, { recursive: true, force: true })).pipe(Effect.orDie)));
  }));
});
