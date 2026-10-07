import { describe, expect, test } from "bun:test";
import { Deferred, Effect, Fiber } from "effect";

import type { AiSummary } from "../core/ai.ts";
import type { DiffContext } from "../core/diff/index.ts";
import { generateAndSaveTitle, MissingGeneratedTitle, TitleEditSuperseded } from "./title-actions.ts";

const context: DiffContext = {
  hash: "title-test", prompt: "a diff", filesTotal: 1,
  counts: { full: 1, tight: 0, hunks: 0, dropped: 0 },
};
const summary: AiSummary = { title: "Generated title", description: "description" };

describe("explicit title generation", () => {
  test("reads fresh context and waits for generation before saving", async () => {
    const calls: string[] = [];
    const result = await Effect.runPromise(generateAndSaveTitle({
      context: Effect.sync(() => { calls.push("context"); return context; }),
      generate: (ctx) => Effect.sync(() => {
        expect(ctx).toBe(context);
        calls.push("generate");
        return summary;
      }),
      save: (title) => Effect.sync(() => { calls.push(title); return true; }),
    }));
    expect(result).toBe(true);
    expect(calls).toEqual(["context", "generate", "Generated title"]);
  });

  test("an empty diff does not invoke the model or replace a title", async () => {
    let calls = 0;
    expect(await Effect.runPromise(generateAndSaveTitle({
      context: Effect.succeed(null),
      generate: () => Effect.sync(() => { calls++; return summary; }),
      save: () => Effect.sync(() => { calls++; return true; }),
    }))).toBe(false);
    expect(calls).toBe(0);
  });

  test.each([null, "", "  "])("missing generated title (%s) preserves the saved title", async (title) => {
    let saved = false;
    const result = await Effect.runPromise(generateAndSaveTitle({
      context: Effect.succeed(context),
      generate: () => Effect.succeed({ ...summary, title }),
      save: () => Effect.sync(() => { saved = true; return true; }),
    }).pipe(Effect.flip));
    expect(result).toBeInstanceOf(MissingGeneratedTitle);
    expect(saved).toBe(false);
  });

  test("a later manual edit wins over an in-flight generation", async () => {
    await Effect.runPromise(Effect.gen(function* () {
      const started = yield* Deferred.make<void>();
      const finish = yield* Deferred.make<AiSummary>();
      let revision = 4;
      const expected = revision;
      let title = "Original";
      const fiber = yield* generateAndSaveTitle({
        context: Effect.succeed(context),
        generate: () => Deferred.succeed(started, undefined).pipe(
          Effect.andThen(Deferred.await(finish)),
        ),
        save: (next) => Effect.sync(() => {
          if (revision !== expected) return false;
          title = next;
          return true;
        }),
      }).pipe(Effect.flip, Effect.forkChild);
      yield* Deferred.await(started);
      revision++;
      title = "My later edit";
      yield* Deferred.succeed(finish, summary);
      expect(yield* Fiber.join(fiber)).toBeInstanceOf(TitleEditSuperseded);
      expect(title).toBe("My later edit");
    }));
  });

  test("model failure preserves the title and remains an actionable error", async () => {
    const failure = new Error("naming command failed");
    let saved = false;
    const result = await Effect.runPromise(generateAndSaveTitle({
      context: Effect.succeed(context),
      generate: () => Effect.fail(failure),
      save: () => Effect.sync(() => { saved = true; return true; }),
    }).pipe(Effect.flip));
    expect(result).toBe(failure);
    expect(saved).toBe(false);
  });
});
