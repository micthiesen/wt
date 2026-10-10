#!/usr/bin/env python3
"""Exercise native CLI commands in an isolated temporary Git/HOME fixture."""

import fcntl
import json
import os
from datetime import datetime, timezone
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
        fake_tail = bin_dir / "tail"
        fake_editor = bin_dir / "fixture-editor"
        fake_gh.write_text("#!/bin/sh\nprintf 'isolated gh fixture\n' >&2\nexit 1\n", encoding="utf-8")
        fake_tmux.write_text("#!/bin/sh\nprintf 'no server running on isolated fixture\n' >&2\nexit 1\n", encoding="utf-8")
        fake_tail.write_text("#!/bin/sh\nshift 3\ncat \"$1\"\n", encoding="utf-8")
        fake_editor.write_text("#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"$WT_FIXTURE_EDITOR\"\n", encoding="utf-8")
        fake_gh.chmod(0o755)
        fake_tmux.chmod(0o755)
        fake_tail.chmod(0o755)
        fake_editor.chmod(0o755)
        git_config = root / "gitconfig"
        git_config.write_text("", encoding="utf-8")
        env = os.environ.copy()
        env.update({
            "HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_CACHE_HOME": str(home / ".cache"), "XDG_STATE_HOME": str(home / ".local/state"),
            "WT_REPO_CONFIG": str(main_repo / ".wt.toml"), "GIT_CONFIG_GLOBAL": str(git_config),
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0",
            "PATH": str(bin_dir) + os.pathsep + os.environ.get("PATH", ""),
            "WT_FIXTURE_EDITOR": str(root / "editor-targets"),
        })
        # Every version spelling reports the same build without loading even
        # malformed repository configuration. Recovery must work outside a repo.
        broken_config = root / "broken.toml"
        broken_config.write_text("not valid TOML [", encoding="utf-8")
        broken_env = dict(env, WT_CONFIG=str(broken_config), WT_REPO_CONFIG=str(broken_config))
        versions = [check_command(binary, root, broken_env, flag).stdout for flag in ("version", "--version", "-v")]
        require(len(set(versions)) == 1 and "(" in versions[0], "version aliases disagree or omit build identity")
        git(main_repo, "init", "-b", "main", env=env)
        git(main_repo, "config", "user.name", "Native Fixture", env=env)
        git(main_repo, "config", "user.email", "native@example.invalid", env=env)
        (main_repo / "README.md").write_text("fixture\n", encoding="utf-8")
        git(main_repo, "add", "README.md", env=env)
        git(main_repo, "commit", "-m", "fixture base", env=env)
        origin = root / "origin.git"
        git(root, "init", "--bare", "--initial-branch=main", str(origin), env=env)
        git(main_repo, "remote", "add", "origin", str(origin), env=env)
        git(main_repo, "push", "origin", "main", env=env)
        git(main_repo, "worktree", "add", "-b", "feature/one", str(worktree), "main", env=env)
        git(worktree, "push", "origin", "feature/one", env=env)
        git(worktree, "branch", "--set-upstream-to=origin/main", env=env)
        (worktree / "feature.txt").write_text("unpushed work\n", encoding="utf-8")
        git(worktree, "add", "feature.txt", env=env)
        git(worktree, "commit", "-m", "one unpushed commit", env=env)
        (worktree / ".sst").mkdir()
        (worktree / ".sst/stage").write_text("fixture-one\n", encoding="utf-8")
        (worktree / ".sst/outputs.json").write_text('{"stage":"fixture-one"}\n', encoding="utf-8")

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
            "[deploy.sst]", 'state_bucket = "native-fixture"', 'state_prefix = "wt/"', 'aws_profile = "fixture"',
            "[issue_tracker]", 'url_template = "https://tracker.invalid/{id}"',
            "read_command = [" + ", ".join(map(json.dumps, [sys.executable, "-c", "import sys; print('reader:'+sys.argv[1]); print('partial-error', file=sys.stderr); sys.exit(7)", "{id}"])) + "]",
            "[editor]", f"command = {json.dumps(str(fake_editor) + ' {{path}}')}",
            "",
        ])
        (main_repo / ".wt.toml").write_text(config, encoding="utf-8")

        # Exercise issue mutations, explicit no-issue, clearing overrides, and
        # read-command substitution/output from a failing configured reader.
        # With the isolated HOME, this also validates that multi-unit `sync`
        # and the `-y` spelling reach the command even when no harness targets
        # are installed for the fixture user.
        check_command(binary, worktree, env, "skills", "sync", "wt", "start", "-y")
        check_command(binary, worktree, env, "skills", "install", "wt", "start", "--yes")
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

        # Exercise user-facing log tailing, manager report delivery into the
        # local spool, and editor target resolution without invoking services.
        log_dir = home / "logs"
        log_dir.mkdir(parents=True, exist_ok=True)
        retained_log = log_dir / "one-2026-10-09T19-30-00.log"
        retained_log.write_text("fixture destroy output\n", encoding="utf-8")
        logs = check_command(binary, worktree, env, "logs", "one")
        require("fixture destroy output" in logs.stdout and str(retained_log) in logs.stderr,
                "wt logs did not select and stream the exact retained slug log")
        opened = check_command(binary, worktree, env, "open", "one")
        editor_targets = Path(env["WT_FIXTURE_EDITOR"]).read_text(encoding="utf-8").splitlines()
        require(opened.returncode == 0 and [Path(path).resolve() for path in editor_targets] == [worktree.resolve()],
                f"wt open did not pass the exact worktree path to the configured editor: {editor_targets!r}")
        check_command(binary, worktree, env, "manager", "report", "--warn", "isolated fixture report")
        report_spool = cache_root / "manager/reports.jsonl"
        reports = [json.loads(line) for line in report_spool.read_text(encoding="utf-8").splitlines()]
        require(len(reports) == 1 and reports[0].get("level") == "warn"
                and reports[0].get("text") == "isolated fixture report" and reports[0].get("at"),
                f"wt manager report wrote an invalid structured report: {reports!r}")

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

        # `wt ls` retains its stage/PR columns and JSON fact contract. The
        # fake gh executable makes PR unavailability explicit without using
        # the caller's authentication.
        listed = check_command(binary, worktree, env, "ls")
        require("STAGE" in listed.stdout and "PR" in listed.stdout, "wt ls omitted the configured stage or PR column")
        require("fixture-one" in listed.stdout and "unavailable" in listed.stdout, "wt ls omitted the pinned deployment or explicit unavailable PR state")
        listed_json = json.loads(check_command(binary, worktree, env, "ls", "--json").stdout)
        live_row = next((row for row in listed_json if row.get("slug") == "one"), None)
        require(live_row is not None, "wt ls --json omitted the fixture worktree")
        require(live_row.get("deployed") is True and live_row.get("stage") == "fixture-one", "wt ls --json omitted safe local deployment facts")
        require({"status_age", "status_op", "dev", "ahead_of_base", "pushed", "unpushed"}.issubset(live_row), "wt ls --json omitted status/dev/push facts")
        require(live_row.get("issue_id") == "LIVE-5", "wt ls --json omitted the explicit issue identity")
        require(live_row.get("pushed") is True and live_row.get("unpushed") == 1, "wt ls --json used configured upstream instead of origin/<branch> for push facts")
        require(live_row.get("ahead_of_base") == 1, "wt ls --json omitted commits ahead of the effective base")

        # Hold a fixture operation lock while the command runs to verify the
        # JSON status fields carry observed age/op facts instead of nulls.
        lock_dir = home / "locks"
        lock_dir.mkdir(exist_ok=True)
        lock_path = lock_dir / "one.lock"
        lock_time = datetime.now(timezone.utc).isoformat()
        with lock_path.open("w+", encoding="utf-8") as lock_file:
            lock_file.write(json.dumps({"op": "restack", "phase": "fetch", "startedAt": lock_time, "phaseStarted": lock_time}))
            lock_file.flush()
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            busy_json = json.loads(check_command(binary, worktree, env, "ls", "--json").stdout)
            busy_row = next((row for row in busy_json if row.get("slug") == "one"), None)
            require(busy_row is not None and busy_row.get("status") == "busy", "wt ls --json missed an active operation lock")
            require(busy_row.get("status_op") == "restack" and busy_row.get("status_age"), "wt ls --json omitted active operation age/op facts")

        # A worker can observe a stored section, but layout belongs to the
        # controller and the public worker snapshot must report null.
        state_db = sqlite3.connect(home / "state.sqlite")
        state_text = state_db.execute("SELECT data FROM repository_state LIMIT 1").fetchone()[0]
        worker_state = json.loads(state_text)
        worker_state.setdefault("slugs", {}).setdefault("one", {})["section"] = "Controller only"
        state_db.execute("UPDATE repository_state SET data = ?", (json.dumps(worker_state),))
        state_db.commit()
        state_db.close()
        worker_config = root / "worker.wt.toml"
        worker_config.write_text(config + '\n[instance]\nrole = "worker"\n', encoding="utf-8")
        worker_env = env.copy()
        worker_env["WT_REPO_CONFIG"] = str(worker_config)
        worker_json = json.loads(check_command(binary, worktree, worker_env, "ls", "--json").stdout)
        worker_row = next((row for row in worker_json if row.get("slug") == "one"), None)
        require(worker_row is not None and worker_row.get("section") is None, "worker wt ls --json leaked controller section state")

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
                require(report.get("kind") == "live" and isinstance(report.get("session"), dict)
                        and isinstance(report.get("dev"), dict), "fleet JSON omitted discriminator or nested nullable source objects")
                require(report.get("pr") is None and isinstance(report.get("pr_note"), str)
                        and report.get("session_note") is None and report["session"]["alive"] is False,
                        "fleet JSON did not distinguish unavailable GitHub from a known absent tmux server")
                fake_tmux.write_text("#!/bin/sh\nprintf 'isolated tmux probe failed\\n' >&2\nexit 1\n", encoding="utf-8")
                unknown = json.loads(check_command(binary, worktree, env, "fleet", "--json").stdout)
                unknown_row = next(row for row in unknown if row.get("slug") == "one")
                require(unknown_row["session"]["alive"] is None and unknown_row.get("session_note"),
                        "fleet treated a failed tmux probe as a stopped session")
            else:
                required = {"sampled_at_ms", "cpu_note", "system_cpu", "wt_cpu", "category_totals", "sessions", "orphans", "orphan_probe_available", "tmux_probe_available"}
                require(required.issubset(parsed), f"perf snapshot omitted measurement/attribution fields: {required - set(parsed)}")
                require(parsed["sampled_at_ms"] > 0 and parsed["cpu_note"], "perf snapshot omitted measurement window caveat")
        print("native issue/state/list/doctor/fleet/perf command checks passed in isolated HOME and Git fixtures")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, subprocess.TimeoutExpired, RuntimeError, sqlite3.Error) as error:
        print(f"native command check failed: {error}", file=sys.stderr)
        raise SystemExit(1)
