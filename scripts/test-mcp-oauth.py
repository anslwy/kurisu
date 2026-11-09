#!/usr/bin/env python3
"""P3 acceptance against a built ku, a loopback MCP/AS and a fake Claude TUI.

No provider, owner credentials, production home or production ports. All state,
including the private tmux socket, lives under the required --tmpdir.
"""
import argparse
import contextlib
import http.server
import json
import os
from pathlib import Path
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse

FIXTURE = r'''
import json, pathlib, shlex, sys
if '--version' in sys.argv:
    print('claude P3 fixture'); sys.exit()
if '--help' in sys.argv:
    print('--permission-mode choices: "default", "auto", "plan"'); sys.exit()
def arg(name): return sys.argv[sys.argv.index(name) + 1]
settings = json.loads(pathlib.Path(arg('--settings')).read_text())
events = pathlib.Path(shlex.split(settings['hooks']['SessionStart'][0]['hooks'][0]['command'])[-1])
config = json.loads(pathlib.Path(arg('--mcp-config')).read_text())
pathlib.Path(arg('--debug-file')).write_text(''.join('MCP server "' + n + '": Successfully connected\n' for n in config['mcpServers']))
def emit(name, **fields):
    with events.open('a') as f:
        f.write(json.dumps(dict(hook_event_name=name, **fields)) + '\n')
emit('SessionStart')
prompt = events.with_name('prompt.txt')
seen = None
for line in sys.stdin:
    if not prompt.exists(): continue
    stamp = prompt.stat().st_mtime_ns
    if stamp == seen: continue
    seen = stamp
    emit('UserPromptSubmit')
    emit('Stop', last_assistant_message='PONG')
'''


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_): pass

    def reply(self, status, value):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        if status == 401:
            self.send_header('WWW-Authenticate', f'Bearer resource_metadata="{self.server.base}/.well-known/oauth-protected-resource"')
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        s = self.server
        if self.path == '/mcp':
            auth = self.headers.get('Authorization', '')
            s.probes.append(auth)
            self.reply(200 if auth == s.accept and not s.reject else 401, {})
        elif self.path == '/.well-known/oauth-protected-resource':
            self.reply(200, dict(resource=s.base + '/mcp', authorization_servers=[s.base]))
        elif self.path == '/.well-known/oauth-authorization-server':
            self.reply(200, dict(issuer=s.base, authorization_endpoint=s.base + '/authorize',
                token_endpoint=s.base + '/token', registration_endpoint=s.base + '/register',
                code_challenge_methods_supported=['S256']))
        else: self.reply(404, {})

    def do_POST(self):
        s = self.server
        raw = self.rfile.read(int(self.headers.get('Content-Length', 0))).decode()
        if self.path == '/register':
            self.reply(201, dict(client_id='p3-client')); return
        assert self.path == '/token'
        assert self.headers['Content-Type'] == 'application/x-www-form-urlencoded'
        form = urllib.parse.parse_qs(raw)
        if form['grant_type'] == ['authorization_code']:
            self.reply(200, dict(access_token='p3-old-access', refresh_token='p3-old-refresh', expires_in=3600)); return
        assert form['grant_type'] == ['refresh_token']
        assert form['client_id'] == ['p3-client']
        assert form['resource'] == [s.base + '/mcp']
        s.forms.append(form)
        if s.invalid:
            self.reply(400, dict(error='invalid_grant', error_description='p3-old-refresh p3-client-secret')); return
        s.generation += 1
        s.accept = f'Bearer p3-access-{s.generation}'
        self.reply(200, dict(access_token=f'p3-access-{s.generation}',
            refresh_token=f'p3-refresh-{s.generation}', expires_in=7200, token_type='Bearer'))


@contextlib.contextmanager
def instance(binary, tmpdir):
    with tempfile.TemporaryDirectory(prefix='p3-oauth-', dir=tmpdir) as tmp:
        root = Path(tmp)
        (root / 'bin').mkdir()
        fake = root / 'bin' / 'claude'
        fake.write_text(f'#!{sys.executable}\n' + FIXTURE)
        fake.chmod(0o755)
        aserver = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Provider)
        assert aserver.server_port not in (8791, 8798)
        aserver.base = f'http://127.0.0.1:{aserver.server_port}'
        aserver.probes, aserver.forms = [], []
        aserver.generation, aserver.invalid, aserver.reject, aserver.accept = 0, False, False, 'not-authorized'
        threading.Thread(target=aserver.serve_forever, daemon=True).start()
        env = dict(os.environ, KURISU_HOME=tmp, TMPDIR=tmpdir, TMUX_TMPDIR=tmpdir,
            KURISU_CLAUDE_BIN=str(fake), KURISU_DEFAULT_HARNESS='claude', KURISU_NO_RESUME='1')
        with (root / 'server.log').open('w') as log:
            daemon = subprocess.Popen([str(binary), 'serve'], env=env, stdout=log, stderr=log)
            def req(op, **fields):
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(20)
                    client.connect(str(root / 'kurisu.sock'))
                    client.sendall((json.dumps(dict(op=op, **fields)) + '\n').encode())
                    result = json.loads(client.makefile().readline())
                    assert 'error' not in result, result
                    return result['ok']
            try:
                deadline = time.monotonic() + 10
                while not (root / 'kurisu.sock').exists():
                    assert daemon.poll() is None, (root / 'server.log').read_text()
                    assert time.monotonic() < deadline, 'isolated daemon startup timeout'
                    time.sleep(.02)
                sid = req('create', name='probe', harness='claude', cwd=str(root / 'work'))['id']
                yield root, env, req, sid, aserver
            finally:
                if daemon.poll() is None:
                    try: req('shutdown')
                    except (OSError, AssertionError, ValueError): daemon.terminate()
                try: daemon.wait(timeout=10)
                except subprocess.TimeoutExpired: daemon.kill(); daemon.wait()
                aserver.shutdown()
                aserver.server_close()


