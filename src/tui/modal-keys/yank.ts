import type { KeyEvent } from "@opentui/core";

import { yankItemsFor } from "../panels/yank.tsx";
import type { SimpleModalContext } from "./ctx.ts";

export function handleYankKey(
  k: KeyEvent,
  { setModal, current, doYank }: SimpleModalContext,
): boolean {
  if (
    k.name === "escape" ||
    k.sequence === "y" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
    return true;
  }
  if (current) {
    const item = yankItemsFor(current).find((it) => it.key === k.sequence);
    if (item) {
      setModal(null);
      doYank(current.wt.slug, item.label, item.value);
    }
  }
  return true;
}
