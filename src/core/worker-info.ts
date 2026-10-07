import { Data, Effect, Fiber, SynchronizedRef } from "effect";

import type { RemoteConfig } from "./config.ts";
import { run } from "./proc.ts";
import { remoteWtCommand } from "./remote-protocol.ts";
import { ensureRemoteRuntime } from "./remote-runtime.ts";
import { wtVersion } from "./update.ts";

import { WORKER_PROTOCOL_VERSION } from "./worker-protocol.ts";
export { WORKER_PROTOCOL_VERSION } from "./worker-protocol.ts";

export type WorkerInfo = {
  role: "controller" | "worker";
  protocol: number;
  build: string;
};

export class WorkerInfoTransportError extends Data.TaggedError(
  "WorkerInfoTransportError",
)<{ readonly message: string; readonly cause?: unknown }> {}

export class WorkerInfoProtocolError extends Data.TaggedError(
  "WorkerInfoProtocolError",
)<{ readonly message: string; readonly cause?: unknown }> {}

export type WorkerInfoError =
  WorkerInfoTransportError | WorkerInfoProtocolError;

export function currentWorkerInfo(role: WorkerInfo["role"]): WorkerInfo {
  return { role, protocol: WORKER_PROTOCOL_VERSION, build: wtVersion() };
}

export function parseWorkerInfo(raw: string): WorkerInfo {
  let value: unknown;
  try {
    value = JSON.parse(raw.trim());
  } catch (cause) {
    throw new WorkerInfoProtocolError({
      message: "remote worker handshake did not return JSON — run sync-wt",
      cause,
    });
  }
  if (!value || typeof value !== "object") {
    throw new WorkerInfoProtocolError({
      message: "remote worker handshake returned an invalid payload",
    });
  }
  const info = value as Partial<WorkerInfo>;
  if (info.role !== "controller" && info.role !== "worker") {
    throw new WorkerInfoProtocolError({
      message: "remote worker handshake did not report a valid role",
    });
  }
  if (typeof info.protocol !== "number" || !Number.isInteger(info.protocol)) {
    throw new WorkerInfoProtocolError({
      message: "remote worker handshake did not report a protocol version",
    });
  }
  if (typeof info.build !== "string" || info.build.trim() === "") {
    throw new WorkerInfoProtocolError({
      message: "remote worker handshake did not report a build",
    });
  }
  return { role: info.role, protocol: info.protocol, build: info.build };
}

type WorkerInfoLoader = (
  remote: RemoteConfig,
) => Effect.Effect<WorkerInfo, WorkerInfoError>;

type SharedRequest = {
  readonly fiber: Fiber.Fiber<WorkerInfo, WorkerInfoError>;
  readonly consumers: number;
  readonly token: object;
};

type FetcherState = {
  readonly successful: Map<string, WorkerInfo>;
  readonly inFlight: Map<string, SharedRequest>;
};

const remoteKey = (remote: RemoteConfig): string =>
  `${remote.host}\0${remote.wtPath}`;

/**
 * Cache policy for worker handshakes. Each caller joins the shared daemon fiber
 * independently; interruption releases only that caller, and the shared load
 * is cancelled only after its final consumer leaves.
 */
