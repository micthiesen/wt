/**
 * Reviewer-picker flows (`v`): build the candidate list, and submit the
 * add/remove set through the optimistic github mutation. Extracted from
 * `app.tsx`; rebuilt per render so the closures see fresh rows + modal.
 */
import type { QueryFilters } from "@tanstack/react-query";

import { editReviewers } from "../../core/github.ts";
import { createLogger } from "../../core/logger.ts";
import type { Contributor } from "../../core/types.ts";
import { patchPullRequest, type GithubData } from "../../state/index.ts";
import type { Modal } from "../modal-state.ts";
import type { MultiPickerItem } from "../panels/picker.tsx";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import { theme } from "../theme.ts";

type ReviewerFlowsCtx = {
  rows: WorktreeRow[];
  modal: Modal | null;
  setModal: (m: Modal | null) => void;
  toast: (message: string, color?: string, ms?: number) => void;
  fetchContributors: () => Promise<readonly Contributor[]>;
  fetchMe: () => Promise<string | null>;
  mutate: <TData>(opts: {
    filter: QueryFilters;
    patch: (prev: TData | undefined) => TData | undefined;
    run: () => Promise<void>;
  }) => Promise<void>;
};

export function makeReviewerFlows(ctx: ReviewerFlowsCtx) {
  const { rows, modal, setModal, toast, fetchContributors, fetchMe, mutate } = ctx;

  async function openReviewerPicker(slug: string): Promise<void> {
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row?.pr) {
      toast("no PR for this row", theme.warn, 2000);
      return;
    }
    if (row.pr.state !== "OPEN") {
      toast("PR is not open", theme.warn, 2000);
      return;
    }
    if (row.pr.isDraft) {
      toast("PR is a draft (mark ready first)", theme.warn, 2000);
      return;
    }
    // `fetchContributors` returns cached data without awaiting when
    // warm (background refresh when stale). Only the first-ever open
    // pays a fetch; after that the picker opens instantly even when
    // the cached list is stale. `fetchMe` is process-cached after
    // first call.
    const [contributors, me] = await Promise.all([
      fetchContributors(),
      fetchMe(),
    ]);
    const requested = new Set(row.pr.requestedReviewers);
    // Three-tier candidate list:
    //   1. PR-scoped suggestions (highest signal — file ownership +
    //      history). Often empty on small diffs.
    //   2. Already-requested logins/teams not in (1), so the picker
    //      doubles as a way to *remove* them.
    //   3. Repo-wide contributors as the fallback so the picker is
    //      never empty just because (1) was. Cached for 24h.
    const items: MultiPickerItem[] = [];
    const seen = new Set<string>();
    const skipSelf = (login: string) => me !== null && login === me;
    for (const s of row.pr.suggestedReviewers) {
      if (skipSelf(s.login)) continue;
      const already = requested.has(s.login);
      const tags: string[] = [];
      if (already) tags.push("requested");
      tags.push("suggested");
      if (s.isAuthor) tags.push("author");
      if (s.isCommenter) tags.push("commenter");
      items.push({
        key: s.login,
        label: s.login,
        hint: `(${tags.join(", ")})`,
      });
      seen.add(s.login);
    }
    for (const login of row.pr.requestedReviewers) {
      if (seen.has(login)) continue;
      if (skipSelf(login)) continue;
      items.push({ key: login, label: login, hint: "(requested)" });
      seen.add(login);
    }
    for (const c of contributors) {
      if (seen.has(c.login)) continue;
      if (skipSelf(c.login)) continue;
      items.push({
        key: c.login,
        label: c.login,
        hint: `(${c.contributions} commits)`,
      });
      seen.add(c.login);
    }
    if (items.length === 0) {
      toast("no reviewer candidates", theme.warn, 2000);
      return;
    }
    setModal({
      kind: "reviewerPicker",
      title: `edit reviewers for #${row.pr.number}`,
      items,
      index: 0,
      checked: new Set(requested),
      original: new Set(requested),
      slug,
      prNumber: row.pr.number,
    });
  }

  async function submitReviewerPicker(): Promise<void> {
    if (modal?.kind !== "reviewerPicker") return;
    const { slug, prNumber, checked, original } = modal;
    const log = createLogger(slug);
    const branch = rows.find((r) => r.wt.slug === slug)?.wt.branch;
    setModal(null);
    if (!branch) {
      // Slug disappeared between picker open and submit (race against
      // a destroy). The mutation would still succeed at the gh layer,
      // but the optimistic patch has nothing to target — bail rather
      // than silently dropping the cache update.
      log.event.warn(`slug ${slug} no longer present; aborting reviewer edit`);
      toast("worktree gone, edit aborted", theme.warn, 2500);
      return;
    }
    const add: string[] = [];
    const remove: string[] = [];
    for (const k of checked) if (!original.has(k)) add.push(k);
    for (const k of original) if (!checked.has(k)) remove.push(k);
    if (add.length === 0 && remove.length === 0) {
      toast("no changes", theme.fgDim, 1500);
      return;
    }
    try {
      await mutate<GithubData>({
        filter: { queryKey: ["github"] },
        patch: (data) =>
          patchPullRequest(data, branch, (pr) => ({
            ...pr,
            requestedReviewers: [...checked],
            reviewRequests: pr.reviewRequests + add.length - remove.length,
          })),
        run: async () => {
          const result = await editReviewers(prNumber, { add, remove });
          if (!result.ok) throw new Error(result.error);
        },
      });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      log.event.err(`edit reviewers failed for #${prNumber}: ${msg}`);
      toast(`edit reviewers failed: ${msg}`, theme.err, 4000);
      return;
    }
    const parts: string[] = [];
    if (add.length > 0) parts.push(`+${add.join(", ")}`);
    if (remove.length > 0) parts.push(`-${remove.join(", ")}`);
    log.event.ok(`edited reviewers for #${prNumber}: ${parts.join("; ")}`);
    const summary = [
      add.length > 0 ? `added ${add.length}` : null,
      remove.length > 0 ? `removed ${remove.length}` : null,
    ]
      .filter(Boolean)
      .join(", ");
    toast(summary, theme.ok, 2500);
  }

  return { openReviewerPicker, submitReviewerPicker };
}
