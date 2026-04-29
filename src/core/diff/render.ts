/**
 * Pure transformations from a parsed `Part` to its rendered string at
 * a given mode. No git, no filesystem — feed it a block and a mode,
 * get text. The fit loop calls these many times so they're cheap and
 * deterministic; results are cached upstream by (path, mode) key.
 */
import type { FileMode, Part } from "./parts.ts";

/**
 * Render a part at the given mode. Always emits a trailing newline
 * so the next part's `diff --git` header lands on its own line; the
 * fit loop concatenates the results without further separator.
 */
export function renderPart(part: Part, mode: FileMode): string {
  if (mode === "dropped") return "";
  let text: string;
  switch (mode) {
    case "full":
      text = part.raw;
      break;
    case "tight":
      text = stripContext(part.raw);
      break;
    case "hunks":
      text = hunkHeadersOnly(part.raw);
      break;
  }
  return text.endsWith("\n") ? text : `${text}\n`;
}

/**
 * `tight`: drop unchanged-context lines (those that start with a
 * single space). Keeps `+`/`-` content, hunk anchors, and file
 * headers. Cheap way to roughly halve a block's size while still
 * showing what was added or removed.
 */
function stripContext(block: string): string {
  const out: string[] = [];
  for (const line of block.split("\n")) {
    if (line.startsWith(" ")) continue;
    out.push(line);
  }
  return out.join("\n");
}

/**
 * `hunks`: keep only file headers and hunk anchors (`@@ … @@ funcname`),
 * discard the actual line-by-line content. With `-W` enabled upstream,
 * each hunk's anchor includes the enclosing function name, so this
 * mode reads as "list of functions that changed in each file" — small
 * and surprisingly informative.
 */
function hunkHeadersOnly(block: string): string {
  const out: string[] = [];
  for (const line of block.split("\n")) {
    if (
      line.startsWith("diff --git") ||
      line.startsWith("index ") ||
      line.startsWith("--- ") ||
      line.startsWith("+++ ") ||
      line.startsWith("@@") ||
      line.startsWith("similarity index") ||
      line.startsWith("rename ") ||
      line.startsWith("new file") ||
      line.startsWith("deleted file") ||
      line.startsWith("copy from") ||
      line.startsWith("copy to") ||
      line.startsWith("Binary files")
    ) {
      out.push(line);
    }
  }
  return out.join("\n");
}
