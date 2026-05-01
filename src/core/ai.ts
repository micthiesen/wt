/**
 * LM Studio client for worktree summaries.
 *
 * LM Studio exposes an OpenAI-compatible `/v1/chat/completions` endpoint;
 * we keep the call shape generic so any compatible local server (Ollama
 * with OpenAI bridge, llama.cpp's server, etc.) works with the same
 * config. No streaming — the TUI just waits for the full response.
 *
 * One call produces both a title and a description via a line-prefixed
 * format that's robust to small-model formatting drift. Co-generation
 * shares one round trip and one diff-context build per cache key.
 */
import { config } from "./config.ts";

const SYSTEM_PROMPT = `You summarise git changes for a developer scanning their worktrees.

Output format, exactly:
TITLE: <single line, 5 to 10 words, present-tense action or noun phrase>
BRIEF: <noun phrase, 2 to 4 words, max 24 characters, no leading verb>
DESCRIPTION: <1 to 3 sentences of plain prose>

Rules:
- TITLE: tight and descriptive, like a good PR title. No quotes. No trailing period.
- BRIEF: ultra-condensed for a narrow list view. Just the *subject* of the change — caveman talk. No verbs ("Add", "Implement", "Fix", "Refactor"...), no articles. Examples: "Auto-merge support" not "Add auto-merge support"; "Diff compactor" not "Refactor the diff compactor"; "Reviewer picker UI" not "Improve reviewer picker UI". Hard cap 24 characters.
- DESCRIPTION: describe what the change does, not which files it touches. No markdown, no headings, no lists.
- Skip filler like "This change..." or "The diff shows...". Lead with the action.
- If the changes feel exploratory or scaffolding, say so.

Return only the formatted output. Nothing before TITLE, nothing after the description.`;

type ChatResponse = {
  choices?: Array<{ message?: { content?: string } }>;
  error?: { message?: string };
};

export type AiSummary = {
  /** LLM-authored title. Null when the model failed to emit a TITLE: line. */
  title: string | null;
  /**
   * Ultra-short noun-phrase variant of the title for the worktree list,
   * where horizontal space after the issue ID and badge cluster can be
   * as tight as ~20 chars. Always set: parser falls back to the title,
   * then the description, when the model omits the BRIEF: line.
   */
  brief: string;
  /** LLM-authored description. Always set on success — falls back to the whole response if structure was missing. */
  description: string;
};

/**
 * Call the configured LLM endpoint with the prepared diff context.
 * Throws on transport / HTTP / parse errors so react-query surfaces
 * them; the details pane renders errors verbatim once retries are
 * exhausted.
 */
export async function summarizeDiff(prompt: string): Promise<AiSummary> {
  if (!config.ai) {
    throw new Error("AI is not configured ([ai] missing in config.toml)");
  }
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), config.ai.timeoutMs);
  let res: Response;
  try {
    res = await fetch(`${config.ai.endpoint}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      signal: ctrl.signal,
      body: JSON.stringify({
        model: config.ai.model,
        messages: [
          { role: "system", content: SYSTEM_PROMPT },
          { role: "user", content: prompt },
        ],
        temperature: 0.2,
        // Small bump from prior 200 to give title + description headroom
        // without inviting rambling. ~3 sentences fits comfortably here.
        max_tokens: 260,
        stream: false,
      }),
    });
  } catch (err) {
    throw new Error(
      `LM Studio unreachable at ${config.ai.endpoint}: ${err instanceof Error ? err.message : String(err)}`,
    );
  } finally {
    clearTimeout(timer);
  }

  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`LM Studio HTTP ${res.status}: ${body.slice(0, 200) || res.statusText}`);
  }

  let parsed: ChatResponse;
  try {
    parsed = (await res.json()) as ChatResponse;
  } catch (err) {
    throw new Error(
      `LM Studio returned non-JSON: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
  if (parsed.error?.message) {
    throw new Error(`LM Studio: ${parsed.error.message}`);
  }
  const content = parsed.choices?.[0]?.message?.content?.trim();
  if (!content) {
    throw new Error("LM Studio returned no content");
  }
  return parseTitleDescription(content);
}

/**
 * Lenient parser for the model's structured output.
 *
 * Markers are extracted independently so the model can emit them in any
 * order. When DESCRIPTION is missing, the body is whatever follows the
 * last single-line marker; when no markers are present, the whole
 * response becomes the description and title/brief fall back through
 * `brief = title ?? description`.
 *
 * Inline cleanup (`cleanInline`) strips wrapping quotes and trailing
 * periods on TITLE / BRIEF so the output reads cleanly in a terminal
 * even when the model adds them.
 */
export function parseTitleDescription(text: string): AiSummary {
  const trimmed = text.trim();
  const titleMatch = trimmed.match(/^TITLE:\s*(.+?)\s*$/m);
  const briefMatch = trimmed.match(/^BRIEF:\s*(.+?)\s*$/m);
  const descMatch = trimmed.match(/^DESCRIPTION:\s*([\s\S]+)$/m);

  const rawTitle = titleMatch?.[1]?.trim() ?? null;
  const title = rawTitle ? cleanInline(rawTitle) || null : null;
  const rawBrief = briefMatch?.[1]?.trim() ?? null;
  const parsedBrief = rawBrief ? cleanInline(rawBrief) || null : null;

  let description: string;
  if (descMatch) {
    description = descMatch[1]!.trim();
  } else {
    // No DESCRIPTION marker — take everything after the last single-line
    // marker as the body. Falls back to the whole response when no
    // markers were emitted at all.
    const lastMarkerEnd = Math.max(
      titleMatch ? titleMatch.index! + titleMatch[0]!.length : -1,
      briefMatch ? briefMatch.index! + briefMatch[0]!.length : -1,
    );
    description = lastMarkerEnd >= 0 ? trimmed.slice(lastMarkerEnd).trim() : trimmed;
  }

  // Brief is required at the type level; degrade gracefully when the
  // model skips it. Title is preferred (still a single-line phrase),
  // then a hard-truncated description tail.
  const brief = parsedBrief ?? title ?? description.slice(0, 24).trim();

  return { title, brief, description };
}

function cleanInline(t: string): string {
  return t
    .replace(/^["'`]+|["'`]+$/g, "")
    .replace(/\.+$/, "")
    .trim();
}
