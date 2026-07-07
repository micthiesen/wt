/**
 * AI client for worktree summaries.
 *
 * Supports OpenAI-compatible `/v1/chat/completions` endpoints (LM Studio,
 * Ollama with OpenAI bridge, llama.cpp's server, etc.) and Google's Gemini
 * `models.generateContent` endpoint. No streaming — the TUI just waits for
 * the full response.
 *
 * One call produces both a title and a description via a line-prefixed
 * format that's robust to small-model formatting drift. Co-generation
 * shares one round trip and one diff-context build per cache key.
 */
import { config, type AiConfig } from "./config.ts";
import { chainSignal } from "./proc.ts";

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

const STACK_SYSTEM_PROMPT = `You name a group of related git branches for a section header in a developer tool.

Output exactly:
TITLE: <name>

Rules:
- Find the common theme — what unifies the branches. Often a feature, subsystem, or area they all touch.
- TITLE: 4 words maximum. Caveman noun phrase, no leading verb ("Add", "Fix", "Refactor"...), no articles, no quotes, no trailing period.
- Examples: "Auto-merge support", "Markdown link popover", "Reviewer picker UI", "Atomic builder claim".
- Name the WORK, not its packaging: never echo words from these instructions ("stack", "branch", "section", "header", "TUI", "group") unless the changes themselves are about that concept.
- If the branches look unrelated, pick the most prominent shared theme rather than listing them.

Return only the TITLE line.`;

type ChatResponse = {
  choices?: Array<{ message?: { content?: string } }>;
  error?: { message?: string };
};

