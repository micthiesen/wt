import { describe, expect, test } from "bun:test";

import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import {
  canonicalRepositoryConfig,
  mergeConfig,
  pathNamespace,
  repositoryConfigPath,
  repositoryNamespace,
  REPOSITORY_CONFIG_ENV,
} from "./config-layer.ts";

describe("repositoryNamespace", () => {
  test("slugs the repository path below home", () => {
    expect(
      repositoryNamespace(
        "/Users/alex/dev/cz/cozee-dev/.wt.toml",
        "/Users/alex",
      ),
    ).toBe("dev-cz-cozee-dev");
  });

  test("uses the full path outside home and normalizes punctuation", () => {
    expect(repositoryNamespace("/srv/Team App/.wt.toml", "/Users/alex"))
      .toBe("srv-team-app");
  });

  test("can derive the same namespace directly from a repository path", () => {
    expect(pathNamespace("/Users/alex/dev/cz/cozee-dev", "/Users/alex"))
      .toBe("dev-cz-cozee-dev");
  });
});

describe("repositoryConfigPath", () => {
  test("finds the nearest .wt.toml above the invocation directory", () => {
    const root = mkdtempSync(join(tmpdir(), "wt-config-layer-"));
    try {
      const nested = join(root, "src", "feature");
      mkdirSync(nested, { recursive: true });
      const path = join(root, ".wt.toml");
      writeFileSync(path, "[branch]\nbase = \"trunk\"\n");

      expect(repositoryConfigPath(nested, {})).toBe(path);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("prefers the inherited repository config path", () => {
    const path = join(tmpdir(), "selected-wt.toml");
    expect(repositoryConfigPath("/", { [REPOSITORY_CONFIG_ENV]: path })).toBe(path);
  });
});

describe("mergeConfig", () => {
  test("task readers inherit when omitted and clear only with an explicit empty array", () => {
    const base = { issue_tracker: { read_command: ["tracker", "read", "{id}"] } };
    expect(mergeConfig(base, { issue_tracker: { prefix: "eng" } })).toEqual({
      issue_tracker: { prefix: "eng", read_command: ["tracker", "read", "{id}"] },
    });
    expect(mergeConfig(base, { issue_tracker: { read_command: [] } })).toEqual({
      issue_tracker: { read_command: [] },
    });
  });

  test("merges tables while replacing scalar values and arrays", () => {
    expect(mergeConfig(
      {
        paths: { main_clone: "/one", worktree_root: "/one-wt" },
        branch: { prefix: "alex", base: "main" },
        actions: [{ id: "global" }],
      },
      {
        paths: { main_clone: "/two" },
        branch: { base: "develop" },
        actions: [{ id: "local" }],
      },
    )).toEqual({
      paths: { main_clone: "/two", worktree_root: "/one-wt" },
      branch: { prefix: "alex", base: "develop" },
      actions: [{ id: "local" }],
    });
  });
});

describe("config loader integration", () => {
  test("builds config from user defaults plus repository overrides", () => {
    const root = mkdtempSync(join(tmpdir(), "wt-config-load-"));
    try {
      const repo = join(root, "repo");
      const nested = join(repo, "src");
      mkdirSync(nested, { recursive: true });
      const userConfig = join(root, "user.toml");
      writeFileSync(userConfig, `
[paths]
main_clone = "/global/repo"
worktree_root = "/global/worktrees"

[instance]
role = "worker"

[harness]
primary = "codex"
hidden = ["opencode"]

[naming]
harness = "primary"
reasoning_effort = "low"

[naming.models]
codex = "gpt-cheap"

[branch]
prefix = "alex"
base = "main"

[ui]
rows = ["branch", "git"]

[github]
ignored_review_repositories = ["Example/noisy-repo"]
`);
      writeFileSync(join(repo, ".wt.toml"), `
[paths]
main_clone = "/local/repo"

[branch]
base = "develop"

[lifecycle]
copy_globs = [".agents/**"]

[browser]
chrome_profile = "Profile 3"

[github]
reviewers = false
`);

      const configModule = pathToFileURL(join(import.meta.dir, "config.ts")).href;
      const badgesModule = pathToFileURL(join(import.meta.dir, "../tui/badges.ts")).href;
      const githubQueriesModule = pathToFileURL(
        join(import.meta.dir, "../state/queries/github.ts"),
      ).href;
      const script = `
        const { config } = await import(${JSON.stringify(configModule)});
        const { reviewBadge } = await import(${JSON.stringify(badgesModule)});
        const { githubQuery, reviewRequestsQuery } = await import(${JSON.stringify(githubQueriesModule)});
        console.log(JSON.stringify({
          repoId: config.repoId,
          instance: config.instance,
          harness: {
            primary: config.harness.primary,
            hidden: [...config.harness.hidden],
          },
          naming: config.naming,
          repoPath: config.repoPath,
          paths: config.paths,
          tmux: config.tmux,
          branch: config.branch,
          lifecycle: config.lifecycle,
          browser: config.browser,
          github: config.github,
          rows: config.ui.rows,
          reviewerBadgeHidden: reviewBadge("unrequested") === null,
          githubEnabled: githubQuery(["feature"]).enabled,
          reviewRequestsEnabled: reviewRequestsQuery().enabled,
        }));
      `;
      const env: Record<string, string | undefined> = {
        ...process.env,
        WT_CONFIG: userConfig,
      };
      delete env[REPOSITORY_CONFIG_ENV];
      const result = Bun.spawnSync([process.execPath, "-e", script], {
        cwd: nested,
        env,
        stdout: "pipe",
        stderr: "pipe",
      });

      expect(result.exitCode).toBe(0);
      const repoId = repositoryNamespace(join(repo, ".wt.toml"));
      expect(JSON.parse(result.stdout.toString())).toMatchObject({
        repoId,
        instance: { role: "worker" },
        harness: { primary: "codex", hidden: ["opencode"] },
        naming: {
          harness: "primary",
          models: { codex: "gpt-cheap" },
          reasoningEffort: "low",
          maxInputTokens: 8000,
          timeoutMs: 120000,
        },
        repoPath: realpathSync(repo),
        paths: {
          mainClone: "/local/repo",
          worktreeRoot: "/global/worktrees",
          cacheDb: join(homedir(), ".cache", "wt", repoId, "cache.sqlite"),
          stateDb: join(homedir(), ".local", "state", "wt", "wt.sqlite"),
        },
        tmux: { socket: `wt-${repoId}` },
        branch: { prefix: "alex", base: "develop" },
        lifecycle: { copyGlobs: [".agents/**"] },
        browser: { chromeProfile: "Profile 3" },
        github: {
          reviewers: false,
          ignoredReviewRepositories: ["Example/noisy-repo"],
        },
        rows: ["branch", "git"],
        reviewerBadgeHidden: true,
        githubEnabled: true,
        reviewRequestsEnabled: false,
      });
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("rejects copy globs that can escape the main clone", () => {
    const root = mkdtempSync(join(tmpdir(), "wt-config-copy-globs-"));
    try {
      const userConfig = join(root, "config.toml");
      writeFileSync(userConfig, `
[paths]
main_clone = "/repo"
worktree_root = "/worktrees"

[branch]
prefix = "alex"

[lifecycle]
copy_globs = ["/tmp/**", "../secrets/**"]
`);

      const configModule = pathToFileURL(join(import.meta.dir, "config.ts")).href;
      const env: Record<string, string | undefined> = {
        ...process.env,
        WT_CONFIG: userConfig,
      };
      delete env[REPOSITORY_CONFIG_ENV];
      const result = Bun.spawnSync([process.execPath, "-e", `await import(${JSON.stringify(configModule)})`], {
        cwd: root,
        env,
        stdout: "pipe",
        stderr: "pipe",
      });

      expect(result.exitCode).toBe(1);
      expect(result.stderr.toString()).toContain(
        "lifecycle.copy_globs entries must be relative paths without '..' segments",
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("production branch must be one that wt keeps fresh", () => {
    const root = mkdtempSync(join(tmpdir(), "wt-config-production-"));
    try {
      const userConfig = join(root, "config.toml");
      writeFileSync(userConfig, `[paths]\nmain_clone = "/repo"\nworktree_root = "/worktrees"\n[branch]\nprefix = "alex"\nbase = "staging"\nproduction = "main"\n`);
      const env: Record<string, string | undefined> = { ...process.env, WT_CONFIG: userConfig };
      delete env[REPOSITORY_CONFIG_ENV];
      const moduleUrl = pathToFileURL(join(import.meta.dir, "config.ts")).href;
      const result = Bun.spawnSync([process.execPath, "-e", `await import(${JSON.stringify(moduleUrl)})`], {
        cwd: root, env, stdout: "pipe", stderr: "pipe",
      });
      expect(result.exitCode).toBe(1);
      expect(result.stderr.toString()).toContain('branch.production "main" must be [branch] base or in [branch] keep_fresh');
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("rejects invalid hidden harness configuration", () => {
    const root = mkdtempSync(join(tmpdir(), "wt-config-hidden-harnesses-"));
    try {
      const userConfig = join(root, "config.toml");
      writeFileSync(userConfig, `
[paths]
main_clone = "/repo"
worktree_root = "/worktrees"

[branch]
prefix = "alex"

[harness]
hidden = ["opencode", 42, "unknown"]
`);

      const configModule = pathToFileURL(join(import.meta.dir, "config.ts")).href;
      const env: Record<string, string | undefined> = {
        ...process.env,
        WT_CONFIG: userConfig,
      };
      delete env[REPOSITORY_CONFIG_ENV];
      const result = Bun.spawnSync([process.execPath, "-e", `await import(${JSON.stringify(configModule)})`], {
        cwd: root,
        env,
        stdout: "pipe",
        stderr: "pipe",
      });

      expect(result.exitCode).toBe(1);
      expect(result.stderr.toString()).toContain("harness.hidden entries must be strings");
      expect(result.stderr.toString()).toContain('harness.hidden: unknown harness "unknown"');
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("points legacy AI config at harness-backed naming", () => {
    const root = mkdtempSync(join(tmpdir(), "wt-config-ai-migration-"));
    try {
      const userConfig = join(root, "config.toml");
      writeFileSync(userConfig, `
[paths]
main_clone = "/repo"
worktree_root = "/worktrees"

[branch]
prefix = "alex"

[ai]
provider = "gemini"
model = "gemini-flash"
`);
      const configModule = pathToFileURL(join(import.meta.dir, "config.ts")).href;
      const result = Bun.spawnSync(
        [process.execPath, "-e", `await import(${JSON.stringify(configModule)})`],
        {
          cwd: root,
          env: { ...process.env, WT_CONFIG: userConfig },
          stdout: "pipe",
          stderr: "pipe",
        },
      );
      expect(result.exitCode).toBe(1);
      expect(result.stderr.toString()).toContain(
        "[ai] is no longer supported; use [naming]",
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});

describe("ui.activity_pane", () => {
  // The pane's position is the one `[ui]` key whose DEFAULT is a product
  // decision someone else made, so the default is pinned here rather than
  // left to whatever the resolver happens to do: a silent flip would move
  // every user's layout, and the setting exists precisely so nobody has to
  // trade one group's layout for another's.
  const load = (uiSection: string) => {
    const root = mkdtempSync(join(tmpdir(), "wt-activity-pane-"));
    try {
      const userConfig = join(root, "user.toml");
      writeFileSync(userConfig, `
[paths]
main_clone = "/global/repo"
worktree_root = "/global/worktrees"

[branch]
prefix = "alex"
base = "main"
${uiSection}
`);
      const configModule = pathToFileURL(join(import.meta.dir, "config.ts")).href;
      const script = `
        const { config } = await import(${JSON.stringify(configModule)});
        console.log(JSON.stringify({ activityPane: config.ui.activityPane, hideTerminalApps: config.ui.hideTerminalApps, actionGroupsLast: config.ui.actionGroupsLast }));
      `;
      const env: Record<string, string | undefined> = {
        ...process.env,
        WT_CONFIG: userConfig,
      };
      delete env[REPOSITORY_CONFIG_ENV];
      return Bun.spawnSync([process.execPath, "-e", script], {
        cwd: root,
        env,
        stdout: "pipe",
        stderr: "pipe",
      });
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  };

  test("defaults to the column layout", () => {
    const result = load("");
    expect(result.exitCode).toBe(0);
    expect(JSON.parse(result.stdout.toString()).activityPane).toBe("column");
    expect(JSON.parse(result.stdout.toString()).hideTerminalApps).toEqual([]);
    expect(JSON.parse(result.stdout.toString()).actionGroupsLast).toEqual([]);
  });

  test("terminal hiding opts in arbitrary process names", () => {
    const result = load('\n[ui]\nhide_terminal_apps = ["Ghostty", "custom terminal"]\n');
    expect(result.exitCode).toBe(0);
    expect(JSON.parse(result.stdout.toString()).hideTerminalApps).toEqual(["Ghostty", "custom terminal"]);
  });

  test("action groups can be placed last", () => {
    const result = load('\n[ui]\naction_groups_last = ["dev server"]\n');
    expect(result.exitCode).toBe(0);
    expect(JSON.parse(result.stdout.toString()).actionGroupsLast).toEqual(["dev server"]);
  });

  test("accepts full_width", () => {
    const result = load("\n[ui]\nactivity_pane = \"full_width\"\n");
    expect(result.exitCode).toBe(0);
    expect(JSON.parse(result.stdout.toString()).activityPane).toBe("full_width");
  });

  test("rejects an unknown value by name", () => {
    const result = load("\n[ui]\nactivity_pane = \"bottom\"\n");
    expect(result.exitCode).toBe(1);
    expect(result.stderr.toString()).toContain("ui.activity_pane");
  });
});

describe("canonicalRepositoryConfig", () => {
  function scaffold(): {
    root: string;
    mainClone: string;
    worktreeRoot: string;
    worktree: string;
  } {
    const root = realpathSync(mkdtempSync(join(tmpdir(), "wt-canonical-")));
    const mainClone = join(root, "code", "myrepo");
    const worktreeRoot = join(root, "code", "myrepo-wt");
    const worktree = join(worktreeRoot, "some-branch");
    mkdirSync(mainClone, { recursive: true });
    mkdirSync(worktree, { recursive: true });
    return { root, mainClone, worktreeRoot, worktree };
  }

  test("a worktree's copy resolves to the repository's own config", () => {
    const { root, mainClone, worktreeRoot, worktree } = scaffold();
    try {
      writeFileSync(join(mainClone, ".wt.toml"), "");
      writeFileSync(join(worktree, ".wt.toml"), "");
      expect(
        canonicalRepositoryConfig(join(worktree, ".wt.toml"), mainClone, worktreeRoot),
      ).toBe(join(mainClone, ".wt.toml"));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("finding nothing still resolves to the repository's own config", () => {
    const { root, mainClone, worktreeRoot } = scaffold();
    try {
      writeFileSync(join(mainClone, ".wt.toml"), "");
      expect(canonicalRepositoryConfig(null, mainClone, worktreeRoot))
        .toBe(join(mainClone, ".wt.toml"));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("a config outside worktree_root names itself", () => {
    const { root, mainClone, worktreeRoot } = scaffold();
    try {
      const other = join(root, "code", "otherrepo");
      mkdirSync(other, { recursive: true });
      writeFileSync(join(mainClone, ".wt.toml"), "");
      writeFileSync(join(other, ".wt.toml"), "");
      expect(canonicalRepositoryConfig(join(other, ".wt.toml"), mainClone, worktreeRoot))
        .toBe(join(other, ".wt.toml"));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("a repository with no config of its own keeps what was found", () => {
    const { root, mainClone, worktreeRoot, worktree } = scaffold();
    try {
      writeFileSync(join(worktree, ".wt.toml"), "");
      expect(
        canonicalRepositoryConfig(join(worktree, ".wt.toml"), mainClone, worktreeRoot),
      ).toBe(join(worktree, ".wt.toml"));
      expect(canonicalRepositoryConfig(null, mainClone, worktreeRoot)).toBe(null);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("an unknown main clone leaves discovery alone", () => {
    // `[paths]` is missing; resolving "" would silently mean the CWD.
    expect(canonicalRepositoryConfig("/a/b/.wt.toml", "", "")).toBe("/a/b/.wt.toml");
    expect(canonicalRepositoryConfig(null, "", "")).toBe(null);
  });
});

describe("repository identity across worktrees", () => {
  // Every worktree carries a COPY of the repository's `.wt.toml`, and a shell
  // outside the repo finds none at all. Deriving identity from whichever file
  // discovery happened to land on gave each of those callers its own namespace
  // — and the namespace picks the durable state database, the cache root and
  // the tmux socket. The board then fragmented in a way that looked consistent
  // from every single vantage point: a status asserted in one worktree was
  // invisible from every other, `wt manager send` cold-started a manager on a
  // tmux server the TUI never reads, and a tracker id set in the TUI was
  // invisible to the session it named.
  type Identity = {
    repoId: string;
    repoPath: string;
    stateDb: string;
    cacheRoot: string;
    tmuxSocket: string;
  };

  function identityFrom(cwd: string, userConfig: string): Identity {
    const configModule = pathToFileURL(join(import.meta.dir, "config.ts")).href;
    const script = `
      const { config } = await import(${JSON.stringify(configModule)});
      console.log(JSON.stringify({
        repoId: config.repoId,
        repoPath: config.repoPath,
        stateDb: config.paths.stateDb,
        cacheRoot: config.paths.cacheRoot,
        tmuxSocket: config.tmux.socket,
      }));
    `;
    const env: Record<string, string | undefined> = { ...process.env, WT_CONFIG: userConfig };
    delete env[REPOSITORY_CONFIG_ENV];
    delete env.WT_TMUX_SOCKET;
    const result = Bun.spawnSync([process.execPath, "-e", script], {
      cwd,
      env,
      stdout: "pipe",
      stderr: "pipe",
    });
    expect(result.stderr.toString()).toBe("");
    expect(result.exitCode).toBe(0);
    return JSON.parse(result.stdout.toString()) as Identity;
  }

  function scaffold(prefix: string, withRepositoryConfig: boolean): {
    root: string;
    mainClone: string;
    worktree: string;
    outside: string;
    userConfig: string;
  } {
    const root = realpathSync(mkdtempSync(join(tmpdir(), prefix)));
    const mainClone = join(root, "code", "myrepo");
    const worktreeRoot = join(root, "code", "myrepo-wt");
    const worktree = join(worktreeRoot, "some-branch");
    const outside = join(root, "elsewhere");
    mkdirSync(mainClone, { recursive: true });
    mkdirSync(worktree, { recursive: true });
    mkdirSync(outside, { recursive: true });

    const userConfig = join(root, "user.toml");
    writeFileSync(
      userConfig,
      `
[paths]
main_clone = ${JSON.stringify(mainClone)}
worktree_root = ${JSON.stringify(worktreeRoot)}

[branch]
prefix = "alex"
`,
    );
    if (withRepositoryConfig) {
      // The same bytes the backends copy into every worktree.
      const repoConfig = "\n[branch]\nbase = \"staging\"\n";
      writeFileSync(join(mainClone, ".wt.toml"), repoConfig);
      writeFileSync(join(worktree, ".wt.toml"), repoConfig);
    }
    return { root, mainClone, worktree, outside, userConfig };
  }

  test("a worktree, its repository and an unrelated cwd share one namespace", () => {
    const { root, mainClone, worktree, outside, userConfig } = scaffold(
      "wt-config-identity-",
      true,
    );
    try {
      const fromMainClone = identityFrom(mainClone, userConfig);
      const fromWorktree = identityFrom(worktree, userConfig);
      const fromOutside = identityFrom(outside, userConfig);

      // Not just the id: the id is only interesting because these three follow
      // it, and each one on its own is enough to split the board.
      expect(fromWorktree).toEqual(fromMainClone);
      expect(fromOutside).toEqual(fromMainClone);
      expect(fromMainClone.repoPath).toBe(mainClone);
      expect(fromMainClone.tmuxSocket).toBe(`wt-${fromMainClone.repoId}`);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("a repository with no .wt.toml keeps the shared compatibility layout", () => {
    const { root, mainClone, worktree, outside, userConfig } = scaffold(
      "wt-config-identity-bare-",
      false,
    );
    try {
      const fromMainClone = identityFrom(mainClone, userConfig);
      expect(identityFrom(worktree, userConfig)).toEqual(fromMainClone);
      expect(identityFrom(outside, userConfig)).toEqual(fromMainClone);
      expect(fromMainClone.tmuxSocket).toBe("wt");
      expect(fromMainClone.stateDb).toBe(join(homedir(), ".cache", "wt", "wt.sqlite"));
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("a worktree config survives the repository losing its own", () => {
    // The repository has no `.wt.toml` to re-point at — the file was deleted
    // after the worktrees were cloned — so the worktree's copy is still what
    // supplies the content. Identity must come from `paths.main_clone`
    // anyway, or that worktree becomes a repository of its own again.
    const { root, mainClone, worktree, outside, userConfig } = scaffold(
      "wt-config-identity-orphan-",
      false,
    );
    try {
      writeFileSync(join(worktree, ".wt.toml"), "\n[branch]\nbase = \"staging\"\n");
      const fromMainClone = identityFrom(mainClone, userConfig);
      expect(identityFrom(worktree, userConfig)).toEqual(fromMainClone);
      expect(identityFrom(outside, userConfig)).toEqual(fromMainClone);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("a second repository outside worktree_root stays isolated", () => {
    const { root, mainClone, userConfig } = scaffold("wt-config-identity-other-", true);
    try {
      const other = join(root, "code", "otherrepo");
      mkdirSync(other, { recursive: true });
      writeFileSync(
        join(other, ".wt.toml"),
        `
[paths]
main_clone = ${JSON.stringify(other)}
worktree_root = ${JSON.stringify(join(root, "code", "otherrepo-wt"))}
`,
      );
      const mine = identityFrom(mainClone, userConfig);
      const theirs = identityFrom(other, userConfig);
      expect(theirs.repoId).not.toBe(mine.repoId);
      expect(theirs.repoPath).toBe(other);
      expect(theirs.tmuxSocket).not.toBe(mine.tmuxSocket);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
