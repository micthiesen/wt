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
