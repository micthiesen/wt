import { createHash } from "node:crypto";
import { config } from "./config.ts";

// Driven by `config.branch.id_pattern`. Must include exactly one
// capturing group around the numeric portion — `computeStage` reads
// `m[1]` to build deterministic, issue-id-aware stage names.
const ISSUE_PREFIX_RE = new RegExp(config.branch.idPattern, "i");

function truncateSlug(s: string, limit = config.branch.slugMaxLen): string {
  if (s.length <= limit) return s;
  // Prefer a clean cut at the last hyphen within the limit; require at
  // least half the budget so we don't produce a stump.
  const cut = s.lastIndexOf("-", limit);
  if (cut >= Math.floor(limit / 2)) return s.slice(0, cut);
  return s.slice(0, limit);
}

export function dirSlug(branch: string): string {
  // Charset note (audited, accepted): unlike `slugify` (which gates
  // NEW-branch input), this only collapses `/` — a foreign branch's
  // other special characters survive into the slug. Slugs stay single
  // path components (no traversal) and every downstream tmux/git call
  // is argv-based, so the loose charset is safe; tightening it would
  // silently re-home existing worktree dirs for the same branch.
  const prefix = config.branch.prefix;
  // Always strip our own prefix — these are "mine", no need to
  // namespace the directory.
  if (branch.startsWith(`${prefix}/`)) {
    return truncateSlug(branch.slice(prefix.length + 1).replace(/\//g, "-"));
  }
  // For other authors, strip the `<name>/` only when an issue ID
  // follows — the ID keeps the slug identifiable without the
  // namespace. Non-issue foreign branches keep the prefix (collapsed
  // to `-`) so slugs from different authors don't collide.
  const slash = branch.indexOf("/");
  if (slash !== -1) {
    const rest = branch.slice(slash + 1);
    if (ISSUE_PREFIX_RE.test(rest)) {
      return truncateSlug(rest.replace(/\//g, "-"));
    }
  }
  return truncateSlug(branch.replace(/\//g, "-"));
}

/**
 * Deterministic stage name.
 *
 * - With Linear ID prefix: `michael-<num>-<hash6>` (e.g. michael-4888-a1b2c3)
 * - Without:               `michael-<hash10>`
 */
export function computeStage(slug: string): string {
  const slugLower = slug.toLowerCase();
  const digest = createHash("sha256").update(slugLower).digest("hex");
  const m = ISSUE_PREFIX_RE.exec(slugLower);
  if (m) return `${config.stage.prefix}${m[1]}-${digest.slice(0, 6)}`;
  return `${config.stage.prefix}${digest.slice(0, 10)}`;
}

export function stageUrl(stage: string): string | null {
  const domain = config.stage.domain;
  if (!domain) return null;
  return `https://${stage}.${domain}/`;
}

export function slugify(s: string): string {
  return s
    .toLowerCase()
    .trim()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

/**
 * Decompose a slug into an issue ID (uppercased, e.g. `ENG-1234`) and
 * the remaining descriptive part with dashes turned into spaces and the
 * first letter capitalized — `eng-1234-do-foo-and-bar` becomes
 * `{ id: "ENG-1234", rest: "Do foo and bar" }`. Slugs without an issue
 * prefix yield `{ id: null, rest: "Do foo and bar" }`. Used by the list
 * panel as the no-AI fallback label.
 */
export function slugLabel(slug: string): { id: string | null; rest: string } {
  const m = ISSUE_PREFIX_RE.exec(slug);
  let id: string | null = null;
  let trimmed = slug;
  if (m) {
    id = m[0].replace(/-$/, "").toUpperCase();
    trimmed = slug.slice(m[0].length);
  }
  const words = trimmed.replace(/-/g, " ").trim();
  const rest = words ? words[0]!.toUpperCase() + words.slice(1) : "";
  return { id, rest };
}

export function capitalizeFirst(s: string): string {
  if (!s) return s;
  return s[0]!.toUpperCase() + s.slice(1);
}
