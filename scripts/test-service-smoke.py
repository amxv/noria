#!/usr/bin/env python3
"""Exercise the installed OS service with isolated state and no accounts/prompts."""
import hashlib
import json
import os
import platform
from pathlib import Path
import socket
import subprocess
import tempfile
import time

repo = Path(__file__).resolve().parent.parent
binary = Path(os.environ.get('NORIA_TEST_SERVICE_BIN', str(repo / ('dist/noria.exe' if os.name == 'nt' else 'dist/noria')))).resolve()
with tempfile.TemporaryDirectory(prefix='noria-service-smoke-', dir=None if os.name == 'nt' else '/tmp') as root:
    root = Path(root).resolve()
    home = root / 'pool'
    client = root / 'client'
    client.mkdir()
    settings = client / 'settings.json'
    initial = {'theme': 'dark', 'apiKeyHelper': 'original-helper', 'env': {'OTHER': 'sentinel'}}
    settings.write_text(json.dumps(initial))
    env = dict(os.environ, HOME=str(root), USERPROFILE=str(root), XDG_CONFIG_HOME=str(root / 'config'), XDG_CACHE_HOME=str(root / 'cache'), XDG_DATA_HOME=str(root / 'data'), NORIA_HOME=str(home), APPDATA=str(root / 'appdata'), LOCALAPPDATA=str(root / 'localappdata'))
    for name in tuple(env):
        if name.startswith(('ANTHROPIC_', 'CLAUDE_', 'CODEX_', 'OPENAI_')):
            env.pop(name)
    env.update(CODEX_HOME=str(root / 'codex'), CLAUDE_CONFIG_DIR=str(root / 'claude'))
    def run(*args, check=True):
        start = time.monotonic()
        print(f'Running {" ".join(args)}', flush=True)
        value = subprocess.run([str(binary), *args], env=env, cwd=root, capture_output=True, text=True, timeout=25)
        if check and value.returncode:
            raise RuntimeError(f'{args}: {value.stderr}\n{value.stdout}')
        print(f'{" ".join(args)} [{time.monotonic()-start:.2f}s]: {value.stdout.strip()}', flush=True)
        return value
    run('init')
    config_path = home / 'config.json'
    config = json.loads(config_path.read_text())
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    config['listen'] = f'127.0.0.1:{port}'
    config_path.write_text(json.dumps(config))
    label = 'xyz.ashray.noria.' + hashlib.sha256(str(home).encode()).hexdigest()[:12]
    try:
        run('start', '--json')
        run('start', '--json')
        run('integrate', 'claude', '--client-home', str(client), '--yes', '--json')
        routed = json.loads(settings.read_text())
        assert routed['env']['ANTHROPIC_BASE_URL'] == f'http://127.0.0.1:{port}/claude'
        routed['theme'] = 'light'
        settings.write_text(json.dumps(routed))
        live = json.loads(run('status', '--json').stdout)
        assert live['running'] and live['integrations'][0]['active']
        run('stop', '--json')
        restored = json.loads(settings.read_text())
        assert restored == dict(initial, theme='light'), restored
        assert not json.loads(run('status', '--json').stdout)['running']
        run('start', '--json')
        assert json.loads(settings.read_text())['env']['ANTHROPIC_BASE_URL'].endswith('/claude')
        run('restart', '--json')
        assert json.loads(run('status', '--json').stdout)['running']
        if os.environ.get('NORIA_TEST_SKIP_LOGIN') != '1':
            run('preferences', 'start-at-login', 'on', '--json')
            run('preferences', 'start-at-login', 'off', '--json')
        run('stop', '--json')
        run('stop', '--json')
        assert json.loads(settings.read_text()) == dict(initial, theme='light')
        login = ', login preference' if os.environ.get('NORIA_TEST_SKIP_LOGIN') != '1' else ' (login preference skipped)'
        print(f'PASS: background lifecycle, idempotence{login}, direct restoration, and unrelated edits.', flush=True)
    finally:
        run('stop', '--json', check=False)
        # Only this test's unique job; never touch another Noria home/service.
        if platform.system() == 'Darwin':
            subprocess.run(['launchctl', 'bootout', f'gui/{os.getuid()}/{label}'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if (home / 'logs/service.log').exists():
            print('Service log:', (home / 'logs/service.log').read_text()[-1600:])
