#!/usr/bin/env python3
"""/undo end to end on a throwaway KURISU_HOME with the local ACP fixture (no
credentials, no network, no web listener): a turn edits and creates files in
the session folder, `ku undo` restores them and removes the turn's history rows.

usage: test-undo.py <ku binary> [--tmpdir DIR]   (DIR defaults to $TMPDIR)
"""
import json, os, socket, subprocess, sys, tempfile, time
from pathlib import Path

binary = str(Path(sys.argv[1]).resolve())
base = sys.argv[sys.argv.index('--tmpdir') + 1] if '--tmpdir' in sys.argv else None
fixture = (Path(__file__).parent / 'fixtures/opencode-acp.py').resolve()
with tempfile.TemporaryDirectory(prefix='ku-undo-', dir=base) as tmp:
    root = Path(tmp); bins = root / 'bin'; bins.mkdir(); work = root / 'work'; work.mkdir()
    (bins / 'opencode').symlink_to(fixture); (bins / 'python3').symlink_to(sys.executable)
    git = subprocess.run(['sh', '-c', 'command -v git'], capture_output=True, text=True).stdout.strip()
    (bins / 'git').symlink_to(git)
    env = dict(os.environ, KURISU_HOME=tmp, PATH=str(bins), KURISU_OPENCODE_BIN=str(fixture), KURISU_OPENCODE_TIMEOUT_SECS='60')
    for k in ('KURISU_SOCK', 'KURISU_WEB_PORT'): env.pop(k, None)
    def req(op, **fields):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(30); s.connect(str(root / 'kurisu.sock'))
            s.sendall((json.dumps(dict(op=op, **fields)) + '\n').encode())
            r = json.loads(s.makefile().readline())
            if 'error' in r: raise RuntimeError(r['error'])
            return r['ok']
    with (root / 'server.log').open('w') as log:
        p = subprocess.Popen([binary, 'serve'], env=env, stdout=log, stderr=log)
        try:
            for _ in range(200):
                if (root / 'kurisu.sock').exists(): break
                assert p.poll() is None, (root / 'server.log').read_text()
                time.sleep(.05)
            sid = req('create', name='undo-e2e', harness='opencode', model='fixture/free', cwd=str(work))['id']
            (work / 'notes.txt').write_text('original')
            assert req('send', name=sid, text='first turn, kept')['reply'] == 'PONG'
            req('send', name=sid, text='FIXTURE_WRITE please edit notes and add a zebra file')
            assert (work / 'notes.txt').read_text() == 'edited by turn' and (work / 'made-by-turn.txt').exists(), 'turn did not write'
            before = req('history_page', name=sid, tools=True, limit=50)['messages']
            print('history before:', [(m['role'], m['text'][:40]) for m in before])
            out = subprocess.run([binary, 'undo', sid], env=env, capture_output=True, text=True, timeout=60)
            assert out.returncode == 0, out.stderr
            print('ku undo said:\n' + out.stdout.strip())
            assert (work / 'notes.txt').read_text() == 'original', 'notes.txt not restored'
            assert not (work / 'made-by-turn.txt').exists(), 'created file not removed'
            assert not (work / '.git').exists(), 'Kurisu wrote git data into the folder'
            after = req('history_page', name=sid, tools=True, limit=50)['messages']
            print('history after:', [(m['role'], m['text'][:40]) for m in after])
            assert not any('FIXTURE_WRITE' in m['text'] or m['text'] == 'Write notes.txt' for m in after), 'undone rows still in history'
            assert any(m['text'] == 'first turn, kept' for m in after), 'earlier turn lost'
            found = req('history_page', name=sid, search='zebra', limit=50)['messages']
            assert not found, f'search still finds the undone turn: {found}'
            # The next prompt carries the withdrawal note (opencode has no ACP undo).
            assert req('send', name=sid, text='after undo')['reply'] == 'PONG'
            prompts = [json.loads(x) for x in (root / 'fixture-requests.jsonl').read_text().splitlines()]
            last = [q for q in prompts if q.get('method') == 'session/prompt'][-1]
            text = '\n'.join(x.get('text', '') for x in last['params']['prompt'])
            assert '/undo' in text and 'withdrawn' in text, text
            # A second /undo takes back "after undo", which changed no file.
            out2 = subprocess.run([binary, 'undo', sid], env=env, capture_output=True, text=True, timeout=60)
            assert out2.returncode == 0 and 'no file in the session folder had changed' in out2.stdout, out2.stdout + out2.stderr
            print('PASS /undo: files restored, created file removed, history rows gone (search too), next prompt told, stacked undo works')
        finally:
            try: req('shutdown')
            except Exception: pass
            try: p.wait(timeout=5)
            except subprocess.TimeoutExpired: p.kill(); p.wait()
