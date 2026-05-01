import type { ReactNode } from "react";

import { KeyHint, type KeyHintPair } from "./key-hint.tsx";
import { theme } from "./theme.ts";

type Percent = `${number}%`;

type Inset = {
  top?: Percent;
  right?: Percent;
  bottom?: Percent;
  left?: Percent;
};

const DEFAULT_INSET: Required<Inset> = {
  top: "20%",
  right: "20%",
  bottom: "20%",
  left: "20%",
};

type Props = {
  /** Title format: `name [· subtitle]`. Never include keystroke hints. */
  title: string;
  /**
   * Border + title color. Defaults to `theme.accent` (non-destructive).
   * Use `theme.warn` for confirm-before-irreversible-action modals.
   */
  borderColor?: string;
  /** Viewport-relative padding. Smaller values yield a larger modal. */
  inset?: Inset;
  /**
   * Keystroke hints rendered along the bottom edge. Pass an empty
   * array only if the modal has no dismiss path (it always has at
   * least one — esc/q/ctrl+c are universal).
   */
  hints: KeyHintPair[];
  children: ReactNode;
};

/**
 * Modal conventions every caller should follow:
 *
 *   1. **Toggle dismiss.** The key that opens the modal also closes it
 *      (e.g. `?` opens & closes help, `y` opens & closes the yank chord,
 *      `v` opens & closes the reviewer picker). Always accept it
 *      alongside the universal `esc` / `q` / `ctrl+c` dismiss keys, so
 *      muscle-memory works in both directions.
 *   2. **Universal dismiss.** Always accept `esc`, `q`, and `ctrl+c`.
 *   3. **Hints.** List dismiss keys in the `hints` prop so the user
 *      sees them along the bottom edge.
 */
export function Modal({
  title,
  borderColor = theme.accent,
  inset,
  hints,
  children,
}: Props) {
  const i = { ...DEFAULT_INSET, ...inset };
  return (
    <box
      position="absolute"
      top={i.top}
      left={i.left}
      right={i.right}
      bottom={i.bottom}
      zIndex={10}
      backgroundColor={theme.bg}
      border
      borderStyle="double"
      borderColor={borderColor}
      title={` ${title} `}
      titleAlignment="left"
      padding={1}
      flexDirection="column"
    >
      <box flexDirection="column" flexGrow={1} overflow="hidden">
        {children}
      </box>
      <box flexShrink={0} flexDirection="row" marginTop={1}>
        <KeyHint pairs={hints} />
      </box>
    </box>
  );
}
