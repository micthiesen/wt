/**
 * Shared "live preview + pin toggle" focus math for modal pickers —
 * `:` outputs and `;` claude sessions. Both pickers want j/k to
 * push the highlighted item's output id into the bottom pane and
 * `'` to toggle pin on that id. Concentrating the patch shape here
 * keeps the two from drifting as the semantics evolve.
 *
 * The helpers return plain `{ focused, pinned }` partials and let
 * the caller dispatch them through its own `setFocus` — no React
 * state, no slug awareness, no knowledge of bucket storage. `null`
 * from either function signals "no previewable output for this
 * item" (ghost session, "+ new" affordance, missing entry); the
 * caller should treat that as a silent no-op rather than nuke the
 * existing pane.
 */

export type FocusPatch = {
  focused: string | null;
  pinned: string | null;
};

/**
 * Patch for landing the picker cursor on `outputId`. Always clears
 * pin — live preview overrides a prior pin so navigation actually
 * shows. Null `outputId` → null patch (caller leaves focus alone).
 */
export function previewFocusPatch(
  outputId: string | null,
): FocusPatch | null {
  if (outputId === null) return null;
  return { focused: outputId, pinned: null };
}

/**
 * Patch for toggling pin on `outputId`. Pinning a previously-
 * unpinned id sets it as both focused and pinned; pinning the
 * already-pinned id clears pin (focused stays so the pane keeps
 * showing it). Null `outputId` → null patch.
 */
export function togglePinPatch(
  outputId: string | null,
  currentPinned: string | null,
): FocusPatch | null {
  if (outputId === null) return null;
  return {
    focused: outputId,
    pinned: currentPinned === outputId ? null : outputId,
  };
}
