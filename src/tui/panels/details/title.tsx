import type { TitleSource } from "../../hooks/useWorktreeRows.ts";
import { truncateEnd, wrapText } from "../../text.ts";
import { theme } from "../../theme.ts";

/**
 * The border title identifies the slug, not the human title already shown
 * on the selected list row. OpenTUI drops an over-wide border title instead
 * of clipping it, so both live and removed details end-truncate with margin.
 */
export function detailPaneTitle(slug: string, width: number, suffix = ""): string {
  return ` ${truncateEnd(`${slug}${suffix}`, Math.max(0, width - 8))} `;
}

/** Full title above the status banner; wrap to this pane, not the terminal. */
export function DetailTitleLine({ title, source, contentWidth }: {
  title: string;
  source?: TitleSource;
  contentWidth: number;
}) {
  const lines = wrapText(title, Math.max(1, contentWidth));
  const sourceText = source ? ` (${source})` : "";
  const sourceFits = Bun.stringWidth(lines.at(-1) ?? "") + Bun.stringWidth(sourceText) <= contentWidth;
  return (
    <box flexShrink={0} overflow="hidden">
      <text fg={theme.fgBright} wrapMode="none">
        {lines.join("\n")}
        {source ? <span fg={theme.fgDim}>{sourceFits ? sourceText : `\n${wrapText(sourceText.trim(), Math.max(1, contentWidth)).join("\n")}`}</span> : null}
      </text>
    </box>
  );
}