def seed(root, req, sid, provider):
    req('mcp_set', name=sid, server='remote', config=dict(type='http', url=provider.base + '/mcp'))
    path = root / 'sessions' / sid / 'mcp-oauth' / 'remote.json'
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    path.write_text(json.dumps(dict(access_token='p3-old-access', refresh_token='p3-old-refresh',
        client_id='p3-client', client_secret='p3-client-secret', token_endpoint=provider.base + '/token',
        expires_at=int(time.time()) - 10)))
    path.chmod(0o600)
    return path


def turn(root, req, sid):
    result = req('send', name=sid, text='Reply PONG')
    assert 'PONG' in result['reply'], result
    assert req('status', name=sid)['status'] == 'idle'
    return json.loads((root / 'sessions' / sid / 'mcp.json').read_text())['mcpServers']


def oauth_refresh_on_401_retries_once_and_rotates_token(binary, tmpdir):
    with instance(binary, tmpdir) as (root, _, req, sid, provider):
        path = seed(root, req, sid, provider)
        servers = turn(root, req, sid)
        assert servers['remote']['headers']['Authorization'] == 'Bearer p3-access-1'
        assert json.loads(path.read_text())['refresh_token'] == 'p3-refresh-1'
        assert stat.S_IMODE(path.stat().st_mode) == 0o600
        assert stat.S_IMODE(path.parent.stat().st_mode) == 0o700
        assert len(provider.forms) == 1 and provider.probes == ['Bearer p3-access-1']
        provider.probes.clear(); provider.forms.clear(); provider.accept = 'force-401'
        servers = turn(root, req, sid)
        assert provider.probes == ['Bearer p3-access-1', 'Bearer p3-access-2']
        assert len(provider.forms) == 1
        assert servers['remote']['headers']['Authorization'] == 'Bearer p3-access-2'
        provider.probes.clear(); provider.forms.clear(); provider.reject = True
        servers = turn(root, req, sid)
        assert len(provider.probes) == 2 and len(provider.forms) == 1
        assert 'remote' not in servers
        assert req('mcp_list', name=sid)['servers'][0]['auth_state'] == 'needs-auth'


def oauth_refresh_failure_transitions_to_needs_auth_without_crash(binary, tmpdir):
    with instance(binary, tmpdir) as (root, _, req, sid, provider):
        seed(root, req, sid, provider)
        provider.invalid = True
        servers = turn(root, req, sid)
        assert 'remote' not in servers
        assert req('mcp_list', name=sid)['servers'][0]['auth_state'] == 'needs-auth'
        assert len(provider.forms) == 1


def oauth_tokens_and_headers_redacted_everywhere(binary, tmpdir):
    with instance(binary, tmpdir) as (root, env, req, sid, provider):
        seed(root, req, sid, provider)
        turn(root, req, sid)
        req('mcp_set', name=sid, server='fixed', config=dict(type='http', url=provider.base + '/mcp',
            headers=dict(Authorization='Bearer p3-fixed-secret', **{'X-Secret': 'p3-header-secret'})))
        listed = json.dumps(req('mcp_list', name=sid))
        cli = subprocess.run([str(binary), 'mcp', sid], env=env, capture_output=True, text=True, check=True)
        public = listed + cli.stdout + cli.stderr + (root / 'server.log').read_text()
        for secret in ['p3-old-access', 'p3-old-refresh', 'p3-access-1', 'p3-refresh-1',
                       'p3-client-secret', 'p3-fixed-secret', 'p3-header-secret']:
            assert secret not in public, 'credential reached MCP status, CLI output or logs'


def oauth_e2e_fake_as_in_process(binary, tmpdir):
    with instance(binary, tmpdir) as (root, _, req, sid, provider):
        connected = req('mcp_connect', session=sid, name='remote', url=provider.base + '/mcp',
            redirect_uri=provider.base + '/callback')
        callback = req('mcp_callback', caller=sid,
            url=provider.base + '/callback?' + urllib.parse.urlencode(dict(code='p3-code', state=connected['state'])))
        assert callback['auth_state'] == 'authorized'
        path = root / 'sessions' / sid / 'mcp-oauth' / 'remote.json'
        assert stat.S_IMODE(path.stat().st_mode) == 0o600
        servers = turn(root, req, sid)
        assert servers['remote']['headers']['Authorization'] == 'Bearer p3-access-1'
        assert req('mcp_list', name=sid)['servers'][0]['auth_state'] == 'authorized'
        req('mcp_remove', name=sid, server='remote')
        assert not path.exists()
        assert not (path.parent / 'pending-remote.json').exists()
        assert 'p3-code' not in (root / 'server.log').read_text()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('--tmpdir', required=True, type=Path)
    parser.add_argument('--case', choices=('all', 'refresh', 'invalid-grant', 'redaction', 'e2e'), default='all')
    args = parser.parse_args()
    cases = dict(refresh=oauth_refresh_on_401_retries_once_and_rotates_token,
        **{'invalid-grant': oauth_refresh_failure_transitions_to_needs_auth_without_crash},
        redaction=oauth_tokens_and_headers_redacted_everywhere, e2e=oauth_e2e_fake_as_in_process)
    for name, check in cases.items():
        if args.case in ('all', name):
            check(args.binary.resolve(), str(args.tmpdir.resolve()))
            print('PASS ' + check.__name__, flush=True)
