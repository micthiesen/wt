import type { TitleSource } from "../../hooks/useWorktreeRows.ts";
import { truncateEnd } from "../../text.ts";
import { theme } from "../../theme.ts";

/**
 * The border title identifies the slug, not the human title already shown
 * on the selected list row. OpenTUI drops an over-wide border title instead
 * of clipping it, so both live and removed details end-truncate with margin.
 */
export function detailPaneTitle(slug: string, width: number, suffix = ""): string {
  return ` ${truncateEnd(`${slug}${suffix}`, Math.max(0, width - 8))} `;
}

/** The title occupies one row above the work-status banner in both views. */
export function DetailTitleLine({ title, source }: { title: string; source?: TitleSource }) {
  return (
    <box flexShrink={0} overflow="hidden">
      <text fg={theme.fgBright} wrapMode="none" truncate>
        {title}
        {source ? <span fg={theme.fgDim}>{` (${source})`}</span> : null}
      </text>
    </box>
  );
}
