import { config } from "./config.ts";

export const LINEAR_URL_RE = /linear\.app\/[^/]+\/issue\/([A-Z]+-\d+)/i;
export const LINEAR_ID_RE = /^[A-Z]+-\d+$/i;
export const LINEAR_SLUG_ID_RE = /([a-z]+-\d+)(?:-|$)/i;

export function linearUrlForSlug(slug: string): string | null {
  if (!config.linear) return null;
  const m = LINEAR_SLUG_ID_RE.exec(slug);
  if (!m || !m[1]) return null;
  return `https://linear.app/${config.linear.workspace}/issue/${m[1].toUpperCase()}`;
}
