import { describe, expect, test } from "bun:test";
import { Deferred, Duration, Effect, Exit, Fiber, Ref } from "effect";
import { TestClock } from "effect/testing";

import {
  AiNamingError,
  isStackTitleMetaOnly,
  NAMING_RETRY_SCHEDULE,
  parseTitleDescription,
  withNamingPermit,
} from "./ai.ts";

describe("isStackTitleMetaOnly", () => {
  test("rejects a bare leaked meta word", () => {
    // The eng-5202 incident: the model handed back its own vocabulary.
    expect(isStackTitleMetaOnly("TUI")).toBe(true);
  });

  test("rejects a title built entirely from packaging words", () => {
    expect(isStackTitleMetaOnly("Header Stack Section")).toBe(true);
    expect(isStackTitleMetaOnly("Developer Tool Group")).toBe(true);
  });

  test("keeps a real title, even one that reuses a meta word as domain content", () => {
    // "header" is on the list, but "Stamp" anchors it — never strip to "Stamp".
    expect(isStackTitleMetaOnly("Header Stamp")).toBe(false);
    expect(isStackTitleMetaOnly("Atomic builder claim")).toBe(false);
    expect(isStackTitleMetaOnly("Orchestration Stack")).toBe(false);
  });

  test("empty input is not meta-only (nothing to reject)", () => {
    expect(isStackTitleMetaOnly("")).toBe(false);
    expect(isStackTitleMetaOnly("   ")).toBe(false);
  });

  test("is punctuation- and case-insensitive", () => {
    expect(isStackTitleMetaOnly("stack.")).toBe(true);
    expect(isStackTitleMetaOnly("BRANCHES")).toBe(true);
  });
});

test("a queued naming request cancelled before its permit never runs", async () => {
  const ran = await Effect.runPromise(Effect.scoped(Effect.gen(function* () {
    const release = yield* Deferred.make<void>();
    const acquired = yield* Deferred.make<void>();
    const count = yield* Ref.make(0);
    const holder = yield* Effect.forkScoped(withNamingPermit(
      Deferred.succeed(acquired, undefined).pipe(Effect.andThen(Deferred.await(release))),
    ));
    yield* Deferred.await(acquired);
    const queued = yield* Effect.forkScoped(withNamingPermit(Ref.update(count, (n) => n + 1)));
    yield* Fiber.interrupt(queued);
    const queuedExit = yield* Fiber.await(queued);
    yield* Deferred.succeed(release, undefined);
    yield* Fiber.join(holder);
    return { count: yield* Ref.get(count), interrupted: Exit.hasInterrupts(queuedExit) };
  })));
  expect(ran).toEqual({ count: 0, interrupted: true });
});

describe("NAMING_RETRY_SCHEDULE", () => {
  test("a transient failure retries once, then a second failure surfaces AiNamingError", async () => {
    const { attempts, failure } = await Effect.runPromise(Effect.gen(function* () {
      let attempts = 0;
      const failingCall = Effect.suspend(() => {
        attempts++;
        return Effect.fail(
          new AiNamingError({ kind: "completion", detail: `attempt ${attempts} failed` }),
        );
      }).pipe(Effect.retry(NAMING_RETRY_SCHEDULE));

      const fiber = yield* Effect.forkChild(failingCall);
      // The schedule caps its delay at 500ms; advancing well past that
      // lets the one permitted retry (and only that one) fire.
      yield* TestClock.adjust(Duration.seconds(2));
      const failure = yield* Fiber.join(fiber).pipe(Effect.flip);
      return { attempts, failure };
    }).pipe(Effect.provide(TestClock.layer())));

    // One initial attempt plus exactly one retry — `Schedule.recurs(1)`
    // caps it there, so a persistently transient failure still surfaces
    // rather than retrying forever.
    expect(attempts).toBe(2);
    expect(failure).toBeInstanceOf(AiNamingError);
    expect(failure.detail).toBe("attempt 2 failed");
  });
});

describe("parseTitleDescription", () => {
  test("extracts the naming contract from harness output", () => {
    expect(parseTitleDescription(
      "TITLE: Fix the thing\nDESCRIPTION: Done.",
    )).toEqual({
      title: "Fix the thing",
      description: "Done.",
    });
  });

  test("tolerates harness noise around the formatted answer", () => {
    expect(parseTitleDescription(
      "startup notice\nTITLE: Fix the thing\nBRIEF: Thing fix\nDESCRIPTION: Done.",
    ).title).toBe("Fix the thing");
  });

  test("ignores legacy BRIEF output instead of giving the list a second title", () => {
    expect(parseTitleDescription(
      'TITLE: "Move files to R2."\nBRIEF: R2 files\nMoves uploads into object storage.',
    )).toEqual({ title: "Move files to R2", description: "Moves uploads into object storage." });
  });

  test("missing title stays unknown rather than promoting the old brief", () => {
    expect(parseTitleDescription("BRIEF: R2 files\nDESCRIPTION: Move uploads.")).toEqual({
      title: null, description: "Move uploads.",
    });
    expect(parseTitleDescription("Unstructured description.")).toEqual({
      title: null, description: "Unstructured description.",
    });
  });
});
