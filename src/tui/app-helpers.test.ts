import { describe, expect, test } from "bun:test";

import { StatusKind } from "../core/types.ts";

import {
  buildActionVars,
  cursorSuccessor,
  destroyHazard,
  destroyHazardLabel,
  destroyHazards,
  isCleanCandidate,
  sectionJumpTarget,
} from "./app-helpers.ts";
import type { FieldState, WorktreeRow } from "./hooks/useWorktreeRows.ts";
import type { VisualItem } from "./hooks/useVisualItems.ts";

function field<T>(data: T | undefined): FieldState<T> {
  return { data, isStale: false, isFetching: false, isLoading: false, error: null };
}

type SyncData = NonNullable<WorktreeRow["fields"]["sync"]["data"]>;

/**
 * `"loading"` is the explicit spelling of "the query has no data yet" —
 * a bare `undefined` would be swallowed by the default parameter and
 * silently test the clean case instead.
 */
function makeRow(overrides: {
  dirty?: readonly string[] | "loading";
  /** Commits ahead of `origin/<branch>`. Ignored when `pushed` is false. */
  ahead?: number | "loading";
  /** Whether `origin/<branch>` exists at all (`sync.remote` non-null). */
  pushed?: boolean;
  /** Commits ahead of the base — the fallback when nothing is pushed. */
  aheadOfBase?: number;
  status?: WorktreeRow["status"];
  pr?: WorktreeRow["pr"];
  archived?: boolean;
  work?: WorktreeRow["work"];
}): WorktreeRow {
  const {
    dirty = [],
    ahead = 0,
    pushed = true,
    aheadOfBase = 0,
    status,
    pr,
    archived = false,
    work,
  } = overrides;
  return {
    wt: {
      slug: "s",
      path: "/tmp/s",
      branch: "michael/s",
      isMain: false,
      stage: "s",
    },
    fields: {
      dirty: field<readonly string[]>(dirty === "loading" ? undefined : dirty),
      lock: field(null),
      deploy: field(false),
      merged: field(false),
      gone: field(false),
      sync: field(
        ahead === "loading"
          ? undefined
          : ({
              main: { ahead: aheadOfBase, behind: 0 },
              remote: pushed ? { ahead, behind: 0 } : null,
            } as unknown as SyncData),
      ),
      claude: field(undefined),
      gitActivity: field(undefined),
      conflict: field(undefined),
    },
    status: status ?? { kind: StatusKind.Clean, label: "clean" },
    stackedOn: null,
    stack: null,
    archived,
    title: "s",
    titleSource: "slug",
    section: null,
    sectionIsStack: false,
    work: work ?? null,
    ...(pr ? { pr } : {}),
  } as unknown as WorktreeRow;
}

