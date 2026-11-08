#!/usr/bin/env python3
"""Running-server identity through socket, MCP and Telegram, using only /tmp.

python3 scripts/test-version.py SERVER_BINARY CALLER_BINARY
Build the binaries with different KURISU_BUILD_COMMIT values. The caller's
--version must differ from the server's. No real CLIs or Telegram are used.
"""
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time

server_bin, caller_bin = [str(Path(p).resolve()) for p in sys.argv[1:]]
server_local = subprocess.check_output([server_bin, '--version'], text=True)
caller_local = subprocess.check_output([caller_bin, '--version'], text=True)
assert server_local != caller_local, 'use binaries built with different SHAs'

with tempfile.TemporaryDirectory(prefix='ku-version-', dir='/tmp') as tmp:
    root = Path(tmp)
    messages = []
    delivered = threading.Event()

    class Telegram(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            data = json.loads(self.rfile.read(int(self.headers.get('Content-Length', 0))) or '{}')
            method = self.path.rsplit('/', 1)[-1]
            result = True
            if method == 'getUpdates':
                if data.get('offset', -1) >= 0 and not delivered.is_set():
                    delivered.set()
                    result = [{'update_id': 1, 'message': {'message_id': 1, 'date': int(time.time()),
                        'chat': {'id': 42, 'type': 'private'}, 'from': {'id': 42}, 'text': '/version'}}]
                else:
                    time.sleep(.05)
                    result = []
            elif method == 'getMe':
                result = {'id': 7, 'username': 'local_test_bot', 'is_bot': True}
            elif method == 'sendMessage':
                messages.append(data['text'])
                result = {'message_id': len(messages)}
            body = json.dumps({'ok': True, 'result': result}).encode()
            self.send_response(200)
            self.end_headers()
            try:
                self.wfile.write(body)
            except BrokenPipeError:
                pass

        do_GET = do_POST

    api = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Telegram)
    while api.server_port == 8791:
        api.server_close()
        api = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Telegram)
    threading.Thread(target=api.serve_forever, daemon=True).start()
    (root / 'telegram.json').write_text(json.dumps({'token': 'local-fake', 'chat_id': '42'}))
    (root / 'bin').mkdir()
    fake_cli = root / 'bin' / 'codex'
    fake_cli.write_text('#!/bin/sh\nexit 1\n')
    fake_cli.chmod(0o755)
    env = dict(os.environ, KURISU_HOME=tmp, KURISU_SESSION="", PATH=str(root / 'bin'),
               KURISU_TELEGRAM_API=f'http://127.0.0.1:{api.server_port}',
               KURISU_BUILD_COMMIT='runtime-env-must-not-override-embedded-sha')

    def req(op, **kwargs):
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(5)
            sock.connect(str(root / 'kurisu.sock'))
            sock.sendall((json.dumps(dict(op=op, **kwargs)) + '\n').encode())
            response = json.loads(sock.makefile().readline())
            assert 'error' not in response, response
            return response['ok']

    with (root / 'server.log').open('w') as log:
        server = subprocess.Popen([server_bin, 'serve'], env=env, stdout=log, stderr=log)
        try:
            for _ in range(100):
                if (root / 'kurisu.sock').exists():
                    break
                assert server.poll() is None, (root / 'server.log').read_text()
                time.sleep(.05)
            info = req('version', git_sha='caller-spoof')
            assert info['git_sha_short'] in server_local, (info, server_local)
            assert info['git_sha_short'] not in caller_local, (info, caller_local)
            assert info['git_sha'] != env['KURISU_BUILD_COMMIT']
            assert all(info[k] for k in ['version', 'git_sha', 'built', 'profile', 'target', 'started_at'])
            assert info['uptime_seconds'] >= 0
            assert req('ping')['build']['git_sha'] == info['git_sha']
            peer = req('create', name='version-peer', harness='codex')
            peer_env = dict(env, KURISU_SESSION=peer['id'])
            calls = [{'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list'},
                     {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call',
                      'params': {'name': 'version', 'arguments': {'git_sha': 'caller-spoof'}}}]
            result = subprocess.run([caller_bin, 'mcp'], env=peer_env, input=''.join(json.dumps(c)+'\n' for c in calls),
                                    text=True, capture_output=True, timeout=10, check=True)
            listing, reply = [json.loads(line)['result'] for line in result.stdout.splitlines()]
            assert any(t['name'] == 'version' for t in listing['tools'])
            assert not reply['isError'], reply
            mcp = json.loads(reply['content'][0]['text'])
            assert mcp['git_sha'] == info['git_sha'], (mcp, info)
            cli = subprocess.check_output([caller_bin, 'version', '--json'], env=env, text=True)
            assert json.loads(cli)['git_sha'] == info['git_sha']
            for _ in range(200):
                if any(info['git_sha'] in m for m in messages):
                    break
                time.sleep(.05)
            else:
                raise AssertionError(('no Telegram version reply', messages))
            text = next(m for m in messages if info['git_sha'] in m)
            assert len(text.splitlines()) == 4, text
            for field in ['version', 'git_sha_short', 'profile', 'target']:
                assert info[field] in text, (field, text)
            assert 'Built ' in text
            print('PASS: Telegram, MCP, CLI and ping report the running server SHA despite a differently built caller and runtime override')
        finally:
            try:
                req('shutdown')
            except Exception:
                pass
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()
            api.shutdown()
