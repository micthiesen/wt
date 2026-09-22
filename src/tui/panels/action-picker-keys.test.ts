import { expect, test } from "bun:test";
import type { ActionDef } from "../../core/config.ts";
import { assignActionKeys } from "./action-picker.tsx";
import type { KeyEvent } from "@opentui/core";
import type { Modal } from "../modal-state.ts";
import type { SimpleModalContext } from "../modal-keys/ctx.ts";
import { handleActionPickerKey } from "../modal-keys/actions.ts";

const action = (id: string, key?: string): ActionDef => ({
  id, name: id, key, kind: "shell", shell: "true", affects: [], requires: [], argPrompt: null, labelExtract: null,
});

test("explicit numeric action shortcuts coexist with letters and reserved picker keys", () => {
  const defs = [action("progress", "1"), action("review", "2"), action("completed", "3"), action("pinned", "p"), action("custom collision", "c")];
  const keys = assignActionKeys(defs, ["m", "l"]);
  expect([...keys.entries()].slice(0, 4)).toEqual([["progress", "1"], ["review", "2"], ["completed", "3"], ["pinned", "p"]]);
  expect(keys.get("custom collision")).not.toBe("c");
  expect(new Set(keys.values()).size).toBe(keys.size);
});

test("numeric duplicates fall back safely without stealing another explicit key", () => {
  const keys = assignActionKeys([action("first", "1"), action("second", "1"), action("third", "s")]);
  expect(keys.get("first")).toBe("1"); expect(keys.get("third")).toBe("s");
  expect(keys.get("second")).not.toBe("1"); expect(keys.get("second")).not.toBe("s");
});

test("a numeric chord dispatches its assigned action, not the highlighted row", async () => {
  const launched: string[] = [];
  const items = [action("highlighted", "h"), action("review", "2")].map((def) => ({ kind: "action", def, key: def.key, availability: { ok: true } }));
  const ctx = {
    setModal: () => {}, rows: [], buildActionPickerItems: () => items,
    canPickAction: () => true,
    launchAction: (_slug: string, def: ActionDef) => { launched.push(def.id); },
    reportActionError: () => { throw new Error("unexpected launch failure"); },
  } as unknown as SimpleModalContext;
  const modal = { kind: "actionPicker", state: { mode: "list", surface: "row", slug: "fixture", rowSlug: "fixture", index: 0 } } as Extract<Modal, { kind: "actionPicker" }>;
  for (const modifiers of [{ ctrl: true }, { meta: true }, { shift: true }]) handleActionPickerKey({ name: "2", sequence: "2", ...modifiers } as unknown as KeyEvent, modal, ctx);
  await Promise.resolve();
  expect(launched).toEqual([]);
  handleActionPickerKey({ name: "2", sequence: "2" } as KeyEvent, modal, ctx);
  await Promise.resolve();
  expect(launched).toEqual(["review"]);
});
