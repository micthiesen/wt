/**
 * Discord #updates digest — run by .github/workflows/discord-digest.yml
 * after the debounce sleep. Collects the commits landed on main since
 * the last successful digest, has a cheap OpenAI model turn their
 * (already AI-written) titles + bodies into a short human-voiced update
 * note, and posts it as one embed via the channel webhook. No diffs are
 * ever sent to the model.
 *
 * State: the "since" boundary is the head SHA of this workflow's own
 * last successful run, fetched from the Actions API. A failed or
 * cancelled digest doesn't advance it, so its commits roll into the
 * next digest. First run ever (no prior success) falls back to the
 * last 10 commits.
 *
 * Env: GITHUB_TOKEN, GITHUB_REPOSITORY, HEAD_SHA, DISCORD_WEBHOOK,
 * OPENAI_API_KEY (optional — missing/failed OpenAI falls back to a
 * plain list of commit titles), DRY_RUN=1 prints instead of posting.
 */

const repo = requireEnv("GITHUB_REPOSITORY");
const headSha = requireEnv("HEAD_SHA");
const ghToken = requireEnv("GITHUB_TOKEN");
const dryRun = process.env.DRY_RUN === "1";

const OPENAI_MODEL = process.env.OPENAI_MODEL || "gpt-6-luna";
/** Bound the model input however big the burst was. */
const MAX_COMMITS = 30;
const MAX_BODY_CHARS = 700;

function requireEnv(name: string): string {
  const v = process.env[name];
  if (!v) {
    console.error(`missing env: ${name}`);
    process.exit(1);
  }
  return v;
}

async function gh<T>(path: string): Promise<T> {
  const res = await fetch(`https://api.github.com${path}`, {
    headers: {
      Authorization: `Bearer ${ghToken}`,
      Accept: "application/vnd.github+json",
      "X-GitHub-Api-Version": "2022-11-28",
    },
  });
  if (!res.ok) {
    throw new Error(`GitHub ${path}: ${res.status} ${await res.text()}`);
  }
  return (await res.json()) as T;
}

type ApiCommit = {
  sha: string;
  parents: { sha: string }[];
  commit: { message: string; author: { name: string } | null };
  author: { login: string } | null;
};

/** Head SHA of the last successful digest run, or null on the first run. */
async function lastDigestSha(): Promise<string | null> {
  try {
    const runs = await gh<{ workflow_runs: { head_sha: string }[] }>(
      `/repos/${repo}/actions/workflows/discord-digest.yml/runs` +
        `?status=success&branch=main&per_page=1`,
    );
    return runs.workflow_runs[0]?.head_sha ?? null;
  } catch (err) {
    // 404 until the workflow has run once; treat any lookup failure as
    // "first run" and let the recent-commits fallback bound the window.
    console.error(`last-run lookup failed: ${String(err)}`);
    return null;
  }
}

async function collectCommits(): Promise<{ commits: ApiCommit[]; url: string }> {
  const since = await lastDigestSha();
  if (since && since !== headSha) {
    try {
      const cmp = await gh<{ html_url: string; commits: ApiCommit[] }>(
        `/repos/${repo}/compare/${since}...${headSha}`,
      );
      return { commits: cmp.commits, url: cmp.html_url };
    } catch (err) {
      // A force-push (or expired sha) breaks the compare; fall through
      // to the recent-commits fallback rather than dying silent.
      console.error(`compare ${since}...${headSha} failed: ${String(err)}`);
    }
  } else if (since === headSha) {
    return { commits: [], url: "" };
  }
  const recent = await gh<ApiCommit[]>(
    `/repos/${repo}/commits?sha=${headSha}&per_page=10`,
  );
  return {
    commits: recent.reverse(),
    url: `https://github.com/${repo}/commits/main`,
  };
}

function commitNotes(commits: ApiCommit[]): string {
  return commits
    .map((c) => {
      const [title = "", ...rest] = c.commit.message.split("\n");
      const body = rest.join("\n").trim().slice(0, MAX_BODY_CHARS);
      return body ? `- ${title}\n${body.replace(/^/gm, "  ")}` : `- ${title}`;
    })
    .join("\n");
}

const SYSTEM_PROMPT = `You write the #updates posts for the Discord server of wt, \
an open-source terminal UI for keeping many git worktrees (and their PRs, CI, \
dev servers, and coding-agent sessions) in flight at once. Input: the commit \
titles and descriptions that landed on main since the last post. Write the \
update note: 2-4 plain sentences, or up to 6 short bullet lines when the \
changes are unrelated. Lead with what changed for people using wt; fold \
internal refactors into a clause or drop them. No hype, no emoji, no headers, \
no greeting, no "this update". Discord markdown is fine (backticks for \
commands/config keys). Stay under 900 characters.`;

async function summarize(commits: ApiCommit[]): Promise<string> {
  const fallback = commits
    .map((c) => `- ${c.commit.message.split("\n")[0]}`)
    .join("\n")
    .slice(0, 3900);
  const key = process.env.OPENAI_API_KEY;
  if (!key) {
    console.error("no OPENAI_API_KEY; posting raw commit titles");
    return fallback;
  }
  try {
    const res = await fetch("https://api.openai.com/v1/chat/completions", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${key}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({
        model: OPENAI_MODEL,
        max_completion_tokens: 500,
        messages: [
          { role: "system", content: SYSTEM_PROMPT },
          { role: "user", content: commitNotes(commits) },
        ],
      }),
    });
    if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
    const data = (await res.json()) as {
      choices: { message: { content: string | null } }[];
    };
    const text = data.choices[0]?.message.content?.trim();
    if (!text) throw new Error("empty completion");
    return text.slice(0, 3900);
  } catch (err) {
    // The channel staying alive beats the prose: post titles instead.
    console.error(`OpenAI failed, posting raw commit titles: ${String(err)}`);
    return fallback;
  }
}

function authorsLine(commits: ApiCommit[]): string {
  const names = [
    ...new Set(
      commits.map((c) =>
        c.author?.login ? `@${c.author.login}` : (c.commit.author?.name ?? "unknown"),
      ),
    ),
  ];
  if (names.length > 1) {
    return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
  }
  return names[0] ?? "unknown";
}

const { commits: all, url } = await collectCommits();
const commits = all.filter((c) => c.parents.length <= 1).slice(-MAX_COMMITS);
if (commits.length === 0) {
  console.log("no new commits since the last digest; nothing to post");
  process.exit(0);
}

const description = await summarize(commits);
const payload = {
  embeds: [
    {
      title: "What's new in wt",
      url,
      description,
      color: 0x5865f2,
      footer: {
        text:
          `${commits.length} commit${commits.length === 1 ? "" : "s"}` +
          ` by ${authorsLine(commits)}`,
      },
      timestamp: new Date().toISOString(),
    },
  ],
};

if (dryRun) {
  console.log(JSON.stringify(payload, null, 2));
  process.exit(0);
}

const webhook = requireEnv("DISCORD_WEBHOOK");
const post = await fetch(webhook, {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify(payload),
});
if (!post.ok) {
  console.error(`Discord webhook failed: ${post.status} ${await post.text()}`);
  process.exit(1);
}
console.log(`posted digest of ${commits.length} commits`);
