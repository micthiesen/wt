import { theme } from "../../theme.ts";

/**
 * One detail row: fixed-width right-aligned label, value sized to
 * content but shrinkable with ellipsis when the pane is too narrow.
 * Optional trailing content (e.g. a staleness glyph) sits directly
 * after the value — not flush-right — and never shrinks.
 */
export function Row({
  label,
  children,
  trailing,
  labelWidth,
}: {
  label: string;
  children: React.ReactNode;
  trailing?: React.ReactNode;
  labelWidth: number;
}) {
  return (
    <box flexDirection="row">
      <box
        width={labelWidth}
        flexShrink={0}
        flexDirection="row"
        justifyContent="flex-end"
        paddingRight={1}
      >
        <text fg={theme.fgDim}>{label}</text>
      </box>
      <box flexShrink={1} overflow="hidden">
        {children}
      </box>
      {trailing ? <box flexShrink={0} paddingLeft={1}>{trailing}</box> : null}
    </box>
  );
}

/**
 * Right-aligned label width for the review-request body. Independent of
 * the configured `RESOLVED_ROWS`, so it sizes to its *own* labels (+2 gap)
 * rather than borrowing the narrower shared `LABEL_WIDTH`, which clipped
 * longer labels like "branch"/"review".
 */
export const RR_LABEL_WIDTH =
  ["state", "branch", "author", "diff", "status", "age"].reduce(
    (m, l) => Math.max(m, l.length),
    0,
  ) + 2;

/** One review-request row: shared wide label column. */
export function RRRow({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <Row label={label} labelWidth={RR_LABEL_WIDTH}>
      {children}
    </Row>
  );
}