type GeminiResponse = {
  candidates?: Array<{
    content?: {
      parts?: Array<{ text?: string }>;
    };
  }>;
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
 *
 * When `external` is provided (queryFn `signal`), it's chained with
 * the timeout so observer cancellation aborts the in-flight LM call.
 * Without this, switching worktrees fast leaves the prior
 * megabyte-prompt request running to completion against LM Studio,
 * burning latency on a result nobody sees.
 */
export async function summarizeDiff(
  prompt: string,
  external?: AbortSignal,
): Promise<AiSummary> {
  // max_tokens bump from prior 200 to give title + description headroom
  // without inviting rambling. ~3 sentences fits comfortably here.
  const content = await callChat(SYSTEM_PROMPT, prompt, 260, external);
  return parseTitleDescription(content);
}

/**
 * Stack-naming round trip. Same client as `summarizeDiff` but a
 * different system prompt and a tiny `max_tokens` since the output is
 * just one line. Input shape is a list of branch summaries; ordering
 * is irrelevant to the model so callers can sort for cache stability.
 *
 * Returns the cleaned title with a hard 6-word ceiling (≤4 prompted,
 * extra slack absorbs small models that overshoot). Throws on
 * transport / HTTP errors; if the model emits a non-TITLE response
 * the whole content is used as a last-resort fallback.
 */
export async function summarizeStack(
  members: ReadonlyArray<{ branch: string; brief: string }>,
  external?: AbortSignal,
): Promise<string> {
  const userPrompt = `Branches in this stack:\n${members
    .map((m) => `- ${m.branch}: ${m.brief}`)
    .join("\n")}`;
  // A small local model routinely ignores the "never echo TUI/stack/…"
  // rule and hands back a title made purely of the prompt's own
  // packaging vocabulary ("TUI", "Header Stack Section", …). Because
  // titles cache forever under the membership signature, one such answer
  // sticks permanently. Detect a meta-only title, nudge once with the
  // rejected text quoted back, and if it still won't name the work,
  // throw — the section falls back to its bare issue label (ENG-5202)
  // rather than baking in junk.
  let lastRejected: string | null = null;
  for (let attempt = 0; attempt < 2; attempt++) {
    external?.throwIfAborted();
    const prompt = lastRejected
      ? `${userPrompt}\n\nYour previous answer "${lastRejected}" just echoed words from the instructions. Name the actual WORK these branches do, not the tool or the grouping.`
      : userPrompt;
    const content = await callChat(STACK_SYSTEM_PROMPT, prompt, 30, external);
    const titleMatch = content.match(/^TITLE:\s*(.+?)\s*$/m);
    const raw = (titleMatch?.[1] ?? content).trim();
    const cleaned = cleanInline(raw);
    // Hard ceiling — 4 prompted, slight slack for small-model drift.
    const capped = cleaned.split(/\s+/).slice(0, 6).join(" ");
    if (capped && !isStackTitleMetaOnly(capped)) return capped;
    lastRejected = capped || raw;
  }
  throw new Error(
    `stack title: model only echoed meta-vocabulary ("${lastRejected}")`,
  );
}

/**
 * Words the stack-naming prompt uses to describe *itself* (the tool, the
 * packaging, the grouping) rather than any change. A leaked title is one
 * built entirely from these — "TUI", "Header Stack Section" — with no
 * domain word to anchor it.
 *
 * The test is all-or-nothing on purpose: reject only when *every* token
 * is meta. A single real word saves the title, so "Header Stamp" survives
 * even though "header" is on the list (eng-5202-02 genuinely stamps a
 * header) — we never strip individual tokens, which would corrupt a
 * legitimately header-themed stack down to "Stamp".
 */
const STACK_TITLE_META_WORDS = new Set([
  "tui", "stack", "stacks", "branch", "branches", "section", "sections",
  "header", "headers", "group", "groups", "grouping", "developer", "tool",
  "tools", "feature", "features", "subsystem", "subsystems", "area", "areas",
]);

export function isStackTitleMetaOnly(title: string): boolean {
  const tokens = title
    .toLowerCase()
    .split(/\s+/)
    .map((t) => t.replace(/[^a-z]/g, ""))
    .filter(Boolean);
  return tokens.length > 0 && tokens.every((t) => STACK_TITLE_META_WORDS.has(t));
}

/**
 * Module-level serial queue over the configured AI endpoint. A restack /
 * rebase flips many diff hashes at once, and the resulting burst of
 * concurrent summary fetches can stampede the model into request timeouts.
 * One in-flight request at a time keeps each call fast and the failure mode
 * boring. Tasks run on settled predecessors, so one failure doesn't poison
 * the queue.
 */
let chatQueueTail: Promise<unknown> = Promise.resolve();

function enqueueChat<T>(task: () => Promise<T>): Promise<T> {
  const next = chatQueueTail.then(task, task);
  chatQueueTail = next.catch(noop);
  return next;
}

/**
 * Single round-trip to the configured AI endpoint. Shared by
 * `summarizeDiff` and `summarizeStack` so both
 * use the same abort chaining, timeout handling, and error messages.
 *
 * Calls are serialized through `enqueueChat`, and the per-call timeout
 * starts when the request actually goes out — not while it waits in the
 * queue, which would re-create the stampede failure with extra steps.
 * A failed attempt gets one retry (transient resets from a busy /
 * model-swapping server recover on the spot); an external abort — the
 * observer was cancelled, nobody wants the result — does not.
 */
async function callChat(
  systemPrompt: string,
  userPrompt: string,
  maxTokens: number,
  external?: AbortSignal,
): Promise<string> {
  const ai = config.ai;
  if (!ai) {
    throw new Error("AI is not configured ([ai] missing in config.toml)");
  }
  return enqueueChat(async () => {
    let lastErr: unknown;
    for (let attempt = 0; attempt < 2; attempt++) {
      // The caller may have been cancelled while this call sat in the
      // queue (or during the retry pause) — bail before sending.
      external?.throwIfAborted();
      if (attempt > 0) await sleep(500);
      try {
        return ai.provider === "gemini"
          ? await requestGeminiChat(ai, systemPrompt, userPrompt, maxTokens, external)
          : await requestOpenAiChat(ai, systemPrompt, userPrompt, maxTokens, external);
      } catch (err) {
        if (external?.aborted) throw err;
        lastErr = err;
      }
    }
    throw lastErr;
  });
}

/** One HTTP attempt against the chat endpoint. Timeout + abort scoped
 *  to this attempt; retry policy lives in `callChat`. */
async function requestOpenAiChat(
  ai: Extract<AiConfig, { provider: "openai" }>,
  systemPrompt: string,
  userPrompt: string,
  maxTokens: number,
  external?: AbortSignal,
): Promise<string> {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), ai.timeoutMs);
  // Forward an external abort (queryFn cancellation) into the same
  // controller so fetch sees a single signal.
  const cleanupAbort = external
    ? chainSignal(external, () => ctrl.abort())
    : noop;
  let res: Response;
  try {
    res = await fetch(`${ai.endpoint}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      signal: ctrl.signal,
      body: JSON.stringify({
        model: ai.model,
        messages: [
          { role: "system", content: systemPrompt },
          { role: "user", content: userPrompt },
        ],
        temperature: 0.2,
        max_tokens: maxTokens,
        stream: false,
      }),
    });
  } catch (err) {
    // One message for every fetch failure, including the timeout abort —
    // so "unreachable" can also mean "timed out after ai.timeout_ms".
    // Known conflation, accepted: both cases have the same user remedy
    // (check LM Studio / bump the timeout) and the appended err.message
    // carries the distinction when it matters.
    throw new Error(
      `AI endpoint unreachable at ${ai.endpoint}: ${err instanceof Error ? err.message : String(err)}`,
    );
  } finally {
    clearTimeout(timer);
    cleanupAbort();
  }

  if (!res.ok) {
    const body = await res.text().catch(() => "");
    // Squash to one line — LM Studio 500s return a full HTML error
    // page, which otherwise dumps line-by-line into the activity pane.
    const oneLine = body.replace(/\s+/g, " ").trim().slice(0, 160);
    throw new Error(`AI endpoint HTTP ${res.status}: ${oneLine || res.statusText}`);
  }

  let parsed: ChatResponse;
  try {
    parsed = (await res.json()) as ChatResponse;
  } catch (err) {
    throw new Error(
      `AI endpoint returned non-JSON: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
  if (parsed.error?.message) {
    throw new Error(`AI endpoint: ${parsed.error.message}`);
  }
  const content = parsed.choices?.[0]?.message?.content?.trim();
  if (!content) {
    throw new Error("AI endpoint returned no content");
  }
  return content;
}

async function requestGeminiChat(
  ai: Extract<AiConfig, { provider: "gemini" }>,
  systemPrompt: string,
  userPrompt: string,
  maxTokens: number,
  external?: AbortSignal,
): Promise<string> {
  const apiKey = process.env[ai.apiKeyEnv];
  if (!apiKey) {
    throw new Error(`Gemini API key env var ${ai.apiKeyEnv} is not set`);
  }
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), ai.timeoutMs);
  const cleanupAbort = external
    ? chainSignal(external, () => ctrl.abort())
    : noop;
  let res: Response;
  try {
    res = await fetch(
      `${ai.endpoint}/${geminiModelPath(ai.model)}:generateContent?key=${encodeURIComponent(apiKey)}`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        signal: ctrl.signal,
        body: JSON.stringify({
          systemInstruction: {
            parts: [{ text: systemPrompt }],
          },
          contents: [
            {
              role: "user",
              parts: [{ text: userPrompt }],
            },
          ],
          generationConfig: {
            temperature: 0.2,
            maxOutputTokens: maxTokens,
          },
        }),
      },
    );
  } catch (err) {
    throw new Error(
      `Gemini unreachable at ${ai.endpoint}: ${err instanceof Error ? err.message : String(err)}`,
    );
  } finally {
    clearTimeout(timer);
    cleanupAbort();
  }

  if (!res.ok) {
    const body = await res.text().catch(() => "");
    const oneLine = body.replace(/\s+/g, " ").trim().slice(0, 160);
    throw new Error(`Gemini HTTP ${res.status}: ${oneLine || res.statusText}`);
  }

  let parsed: GeminiResponse;
  try {
    parsed = (await res.json()) as GeminiResponse;
  } catch (err) {
    throw new Error(
      `Gemini returned non-JSON: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
  if (parsed.error?.message) {
    throw new Error(`Gemini: ${parsed.error.message}`);
  }
  const content = parsed.candidates?.[0]?.content?.parts
    ?.map((part) => part.text ?? "")
    .join("")
    .trim();
  if (!content) {
    throw new Error("Gemini returned no content");
  }
  return content;
}

function geminiModelPath(model: string): string {
  return model.startsWith("models/") ? model : `models/${model}`;
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

const noop = (): void => {};

const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));

function cleanInline(t: string): string {
  return t
    .replace(/^["'`]+|["'`]+$/g, "")
    .replace(/\.+$/, "")
    .trim();
}
