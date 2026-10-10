#!/usr/bin/env python3
"""Exercise a published native controller against an explicitly selected SSH host.

Creates two temporary worker repositories and one private tmux server. Only the
fixture directories and newly provisioned, unused matching runtimes are removed.
Requires Python 3, Git and tmux on the chosen host; never installs dependencies.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import uuid


SETUP = r'''
import json, os, pathlib, subprocess, sys
root = pathlib.Path(sys.argv[1])
root.mkdir(mode=0o700)
runtime_root = pathlib.Path.home() / '.cache/wt/native-runtimes'
before = sorted(str(p) for p in runtime_root.glob('*/*/wt'))
(root / 'before.json').write_text(json.dumps(before))
env = dict(os.environ, GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null')
for key in ('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'TMUX', 'TMUX_PANE'):
    env.pop(key, None)
socket = root.name
(root / 'socket').write_text(socket)
def run(*argv):
    return subprocess.check_output(argv, env=env, stderr=subprocess.STDOUT, text=True)
configs = {}
for name in ('a', 'b'):
    project = root / name
    main = project / 'main'
    worktrees = project / 'worktrees'
    project.mkdir()
    worktrees.mkdir()
    run('git', 'init', '-b', 'main', str(main))
    run('git', '-C', str(main), 'config', 'user.name', 'wt SSH fixture')
    run('git', '-C', str(main), 'config', 'user.email', 'ssh-fixture@example.invalid')
    run('git', '-C', str(main), 'config', 'commit.gpgSign', 'false')
    run('git', '-C', str(main), 'config', 'core.hooksPath', '/dev/null')
    (main / 'fixture.txt').write_text('isolated fixture\n')
    run('git', '-C', str(main), 'add', 'fixture.txt')
    run('git', '-C', str(main), 'commit', '-m', 'fixture')
    origin = project / 'origin.git'
    run('git', 'clone', '--bare', str(main), str(origin))
    run('git', '-C', str(main), 'remote', 'add', 'origin', str(origin))
    run('git', '-C', str(main), 'fetch', 'origin')
    run('git', '-C', str(main), 'worktree', 'add', '-b', 'fixture/same', str(worktrees / 'same'))
    config = project / 'worker.toml'
    config.write_text('\n'.join([
        '[instance]', 'role = "worker"', '[paths]',
        'main_clone = ' + json.dumps(str(main)),
        'worktree_root = ' + json.dumps(str(worktrees)),
        'state_db = ' + json.dumps(str(project / 'state.sqlite')),
        'cache_db = ' + json.dumps(str(project / 'cache.sqlite')),
        'lock_dir = ' + json.dumps(str(project / 'locks')),
        'log_dir = ' + json.dumps(str(project / 'logs')),
        '[branch]', 'prefix = "fixture"', 'base = "main"',
        '[tmux]', 'socket = ' + json.dumps(socket),
        '[naming]', 'auto_rename = false',
    ]) + '\n')
    configs[name] = str(config)
run('tmux', '-L', socket, '-f', '/dev/null', 'new-session', '-d', '-s', 'sentinel', 'sleep 300')
print(json.dumps({'configs': configs, 'before': before, 'socket': socket}))
'''

INSPECT = r'''
import hashlib, json, pathlib, subprocess, sys
root = pathlib.Path(sys.argv[1])
build = sys.argv[2]
runtime_root = pathlib.Path.home() / '.cache/wt/native-runtimes'
matching = []
for binary in runtime_root.glob('*/*/wt'):
    # Inspect only cache entries newly introduced during this fixture.
    if str(binary) in json.loads((root / 'before.json').read_text()):
        continue
    probe = subprocess.check_output([str(binary), '--_boot-probe'], text=True).strip()
    if probe == 'wt-build-id:' + build + ':x86_64-unknown-linux-gnu':
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == binary.parent.name
        matching.append(str(binary))
subprocess.run(['tmux', '-L', (root / 'socket').read_text(), 'has-session', '-t', '=sentinel'], check=True)
print(json.dumps({'new_matching_runtimes': matching, 'sentinel_alive': True}))
'''

CLEANUP = r'''
import json, pathlib, shutil, subprocess, sys
root = pathlib.Path(sys.argv[1])
assert root.parent == pathlib.Path('/tmp') and root.name.startswith('wt-native-ssh-')
if root.exists():
    if (root / 'socket').exists():
        subprocess.run(['tmux', '-L', (root / 'socket').read_text(), 'kill-server'], capture_output=True)
    before = json.loads((root / 'before.json').read_text()) if (root / 'before.json').exists() else None
    if before is not None:
        runtime_root = pathlib.Path.home() / '.cache/wt/native-runtimes'
        for binary in runtime_root.glob('*/*/wt'):
            if str(binary) in before:
                continue
            probe = subprocess.run([str(binary), '--_boot-probe'], capture_output=True, text=True)
            if probe.stdout.strip() != 'wt-build-id:' + sys.argv[2] + ':x86_64-unknown-linux-gnu':
                continue
            used = False
            for executable in pathlib.Path('/proc').glob('[0-9]*/exe'):
                try:
                    used |= executable.resolve() == binary
                except OSError:
                    pass
            if not used:
                shutil.rmtree(binary.parent)
    shutil.rmtree(root)
print(json.dumps({'fixture_removed': not root.exists()}))
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--host', required=True)
    parser.add_argument('--binary', type=Path, required=True, help='Installed native launcher')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.absolute()
    args.output.mkdir(parents=True, exist_ok=False)
    remote_root = '/tmp/wt-native-ssh-' + uuid.uuid4().hex[:12]
    env = {key: value for key, value in os.environ.items() if not key.startswith('WT_')}
    # Retain the isolated install root selected by the caller, never a global install.
    if 'WT_INSTALL_ROOT' in os.environ:
        env['WT_INSTALL_ROOT'] = os.environ['WT_INSTALL_ROOT']
    env.update(WT_UPDATE='off', WT_AUTO_UPDATE='off', WT_SKILLS='off', WT_GITHUB='off', WT_AUTOMATIONS='off')

    def ssh(script, *values):
        command = shlex.join(['python3', '-c', script, remote_root, *values])
        result = subprocess.run(['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', args.host, command],
                                capture_output=True, text=True, timeout=90)
        if result.returncode:
            raise AssertionError(f'SSH fixture failed: {result.stderr}\n{result.stdout}')
        return json.loads(result.stdout)

    def wt(*argv):
        result = subprocess.run([str(binary), *argv], env=env, capture_output=True, text=True, timeout=180)
        if result.returncode:
            raise AssertionError(f'wt {argv!r}: {result.stdout}\n{result.stderr}')
        return result.stdout

    build = wt('--_boot-probe').strip().split(':')[1]
    try:
        setup = ssh(SETUP)
        with tempfile.TemporaryDirectory(prefix='wt-native-ssh-local-') as temporary:
            root = Path(temporary)
            (root / 'main').mkdir()
            (root / 'worktrees').mkdir()
            config = root / 'controller.toml'
            config.write_text('\n'.join([
                '[paths]', f'main_clone = {json.dumps(str(root / "main"))}',
                f'worktree_root = {json.dumps(str(root / "worktrees"))}',
                f'state_db = {json.dumps(str(root / "state.sqlite"))}',
                f'cache_db = {json.dumps(str(root / "cache.sqlite"))}',
                f'log_dir = {json.dumps(str(root / "logs"))}',
                f'lock_dir = {json.dumps(str(root / "locks"))}',
                '[branch]', 'prefix = "fixture"',
                *[line for name in ('a', 'b') for line in (
                    '[[remotes]]', f'host = {json.dumps(args.host)}',
                    f'label = "fixture-{name}"', f'config = {json.dumps(setup["configs"][name])}',
                )],
            ]) + '\n')
            env['WT_CONFIG'] = str(config)
            initial = json.loads(wt('remote', '--host', 'fixture-a', 'ls', '--json'))
            assert any(row['slug'] == 'same' for row in initial), initial
            note = "Exact SSH argv: a quote ' and $HOME stay literal.\nSecond line."
            wt('remote', '--host', 'fixture-a', 'status', 'same', 'ready', '--risk', 'low', '-m', note)
            wt('remote', '--host', 'fixture-b', 'status', 'same', 'working', '-m', 'independent config')
            first = json.loads(wt('remote', '--host', 'fixture-a', 'ls', '--json'))
            second = json.loads(wt('remote', '--host', 'fixture-b', 'ls', '--json'))
            a = next(row for row in first if row['slug'] == 'same')
            b = next(row for row in second if row['slug'] == 'same')
            assert a['work_state'] == 'ready' and a['work_note'] == note, a
            assert b['work_state'] == 'working' and b['work_note'] == 'independent config', b
            inspected = ssh(INSPECT, build)
            result = {'build': build, 'host': args.host, 'cross_target_provisioned': True,
                      'same_slug_config_isolation': True, 'exact_argv': True, **inspected}
            (args.output / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
            print(json.dumps(result))
    finally:
        cleanup = ssh(CLEANUP, build)
        (args.output / 'cleanup.json').write_text(json.dumps(cleanup, indent=2) + '\n')
        print(json.dumps(cleanup))


if __name__ == '__main__':
    main()
