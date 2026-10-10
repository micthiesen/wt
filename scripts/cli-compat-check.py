#!/usr/bin/env python3
"""Compare stable CLI JSON with an explicitly supplied legacy checkout.

The legacy checkout is only a test oracle. Neither native installation nor CI
requires it. Every mutation happens in a temporary Git repository and HOME.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--reference', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    native = [str(args.binary.resolve())]
    reference = ['bun', str(args.reference.resolve() / 'src/main.ts')]
    args.output.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory(prefix='wt-cli-compat-') as temporary:
        root = Path(temporary).resolve()
        home, repo, trees, tools = [root / name for name in ('home', 'repo', 'trees', 'bin')]
        for path in (home, repo, trees, tools):
            path.mkdir()
        for program, message in [('gh', 'isolated GitHub unavailable'), ('tmux', 'no server running on isolated fixture')]:
            path = tools / program
            path.write_text(f'#!/bin/sh\nprintf "{message}\\n" >&2\nexit 1\n')
            path.chmod(0o700)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(('WT_', 'GIT_')) and key not in (
                   'HOME', 'CODEX_HOME', 'BUN_INSPECT', 'BUN_OPTIONS', 'NODE_OPTIONS',
                   'XDG_CONFIG_HOME', 'XDG_CACHE_HOME', 'XDG_STATE_HOME', 'TMUX', 'TMUX_PANE')}
        config = root / 'config.toml'
        env.update(HOME=str(home), PATH=str(tools) + os.pathsep + env.get('PATH', ''),
                   WT_CONFIG=str(config), WT_UPDATE='off', WT_SKILLS='off', WT_GITHUB='off',
                   WT_AUTOMATIONS='off', WT_NO_HINTS='1', GIT_CONFIG_NOSYSTEM='1',
                   GIT_CONFIG_GLOBAL='/dev/null', GIT_TERMINAL_PROMPT='0')

        def run(command, *argv, cwd=repo):
            result = subprocess.run([*command, *argv], cwd=cwd, env=env,
                                    text=True, capture_output=True, timeout=60)
            if result.returncode:
                # Preserve only this isolated fixture for diagnosing migration
                # or reference/runtime failures before TemporaryDirectory exits.
                shutil.copytree(root, args.output / 'failed-fixture', dirs_exist_ok=True)
                raise AssertionError(f'{command[0]} {argv}: exit {result.returncode}\n{result.stderr}\n{result.stdout}')
            return result.stdout

        run(['git'], 'init', '-b', 'main')
        run(['git'], 'config', 'user.name', 'CLI compatibility fixture')
        run(['git'], 'config', 'user.email', 'fixture@example.invalid')
        (repo / 'file.txt').write_text('base\n')
        run(['git'], 'add', 'file.txt')
        run(['git'], 'commit', '-m', 'base')
        origin = root / 'origin.git'
        run(['git'], 'clone', '--bare', str(repo), str(origin))
        run(['git'], 'remote', 'add', 'origin', str(origin))
        run(['git'], 'fetch', 'origin')
        worktree = trees / 'one'
        run(['git'], 'worktree', 'add', '-b', 'fixture/one', str(worktree))
        run(['git'], 'push', 'origin', 'fixture/one')
        config.write_text('\n'.join([
            '[paths]', f'main_clone = {json.dumps(str(repo))}',
            f'worktree_root = {json.dumps(str(trees))}',
            f'state_db = {json.dumps(str(root / "state.sqlite"))}',
            f'cache_db = {json.dumps(str(root / "cache/cache.sqlite"))}',
            f'lock_dir = {json.dumps(str(root / "locks"))}',
            f'log_dir = {json.dumps(str(root / "logs"))}',
            '[branch]', 'prefix = "fixture"', 'base = "main"',
            '[naming]', 'auto_rename = false',
        ]) + '\n')
        run(reference, 'status', 'one', 'ready', '--risk', 'low', '-m', 'Stable claim',
            '--blocked-on', 'External gate', '--verify-after-merge', 'STEPS: 1. Check deployment')
        # Bun 1.4's readonly SQLite cannot recreate absent WAL sidecars after
        # another runtime closes its final connection. Keep a neutral reader
        # attached while comparing; it changes no state and holds no transaction.
        keeper = sqlite3.connect(root / 'state.sqlite')
        keeper.execute('SELECT count(*) FROM repositories').fetchone()
        families = [('ls', '--json'), ('status', '--all', '--json'), ('fleet', '--json')]
        results = []
        differences = []

        def mismatch(path, expected, actual):
            differences.append({'path': path, 'reference': expected, 'native': actual})

        def compare(expected, actual, path=''):
            if isinstance(expected, dict):
                if not isinstance(actual, dict):
                    mismatch(path, expected, actual)
                    return
                for key, value in expected.items():
                    # Elapsed/age readings can cross a clock boundary between
                    # sequential programs. Their presence and type stay checked.
                    if key not in actual:
                        mismatch(f'{path}.{key}', value, '<missing>')
                        continue
                    if key in ('age', 'status_age'):
                        if type(value) is not type(actual[key]):
                            mismatch(f'{path}.{key}', value, actual[key])
                    elif key == 'pr_note' and value is not None:
                        # Native diagnostics preserve the actual gh stderr;
                        # the reference substitutes generic auth/remote advice.
                        if not isinstance(actual[key], str) or 'isolated GitHub unavailable' not in actual[key]:
                            mismatch(f'{path}.{key}', value, actual[key])
                    else:
                        compare(value, actual[key], f'{path}.{key}')
            elif isinstance(expected, list):
                if not isinstance(actual, list) or len(expected) != len(actual):
                    mismatch(path, expected, actual)
                    return
                for index, (old, new) in enumerate(zip(expected, actual)):
                    compare(old, new, f'{path}[{index}]')
            else:
                if type(expected) is not type(actual) or expected != actual:
                    mismatch(path, expected, actual)

        for scenario in ('clean', 'dirty', 'new-head'):
            if scenario == 'dirty':
                (worktree / 'file.txt').write_text('changed\n')
            elif scenario == 'new-head':
                run(['git'], 'add', 'file.txt', cwd=worktree)
                run(['git'], 'commit', '-m', 'unpublished change', cwd=worktree)
            for command in families:
                old = json.loads(run(reference, *command))
                new = json.loads(run(native, *command))
                label = scenario + '-' + command[0]
                for name, data in [('reference', old), ('native', new)]:
                    (args.output / f'{label}-{name}.json').write_text(json.dumps(data, indent=2) + '\n')
                count = len(differences)
                compare(old, new, label)
                if len(differences) == count:
                    results.append(label)
        report = {'passed': results, 'differences': differences}
        keeper.close()
        (args.output / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report))
        if differences:
            raise SystemExit(1)


if __name__ == '__main__':
    main()