describe("destroyHazard", () => {
  test("a clean, fully-pushed worktree has no hazard", () => {
    expect(destroyHazard(makeRow({}))).toBeNull();
  });

  test("uncommitted changes are a hazard, counted", () => {
    const hazard = destroyHazard(makeRow({ dirty: ["a.ts", "b.ts", "c.ts"] }));
    expect(hazard).toEqual({ kind: "dirty", count: 3 });
    expect(destroyHazardLabel(hazard!)).toBe("3 uncommitted changes");
  });

  test("unpushed commits are a hazard", () => {
    const hazard = destroyHazard(makeRow({ ahead: 1 }));
    expect(hazard).toEqual({ kind: "unpushed", count: 1 });
    expect(destroyHazardLabel(hazard!)).toBe("1 unpushed commit");
  });

  test("still-loading state is a hazard, not an absence of one", () => {
    // Both fields read undefined while their queries load or after an
    // error. Collapsing that into "clean" is what let a merged worktree
    // with 16 uncommitted files get swept.
    expect(destroyHazard(makeRow({ dirty: "loading" }))).toEqual({ kind: "unknown" });
    expect(destroyHazard(makeRow({ ahead: "loading" }))).toEqual({ kind: "unknown" });
  });

  test("a merged branch with uncommitted work is a clean CANDIDATE but still hazardous", () => {
    // The regression this pins: `isCleanCandidate` is a claim about the
    // branch, `destroyHazard` a claim about the working tree. The clean
    // sweep once consulted only the first and destroyed rift checkouts —
    // independent clones, so branch and reflog went with the directory.
    const row = makeRow({
      dirty: ["src/x.ts"],
      status: { kind: StatusKind.Merged, label: "merged" },
    });
    expect(isCleanCandidate(row)).toBe(true);
    expect(destroyHazard(row)).toEqual({ kind: "dirty", count: 1 });
  });

  test("dirty outranks unpushed when both are present", () => {
    expect(destroyHazard(makeRow({ dirty: ["a.ts"], ahead: 4 }))).toEqual({
      kind: "dirty",
      count: 1,
    });
  });

  test("destroyHazards lists both so the confirm can't understate the loss", () => {
    expect(destroyHazards(makeRow({ dirty: ["a.ts"], ahead: 4 }))).toEqual([
      { kind: "dirty", count: 1 },
      { kind: "unpushed", count: 4 },
    ]);
  });

  test("a pushed branch ahead of its base only is NOT unpushed", () => {
    // The regression this pins: ahead-of-base is not local-only work. Feeding
    // it to the destroy guard made `d`/`c` refuse a fully pushed PR branch.
    expect(destroyHazard(makeRow({ ahead: 0, aheadOfBase: 3 }))).toBeNull();
  });

  test("nothing on origin: commits since the base are the hazard", () => {
    expect(destroyHazard(makeRow({ pushed: false, aheadOfBase: 2 }))).toEqual({
      kind: "unpushed",
      count: 2,
    });
  });

  test("squash-merged and pruned: the same commits are landed, not lost", () => {
    // Merged + no `origin/<branch>` is the shape a squash merge leaves
    // behind. Its local commits aren't on trunk by sha, so the fallback
    // count is non-zero — but the work is upstream, and treating it as a
    // hazard would make the `c` sweep keep every row it exists to clear.
    const row = makeRow({
      pushed: false,
      aheadOfBase: 2,
      status: { kind: StatusKind.Merged, label: "merged" },
    });
    expect(isCleanCandidate(row)).toBe(true);
    expect(destroyHazard(row)).toBeNull();
  });
});

/**
 * Minimal visual items for the cursor rule: only `kind`, the identity
 * `visualKey` reads, the section, and the archived flag matter.
 */
function wtItem(
  slug: string,
  section: string | null,
  archived = false,
): VisualItem {
  return {
    kind: "wt",
    row: { wt: { slug }, section, archived },
    target: null,
  } as unknown as VisualItem;
}

function sectionItem(sectionKey: string): VisualItem {
  return { kind: "section", sectionKey, label: sectionKey, members: [] } as unknown as VisualItem;
}

describe("cursorSuccessor", () => {
  // A realistic unfolded board: sections are contiguous runs of rows,
  // NOT header items — `visualItems` only carries a `section` item for a
  // FOLDED section (the "── Landed ──" divider is drawn by the list
  // panel, not an item).
  //   Inbox: a, b · Landed: c, d, e
  const board: VisualItem[] = [
    wtItem("a", null),
    wtItem("b", null),
    wtItem("c", "Landed"),
    wtItem("d", "Landed"),
    wtItem("e", "Landed"),
  ];

  test("takes the row directly below, in the same section", () => {
    expect(cursorSuccessor(board, 2, new Set(["c"]))).toBe("d");
  });

  test("falls back UP when the departing row is last in its section", () => {
    expect(cursorSuccessor(board, 4, new Set(["e"]))).toBe("d");
  });

  test("never lands on another row the same sweep is destroying", () => {
    // c and d both going: the survivor below them is e, not d.
    expect(cursorSuccessor(board, 2, new Set(["c", "d"]))).toBe("e");
  });

  test("stays put rather than following the row into another section", () => {
    // `b` is the last inbox row; the rows below it belong to Landed, so
    // the constrained scan stops at the boundary and comes back up.
    expect(cursorSuccessor(board, 1, new Set(["b"]))).toBe("a");
  });

  test("leaves the section only when the whole section is going", () => {
    expect(cursorSuccessor(board, 2, new Set(["c", "d", "e"]))).toBe("b");
  });

  test("a folded section below is a valid landing spot", () => {
    const folded: VisualItem[] = [
      wtItem("c", "Landed"),
      wtItem("d", "Landed"),
      sectionItem("Statuses"),
    ];
    expect(cursorSuccessor(folded, 0, new Set(["c", "d"]))).toBe(
      "section:Statuses",
    );
  });

  test("skips rows already archived by an earlier destroy", () => {
    const withArchived: VisualItem[] = [
      wtItem("c", "Landed"),
      wtItem("d", "Landed", true),
      wtItem("e", "Landed"),
    ];
    expect(cursorSuccessor(withArchived, 0, new Set(["c"]))).toBe("e");
  });

  test("no move when the cursor isn't on a departing row", () => {
    // An automation cleaning `c` while the user reads `a`.
    expect(cursorSuccessor(board, 0, new Set(["c"]))).toBeNull();
  });

  test("null when nothing survives", () => {
    expect(cursorSuccessor([wtItem("a", null)], 0, new Set(["a"]))).toBeNull();
  });
});

