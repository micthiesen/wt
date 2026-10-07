import { describe, expect, test } from "bun:test";
import type { KeyEvent } from "@opentui/core";

import type { FooterMode } from "../panels/footer.tsx";
import { makeEdit } from "../text-edit.tsx";
import { handleFooterInputKey, restoreFailedCreateFooter, type FooterInputKeysCtx } from "./footer-input-keys.ts";

const submitted: Extract<FooterMode, { kind: "input" }> = {
  kind: "input",
  prompt: "new:",
  edit: makeEdit("remember-me"),
  purpose: "new",
};

test("manual title typing preserves Unicode and rejects terminal controls", () => {
  let footer: FooterMode = { ...submitted, purpose: "worktree-title", edit: makeEdit("") };
  const type = (sequence: string) => handleFooterInputKey({ name: sequence, sequence } as KeyEvent, {
    footer,
    setFooter: (next) => { footer = typeof next === "function" ? next(footer) : next; },
  } as FooterInputKeysCtx);
  type("iOS 日本語 👩🏽‍💻 café\n\u0085\u2028\u2029\u0007\u202e\u2066\u2069");
  expect(footer.kind === "input" && footer.edit.value).toBe("iOS 日本語 👩🏽‍💻 café");
  type("\x1b[21~");
  expect(footer.kind === "input" && footer.edit.value).toBe("iOS 日本語 👩🏽‍💻 café");
  footer = { ...submitted, edit: makeEdit("") };
  type("slug-日本語");
  expect(footer.kind === "input" && footer.edit.value).toBe("slug-");
});

describe("restoreFailedCreateFooter", () => {
  test("restores submitted input when the footer is still idle", () => {
    expect(restoreFailedCreateFooter({ kind: "legend" }, submitted)).toBe(
      submitted,
    );
  });

  test("does not overwrite a later footer interaction", () => {
    const current: Extract<FooterMode, { kind: "input" }> = {
      kind: "input",
      prompt: "issue:",
      edit: makeEdit("COZ-9"),
      purpose: "issue-id",
    };

    expect(restoreFailedCreateFooter(current, submitted)).toBe(current);
  });
});