export function createWorkerInfoFetcher(load: WorkerInfoLoader) {
  const state = SynchronizedRef.makeUnsafe<FetcherState>({
    successful: new Map(),
    inFlight: new Map(),
  });

  const acquire = (
    remote: RemoteConfig,
    force: boolean,
  ): Effect.Effect<WorkerInfo | SharedRequest, never> =>
    SynchronizedRef.modifyEffect(
      state,
      Effect.fnUntraced(function* (
        current: FetcherState,
      ): Effect.fn.Return<readonly [WorkerInfo | SharedRequest, FetcherState]> {
        const key = remoteKey(remote);
        const successful = new Map(current.successful);
        if (force) successful.delete(key);
        const cached = successful.get(key);
        if (cached) {
          return [cached, { ...current, successful }] as const;
        }
        const existing = current.inFlight.get(key);
        if (existing) {
          const shared = { ...existing, consumers: existing.consumers + 1 };
          const inFlight = new Map(current.inFlight).set(key, shared);
          return [shared, { successful, inFlight }] as const;
        }
        const token = {};
        const fiber = yield* Effect.forkDetach(
          Effect.interruptible(load(remote)),
          { startImmediately: true },
        );
        const shared = { fiber, consumers: 1, token };
        const inFlight = new Map(current.inFlight).set(key, shared);
        return [shared, { successful, inFlight }] as const;
      }),
    );

  const release = (
    remote: RemoteConfig,
    acquired: WorkerInfo | SharedRequest,
  ): Effect.Effect<void> => {
    if (!("fiber" in acquired)) return Effect.void;
    const key = remoteKey(remote);
    return SynchronizedRef.modify(state, (current) => {
      const active = current.inFlight.get(key);
      if (!active || active.fiber !== acquired.fiber)
        return [undefined, current];
      if (active.consumers > 1) {
        const inFlight = new Map(current.inFlight).set(key, {
          ...active,
          consumers: active.consumers - 1,
        });
        return [undefined, { ...current, inFlight }];
      }
      const inFlight = new Map(current.inFlight);
      inFlight.delete(key);
      return [active.fiber, { ...current, inFlight }];
    }).pipe(
      Effect.flatMap((fiber) =>
        fiber ? Fiber.interrupt(fiber) : Effect.void,
      ),
    );
  };

  const fetchEffect = (
    remote: RemoteConfig,
    force = false,
  ): Effect.Effect<WorkerInfo, WorkerInfoError> =>
    Effect.acquireUseRelease(
      acquire(remote, force),
      (acquired) =>
        "fiber" in acquired
          ? Fiber.join(acquired.fiber).pipe(
              Effect.tap((info) =>
                SynchronizedRef.update(state, (latest) => ({
                  ...latest,
                  successful:
                    latest.inFlight.get(remoteKey(remote))?.token ===
                      acquired.token
                      ? new Map(latest.successful).set(
                          remoteKey(remote),
                          info,
                        )
                      : latest.successful,
                })),
              ),
            )
          : Effect.succeed(acquired),
      (acquired) => release(remote, acquired),
    );

  return {
    fetchEffect: (remote: RemoteConfig) => fetchEffect(remote),
    refreshEffect: (remote: RemoteConfig) => fetchEffect(remote, true),
  };
}

const loadRemoteWorkerInfo = Effect.fnUntraced(function* (
  remote: RemoteConfig,
): Effect.fn.Return<WorkerInfo, WorkerInfoError> {
  yield* ensureRemoteRuntime(remote).pipe(Effect.mapError((cause) =>
    new WorkerInfoTransportError({ message: `remote runtime setup failed: ${cause.message}`, cause }),
  ));
  const result = yield* run(
    [
      "ssh",
      "-o",
      "BatchMode=yes",
      "-o",
      "ConnectTimeout=5",
      remote.host,
      remoteWtCommand(remote, ["_hello"]),
    ],
    { cwd: process.cwd(), timeoutMs: 15_000 },
  ).pipe(
    Effect.mapError(
      (cause) =>
        new WorkerInfoTransportError({
          message: `remote worker handshake failed: ${cause.message}`,
          cause,
        }),
    ),
  );
  if (result.exitCode !== 0) {
    const detail =
      result.stderr.trim() ||
      result.stdout.trim() ||
      `SSH exited ${result.exitCode}`;
    return yield* new WorkerInfoTransportError({
      message: `remote worker handshake failed: ${detail}`,
    });
  }
  const info = yield* Effect.try({
    try: () => parseWorkerInfo(result.stdout),
    catch: (cause) =>
      cause instanceof WorkerInfoProtocolError
        ? cause
        : new WorkerInfoProtocolError({
            message: "remote worker handshake returned an invalid payload",
            cause,
          }),
  });
  if (info.role !== "worker") {
    return yield* new WorkerInfoProtocolError({
      message: `remote ${remote.label} is configured as ${info.role}; set [instance] role = "worker" there`,
    });
  }
  if (info.protocol !== WORKER_PROTOCOL_VERSION) {
    return yield* new WorkerInfoProtocolError({
      message: `remote ${remote.label} uses protocol ${info.protocol}; this controller requires ${WORKER_PROTOCOL_VERSION} — run sync-wt`,
    });
  }
  return info;
});

const workerInfoFetcher = createWorkerInfoFetcher(loadRemoteWorkerInfo);

export const fetchRemoteWorkerInfo = workerInfoFetcher.fetchEffect;
export const refreshRemoteWorkerInfo = workerInfoFetcher.refreshEffect;
