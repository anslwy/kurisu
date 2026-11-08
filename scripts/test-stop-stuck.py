#!/usr/bin/env python3
"""Stop an ACP agent that ignores cancel, then reuse the persisted session.
Fresh /tmp home, local fixture only; no browser, credentials, or web listener.
"""
import concurrent.futures
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

binary = str(Path(sys.argv[1]).resolve())
fixture = Path(__file__).parent / 'fixtures/opencode-acp.py'
with tempfile.TemporaryDirectory(prefix='ku-stop-stuck-', dir='/tmp') as tmp:
    root = Path(tmp)
    bins = root / 'bin'
    bins.mkdir()
    (bins / 'opencode').symlink_to(fixture.resolve())
    (bins / 'python3').symlink_to(sys.executable)
    env = dict(os.environ, KURISU_HOME=tmp, PATH=str(bins),
               KURISU_OPENCODE_BIN=str(fixture.resolve()),
               KURISU_OPENCODE_TIMEOUT_SECS='120', FIXTURE_IGNORE_CANCEL='1')
    def req(op, **fields):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(15)
            s.connect(str(root / 'kurisu.sock'))
            s.sendall((json.dumps(dict(op=op, **fields)) + '\n').encode())
            r = json.loads(s.makefile().readline())
            if 'error' in r:
                raise RuntimeError(r['error'])
            return r['ok']
    with (root / 'server.log').open('w') as log:
        p = subprocess.Popen([binary, 'serve'], env=env, stdout=log, stderr=log)
        try:
            for _ in range(100):
                if (root / 'kurisu.sock').exists(): break
                assert p.poll() is None, (root / 'server.log').read_text()
                time.sleep(.05)
            sid = req('create', name='stuck', harness='opencode', model='fixture/free', cwd=str(root / 'work'))['id']
            assert req('send', name=sid, text='hello')['reply'] == 'PONG'
            bound = req('status', name=sid)['harness_session_id']
            old_pid = int((root / 'fixture-pids').read_text().splitlines()[-1])
            with concurrent.futures.ThreadPoolExecutor() as pool:
                f = pool.submit(req, 'send', name=sid, text='FIXTURE_WAIT')
                for _ in range(100):
                    requests = [json.loads(x) for x in (root / 'fixture-requests.jsonl').read_text().splitlines()]
                    if any('FIXTURE_WAIT' in json.dumps(x) for x in requests): break
                    time.sleep(.02)
                else: raise AssertionError('prompt not started')
                stopped = req('interrupt', name=sid)
                assert stopped['stopped'], stopped
                try: f.result(timeout=2)
                except RuntimeError as e: assert 'stopped' in str(e), str(e)
                else: raise AssertionError('stuck prompt should be stopped')
            assert req('status', name=sid)['status'] == 'stopped'
            try: os.kill(old_pid, 0)
            except ProcessLookupError: pass
            else: raise AssertionError('stopped ACP process still exists')
            history = req('history_page', name=sid, tools=True)['messages']
            waiting = [x for x in history if (x.get('data') or {}).get('tool_call_id') == 'waiting']
            assert waiting and all(x['data']['status'] == 'cancelled' for x in waiting), waiting
            assert req('send', name=sid, text='after forced stop')['reply'] == 'PONG'
            assert req('status', name=sid)['harness_session_id'] == bound
            requests = [json.loads(x) for x in (root / 'fixture-requests.jsonl').read_text().splitlines()]
            assert any(x.get('method') == 'session/cancel' for x in requests)
            assert any(x.get('method') == 'session/load' and x['params']['sessionId'] == bound for x in requests)
            print('PASS ignored ACP cancel: stopped state, released turn, persisted session reload, next message succeeds')
        finally:
            try: req('shutdown')
            except Exception: pass
            try: p.wait(timeout=5)
            except subprocess.TimeoutExpired: p.kill(); p.wait()
