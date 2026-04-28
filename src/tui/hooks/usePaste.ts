import { useEffect, useRef } from "react";
import type { PasteEvent } from "@opentui/core";
import { decodePasteBytes } from "@opentui/core";
import { useRenderer } from "@opentui/react";

/**
 * Subscribe to bracketed-paste events. The terminal wraps a Cmd+V blob
 * in `\x1b[200~ ... \x1b[201~`; OpenTUI parses that into a `paste`
 * event on the renderer's key handler rather than a stream of
 * individual keypresses, so `useKeyboard` never sees pasted text. The
 * handler is held in a ref so the effect only subscribes once per
 * renderer lifetime.
 */
export function usePaste(handler: (text: string) => void): void {
  const renderer = useRenderer();
  const handlerRef = useRef(handler);
  handlerRef.current = handler;
  useEffect(() => {
    if (!renderer) return;
    const onPaste = (event: PasteEvent): void => {
      const text = decodePasteBytes(event.bytes);
      if (text) handlerRef.current(text);
    };
    renderer.keyInput.on("paste", onPaste);
    return () => {
      renderer.keyInput.off("paste", onPaste);
    };
  }, [renderer]);
}
