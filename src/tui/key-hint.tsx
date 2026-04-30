import type { ReactNode } from "react";

import { theme } from "./theme.ts";

export type KeyHintPair = [key: string, label: string];

type Props = {
  pairs: KeyHintPair[];
  separator?: string;
};

/**
 * Inline keystroke hint, e.g. `j/k move · ⏎ pick · esc cancel`.
 * Returns a Fragment of `<text>` nodes so the caller controls the
 * row container (typically `<box flexDirection="row">`).
 */
export function KeyHint({ pairs, separator = " · " }: Props) {
  const parts: ReactNode[] = [];
  pairs.forEach(([key, label], i) => {
    if (i > 0) {
      parts.push(
        <text key={`sep-${i}`} fg={theme.fgDim}>
          {separator}
        </text>,
      );
    }
    parts.push(
      <text key={`k-${i}`} fg={theme.accent} attributes={1}>
        {key}
      </text>,
    );
    parts.push(
      <text key={`l-${i}`} fg={theme.fgDim}>
        {" "}
        {label}
      </text>,
    );
  });
  return <>{parts}</>;
}
