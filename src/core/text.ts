export function pluralize(count: number, singular: string, plural?: string): string {
  return `${count} ${count === 1 ? singular : (plural ?? `${singular}s`)}`;
}

/** Human-readable elapsed time, never raw floating-point milliseconds. */
export function formatDuration(ms: number): string {
  if (!Number.isFinite(ms)) return "unknown duration";
  const rounded = Math.max(0, Math.round(ms));
  if (rounded < 1000) return `${rounded}ms`;
  if (rounded < 60_000) return `${Number((rounded / 1000).toFixed(1))}s`;
  const seconds = Math.round(rounded / 1000);
  return `${Math.floor(seconds / 60)}m${String(seconds % 60).padStart(2, "0")}s`;
}
