#!/usr/bin/env python3
"""Exercise native CLI commands in an isolated temporary Git/HOME fixture."""

import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile


def run(binary: str, cwd: Path, env: dict[str, str], *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run([binary, *args], cwd=cwd, env=env, capture_output=True, text=True, timeout=45)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def git(cwd: Path, *args: str, env: dict[str, str]) -> None:
    result = subprocess.run(["git", *args], cwd=cwd, env=env, capture_output=True, text=True, timeout=15)
    require(result.returncode == 0, f"git {' '.join(args)} failed: {result.stderr.strip()}")


def create_stranded_database(path: Path, worktree: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(path) as db:
        db.executescript("""
            CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
            INSERT INTO schema_migrations(version, applied_at) VALUES(1, 1);
            CREATE TABLE repositories(repo_id TEXT PRIMARY KEY, repo_path TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
            CREATE TABLE repository_state(repo_id TEXT PRIMARY KEY REFERENCES repositories(repo_id) ON DELETE CASCADE, data TEXT NOT NULL, updated_at INTEGER NOT NULL);
            CREATE TABLE archived_worktrees(repo_id TEXT NOT NULL REFERENCES repositories(repo_id) ON DELETE CASCADE, worktree_key TEXT NOT NULL, archived_at INTEGER NOT NULL, PRIMARY KEY(repo_id, worktree_key));
        """)
        data = {"version": 17, "slugs": {"one": {"baseBranch": "main", "section": "Stranded"}}}
        db.execute("INSERT INTO repositories VALUES(?,?,1,1)", ("stray-id", str(worktree)))
        db.execute("INSERT INTO repository_state VALUES(?,?,1)", ("stray-id", json.dumps(data)))
        db.execute("INSERT INTO archived_worktrees VALUES(?,?,1)", ("stray-id", "one"))


def check_command(binary: str, worktree: Path, env: dict[str, str], name: str, *args: str) -> subprocess.CompletedProcess[str]:
    result = run(binary, worktree, env, name, *args)
    require(result.returncode == 0, f"wt {name} {' '.join(args)} failed ({result.returncode}): {result.stderr.strip()}\n{result.stdout}")
    return result


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: native-command-check.py <wt-binary>", file=sys.stderr)
        return 2
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="wt-native-command-") as scratch:
        root = Path(scratch)
        home = root / "home"
        main_repo = root / "repo"
        worktree_root = root / "worktrees"
        worktree = worktree_root / "one"
        cache_root = home / ".cache/wt/current"
        legacy = home / ".cache/wt/legacy"
        bin_dir = root / "bin"
        for directory in (home, main_repo, worktree_root, bin_dir, legacy, cache_root):
            directory.mkdir(parents=True, exist_ok=True)

        # Block access to a real user's GitHub auth and tmux server. The command
        # under test must report these optional sources as unavailable.
        fake_gh = bin_dir / "gh"
        fake_tmux = bin_dir / "tmux"
        fake_gh.write_text("#!/bin/sh\nprintf 'isolated gh fixture\n' >&2\nexit 1\n", encoding="utf-8")
        fake_tmux.write_text("#!/bin/sh\nprintf 'no server running on isolated fixture\n' >&2\nexit 1\n", encoding="utf-8")
        fake_gh.chmod(0o755)
        fake_tmux.chmod(0o755)
        git_config = root / "gitconfig"
        git_config.write_text("", encoding="utf-8")
        env = os.environ.copy()
        env.update({
            "HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_CACHE_HOME": str(home / ".cache"), "XDG_STATE_HOME": str(home / ".local/state"),
            "WT_REPO_CONFIG": str(main_repo / ".wt.toml"), "GIT_CONFIG_GLOBAL": str(git_config),
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0",
            "PATH": str(bin_dir) + os.pathsep + os.environ.get("PATH", ""),
        })
        git(main_repo, "init", "-b", "main", env=env)
        git(main_repo, "config", "user.name", "Native Fixture", env=env)
        git(main_repo, "config", "user.email", "native@example.invalid", env=env)
        (main_repo / "README.md").write_text("fixture\n", encoding="utf-8")
        git(main_repo, "add", "README.md", env=env)
        git(main_repo, "commit", "-m", "fixture base", env=env)
        git(main_repo, "worktree", "add", "-b", "feature/one", str(worktree), "main", env=env)

        config = "\n".join([
            "[paths]",
            f"main_clone = {json.dumps(str(main_repo))}",
            f"worktree_root = {json.dumps(str(worktree_root))}",
            f"state_db = {json.dumps(str(home / 'state.sqlite'))}",
            f"cache_db = {json.dumps(str(cache_root / 'cache.sqlite'))}",
            f"cache_root = {json.dumps(str(cache_root))}",
            f"lock_dir = {json.dumps(str(home / 'locks'))}",
            f"log_dir = {json.dumps(str(home / 'logs'))}",
            f"app_log_dir = {json.dumps(str(home / 'logs/app'))}",
            f"dotfiles = {json.dumps(str(home / 'dotfiles'))}",
            "[branch]", 'prefix = "feature/"', 'base = "main"',
            "[stage]", 'prefix = "fixture-"',
            "[issue_tracker]", 'url_template = "https://tracker.invalid/{id}"',
            "read_command = [" + ", ".join(map(json.dumps, [sys.executable, "-c", "import sys; print('reader:'+sys.argv[1]); print('partial-error', file=sys.stderr); sys.exit(7)", "{id}"])) + "]",
            "",
        ])
        (main_repo / ".wt.toml").write_text(config, encoding="utf-8")

        # Exercise issue mutations, explicit no-issue, clearing overrides, and
        # read-command substitution/output from a failing configured reader.
        check_command(binary, worktree, env, "issue", "one", "--id", "coz-51")
        check_command(binary, worktree, env, "issue", "one", "--no-id")
        check_command(binary, worktree, env, "issue", "one", "--clear-id")
        check_command(binary, worktree, env, "issue", "one", "--gh", "12")
        check_command(binary, worktree, env, "issue", "one", "--clear-gh")
        check_command(binary, worktree, env, "issue", "one", "--id", "LIVE-5")
        read = run(binary, worktree, env, "issue", "one", "--read")
        require(read.returncode == 7 and "reader:LIVE-5" in read.stdout and "partial-error" in read.stderr, "issue --read did not preserve substituted arguments and partial output on reader failure")
        bad_number = run(binary, worktree, env, "issue", "one", "--gh", "0")
        require(bad_number.returncode == 2, "issue --gh accepted zero")

        # Legacy JSON, a stranded per-worktree namespace, and harness runtime
        # files all migrate from temp-only sources. Unknown fields and current
        # values survive, while foreign slug rows remain outside this repo.
        legacy_state = {"version": 17, "futureRoot": {"kept": True}, "slugs": {"one": {"issueId": "OLD-1"}, "foreign": {"issueId": "FOREIGN-1"}}, "sectionsOrder": ["Stranded"]}
        (legacy / "state.json").write_text(json.dumps(legacy_state), encoding="utf-8")
        (legacy / "archive.json").write_text(json.dumps({"futureArchive": 9, "slugs": ["one", "foreign"]}), encoding="utf-8")
        (legacy / "claude-sessions.json").write_text(json.dumps({"one": ["legacy-session"]}), encoding="utf-8")
        (cache_root / "claude-sessions.json").write_text(json.dumps({"one": ["current-session"]}), encoding="utf-8")
        (legacy / "automations.json").write_text('{"once":true}\n', encoding="utf-8")
        create_stranded_database(home / ".cache/wt/stray/wt.sqlite", worktree)
        migrate = check_command(binary, worktree, env, "state", "migrate", "--from", str(legacy))
        require("migrated 1" in migrate.stdout and "adopted 1 stranded" in migrate.stdout, "state migration omitted live or stranded data")
        state_db = sqlite3.connect(home / "state.sqlite")
        state_text = state_db.execute("SELECT data FROM repository_state LIMIT 1").fetchone()[0]
        state_db.close()
        migrated = json.loads(state_text)
        require(migrated["futureRoot"] == {"kept": True}, "migration dropped unknown root fields")
        require(migrated["slugs"]["one"]["issueId"] == "LIVE-5", "migration overwrote the current issue id")
        require(migrated["slugs"]["one"].get("baseBranch") == "main", f"migration missed stranded worktree fields: {migrated!r}")
        require("foreign" not in migrated["slugs"], "migration adopted a foreign legacy slug")
        legacy_after = json.loads((legacy / "state.json").read_text(encoding="utf-8"))
        archive_after = json.loads((legacy / "archive.json").read_text(encoding="utf-8"))
        require(legacy_after["futureRoot"] == {"kept": True} and "foreign" in legacy_after["slugs"], "legacy pruning dropped unknown or foreign state")
        require(archive_after["futureArchive"] == 9 and archive_after["slugs"] == ["foreign"], "archive pruning dropped unknown or foreign archive keys")
        require(any("bak-sqlite-" in path.name for path in legacy.iterdir()), "migration did not make durable source backups")
        sessions = json.loads((cache_root / "claude-sessions.json").read_text(encoding="utf-8"))
        require(sessions["one"] == ["current-session", "legacy-session"], f"migration failed to merge session registries: {sessions!r}; source={(legacy / 'claude-sessions.json').read_text(encoding='utf-8')!r}; destination={(cache_root / 'claude-sessions.json').read_text(encoding='utf-8')!r}")
        require((cache_root / "automations.json").exists(), "migration did not carry the automation ledger")
        check_command(binary, worktree, env, "state", "migrate", "--from", str(legacy))

        # Machine-readable diagnostics use only the fixture HOME/repo and fake
        # gh/tmux executables, so they cannot inspect the caller's services.
        # Doctor's dependency probe applies to managed JavaScript projects,
        # independent of wt's own native runtime.
        (worktree / "package.json").write_text('{}\n', encoding="utf-8")
        (worktree / "pnpm-lock.yaml").write_text('lockfileVersion: 9\n', encoding="utf-8")
        (worktree / "node_modules").mkdir()
        for name, args in (("doctor", ("--all", "--json")), ("fleet", ("--json",)), ("perf", ("--json",))):
            result = check_command(binary, worktree, env, name, *args)
            parsed = json.loads(result.stdout)
            require(isinstance(parsed, list if name in ("doctor", "fleet") else dict), f"wt {name} --json returned the wrong shape")
            if name == "doctor":
                report = next((row for row in parsed if row.get("slug") == "one"), None)
                require(report is not None, "doctor omitted the live worktree")
                names = {check.get("name") for check in report.get("checks", [])}
                require({"operation lock", "gh merge base", "dependencies"}.issubset(names), f"doctor omitted actionable native health checks: {names!r}")
                pnpm = next((check for check in report["checks"] if check.get("name") == "pnpm tree"), None)
                if pnpm is not None:
                    require("does not scan pnpm's isolated virtual store" in pnpm.get("message", ""), "doctor's pnpm scope omission is unexplained")
            elif name == "fleet":
                report = next((row for row in parsed if row.get("slug") == "one"), None)
                require(report is not None, "fleet omitted the live worktree")
                require("session" in report and "operation" in report and "dev" in report, "fleet omitted session, lock, or dev-server availability facts")
            else:
                required = {"sampled_at_ms", "cpu_note", "system_cpu", "wt_cpu", "category_totals", "sessions", "orphans", "orphan_probe_available", "tmux_probe_available"}
                require(required.issubset(parsed), f"perf snapshot omitted measurement/attribution fields: {required - set(parsed)}")
                require(parsed["sampled_at_ms"] > 0 and parsed["cpu_note"], "perf snapshot omitted measurement window caveat")
        print("native issue/state/doctor/fleet/perf command checks passed in isolated HOME and Git fixtures")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, subprocess.TimeoutExpired, RuntimeError, sqlite3.Error) as error:
        print(f"native command check failed: {error}", file=sys.stderr)
        raise SystemExit(1)
