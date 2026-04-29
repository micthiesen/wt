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
DESCRIPTION: <1 to 3 sentences of plain prose>

Rules:
- TITLE: tight and descriptive, like a good PR title. No quotes. No trailing period.
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
 * Cases handled:
 *   - "TITLE: foo\nDESCRIPTION: bar" — happy path, both extracted.
 *   - "TITLE: foo\nbar baz" — no DESCRIPTION marker; everything after
 *     the title line becomes the description.
 *   - "DESCRIPTION: bar" — title null, description extracted.
 *   - "bar baz" — no markers; whole thing becomes the description,
 *     title null. Caller falls back to PR title / first-commit subject.
 *
 * Title cleanup strips wrapping quotes and trailing periods so the
 * output reads cleanly in a terminal even when the model adds them.
 */
export function parseTitleDescription(text: string): AiSummary {
  const trimmed = text.trim();
  const titleMatch = trimmed.match(/^TITLE:\s*(.+?)\s*$/m);
  const rawTitle = titleMatch?.[1]?.trim() ?? null;
  const title = rawTitle ? cleanTitle(rawTitle) : null;

  const descMatch = trimmed.match(/^DESCRIPTION:\s*([\s\S]+)$/m);
  let description: string;
  if (descMatch) {
    description = descMatch[1]!.trim();
  } else if (titleMatch) {
    // Title present but no DESCRIPTION marker — take everything after
    // the title line as the body.
    const after = trimmed.slice(titleMatch.index! + titleMatch[0]!.length);
    description = after.trim();
  } else {
    // No structure at all; treat the whole response as description.
    description = trimmed;
  }
  return { title: title || null, description };
}

function cleanTitle(t: string): string {
  return t
    .replace(/^["'`]+|["'`]+$/g, "")
    .replace(/\.+$/, "")
    .trim();
}
