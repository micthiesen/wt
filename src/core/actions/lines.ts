import {
  type ActionLine,
  type MessageEmit,
  MAX_BUFFERED_LINES,
} from "../harness/claude/events.ts";

/**
 * Apply one parser delta to a buffer snapshot. Patches by id (no-op
 * when the id has already been evicted past `MAX_BUFFERED_LINES`),
 * then appends. Cheap at our buffer scale (1000 lines, a handful of
 * patches per delta) and stays pure for reasoning.
 */
export function applyEmit(
  prev: readonly ActionLine[],
  emit: MessageEmit,
): ActionLine[] {
  const { append, patch } = emit;
  if (append.length === 0 && patch.length === 0) return prev.slice();
  let next: ActionLine[] = prev.slice();
  if (patch.length > 0) {
    const byId = new Map<number, ActionLine>();
    for (const p of patch) byId.set(p.id, p.line);
    next = next.map((l) => byId.get(l.id) ?? l);
  }
  if (append.length > 0) next = next.concat(append);
  return next;
}

export function capLines(lines: readonly ActionLine[]): readonly ActionLine[] {
  return lines.length > MAX_BUFFERED_LINES
    ? lines.slice(-MAX_BUFFERED_LINES)
    : lines;
}