describe("sectionJumpTarget", () => {
  const board: VisualItem[] = [
    wtItem("inbox-a", null),
    wtItem("inbox-b", null),
    wtItem("doing-a", "Doing"),
    wtItem("doing-b", "Doing"),
    sectionItem("Paused"),
    wtItem("archived", null, true),
  ];

  test("down lands on the first row of the next expanded section", () => {
    expect(sectionJumpTarget(board, 0, 1)).toBe("doing-a");
  });

  test("down lands on a folded section title", () => {
    expect(sectionJumpTarget(board, 2, 1)).toBe("section:Paused");
  });

  test("up lands on the first row of the previous section", () => {
    expect(sectionJumpTarget(board, 4, -1)).toBe("doing-a");
    expect(sectionJumpTarget(board, 3, -1)).toBe("inbox-a");
  });

  test("stops at the board edges", () => {
    expect(sectionJumpTarget(board, 0, -1)).toBeNull();
    expect(sectionJumpTarget(board, board.length - 1, 1)).toBeNull();
  });
});

describe("an unverified landing is a destroy hazard", () => {
  const merged = { kind: StatusKind.Merged, label: "merged" } as WorktreeRow["status"];
  const owed = {
    state: "ready",
    at: new Date().toISOString(),
    verifyAfterMerge: "connect gcal on staging",
  } as WorktreeRow["work"];

  // The row a sweep would take, on the day the check became runnable.
  // Removing it deletes the obligation along with the checkout, and
  // nothing afterwards records that the check never happened.
  test("a merged row owing a check is kept back", () => {
    const row = makeRow({ status: merged, work: owed });
    expect(isCleanCandidate(row)).toBe(true);
    expect(destroyHazard(row)).toEqual({
      kind: "unverified",
      steps: "connect gcal on staging",
    });
    // The steps stay on the hazard and out of the label: every reader
    // of the label is a scan line, and the field has no length budget.
    expect(destroyHazardLabel(destroyHazard(row)!)).toBe(
      "post-merge verification still owed",
    );
  });

  // It must never hold back a row before the merge — that would make
  // it a second `--blocked-on`, which is the failure it exists to avoid.
  test("the same record on an unmerged row is not a hazard", () => {
    expect(destroyHazard(makeRow({ work: owed }))).toBeNull();
  });

  test("verified discharges it", () => {
    const row = makeRow({
      status: merged,
      work: { ...owed!, state: "verified" } as WorktreeRow["work"],
    });
    expect(destroyHazard(row)).toBeNull();
  });

  // Real data loss outranks an obligation when a confirm has one line.
  test("it sorts below actual loss", () => {
    const row = makeRow({ status: merged, work: owed, dirty: ["a.ts"] });
    expect(destroyHazards(row).map((h) => h.kind)).toEqual(["dirty", "unverified"]);
  });
});

// `{{issue_id}}` exists so a shell action never has to re-implement
// `branch.id_pattern` in sed — two definitions of one convention drift
// the first time the pattern changes.
describe("buildActionVars issue_id", () => {
  const varsFor = (slug: string) =>
    buildActionVars(
      { ...makeRow({}), wt: { ...makeRow({}).wt, slug } } as WorktreeRow,
      "/",
    );

  test("pulls the tracker id out of the slug, upper-cased", () => {
    expect(varsFor("coz-2176-active-louse").issue_id).toBe("COZ-2176");
  });

  // Empty, never the slug: an action templating a tracker call wants a
  // substitution that produces an obviously-wrong request over one that
  // produces a plausible request against the wrong issue.
  test("a slug with no id yields an empty string", () => {
    expect(varsFor("meetings-frontend-cleanup").issue_id).toBe("");
  });

  // Not anchored to the start, matching every other surface that reads
  // an id out of a slug (the row's issue link, `wt ls --json`). A slug
  // someone prefixed by hand still names its issue.
  test("the id need not lead the slug", () => {
    expect(varsFor("fix-coz-2176-later").issue_id).toBe("COZ-2176");
  });
});
