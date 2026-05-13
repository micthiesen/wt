/**
 * Shared "live preview" focus math for modal pickers — `'` outputs
 * and `;` claude sessions. Both pickers want j/k to push the
 * highlighted item's output id into the bottom pane. Concentrating
 * the patch shape here keeps the two from drifting as the semantics
 * evolve.
 *
 * The helper returns a plain `{ focused }` partial and lets the
 * caller dispatch it through its own `setFocus`. `null` from a null
 * `outputId` signals "no previewable output for this item" (ghost
 * session, "+ new" affordance, missing entry); the caller should
 * treat that as a silent no-op rather than nuke the existing pane.
 */

export type FocusPatch = {
  focused: string | null;
};

/**
 * Patch for landing the picker cursor on `outputId`. Null `outputId`
 * → null patch (caller leaves focus alone).
 */
export function previewFocusPatch(
  outputId: string | null,
): FocusPatch | null {
  if (outputId === null) return null;
  return { focused: outputId };
}
