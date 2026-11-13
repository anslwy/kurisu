#!/usr/bin/env python3
"""Telegram /mcp_server against a real daemon, a loopback Telegram mock and a throwaway home.

Proves, with no Telegram traffic and no real port: the card text and its three
buttons, Enable actually serving MCP over HTTP, Disable closing the port, a
restart re-arming the listener with no tap, and a port held by someone else
reported (never killed) with the stored switch put back to off.
"""
import http.server, json, os, socket, subprocess, sys, tempfile, threading, time, urllib.request
from pathlib import Path

binary = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='ku-mcp-server-', dir='/tmp') as tmp:
    root = Path(tmp)
    # Telegram semantics, not a queue: an update stays available until a poller
    # confirms it with a higher offset, so a dying poller cannot swallow one.
    updates = []
    messages = []
    answers = []
    uid = 0

    class Telegram(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_): pass
        def do_POST(self):
            data = json.loads(self.rfile.read(int(self.headers.get('Content-Length', 0))) or '{}')
            method = self.path.rsplit('/', 1)[-1]
            result = True
            if method == 'getUpdates':
                offset = data.get('offset', -1)
                result = [u for u in updates if u['update_id'] >= offset][:1] if offset >= 0 else []
                if not result: time.sleep(.05)
            elif method == 'getMe':
                result = {'id': 7, 'username': 'local_test_bot', 'is_bot': True}
            elif method in ('sendMessage', 'editMessageText'):
                messages.append(dict(data, method=method))
                result = {'message_id': len(messages), 'chat': {'id': 42}, 'text': data.get('text', '')}
            elif method == 'answerCallbackQuery':
                answers.append(data)
            body = json.dumps({'ok': True, 'result': result}).encode()
            self.send_response(200); self.end_headers()
            try: self.wfile.write(body)
            except BrokenPipeError: pass

    api = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Telegram)
    threading.Thread(target=api.serve_forever, daemon=True).start()

    (root / 'telegram.json').write_text(json.dumps({'token': 'local-fake', 'chat_id': '42'}))
    # A free port for the outward listener, and a switch that starts off.
    probe = socket.socket(); probe.bind(('127.0.0.1', 0)); port = probe.getsockname()[1]; probe.close()
    config = root / 'mcp-server.json'
    switch = lambda on: config.write_text(json.dumps({'enabled': on, 'host': '127.0.0.1', 'port': port}))
    switch(False)
    env = dict(os.environ, KURISU_HOME=str(root), KURISU_TELEGRAM_API=f'http://127.0.0.1:{api.server_port}')

    daemon = None
    def start_daemon(label):
        """Start the daemon and wait until its Telegram bridge is polling, so an
        update queued next cannot be swallowed by a previous, dying poller."""
        global daemon
        log = root / f'server-{label}.log'
        daemon = subprocess.Popen([binary, 'serve'], env=env, stdout=open(log, 'w'), stderr=subprocess.STDOUT)
        until(lambda: 'bridge started' in log.read_text() and daemon.poll() is None, f'{label}: daemon + telegram')

    def req(op, **fields):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(15); s.connect(str(root / 'kurisu.sock'))
            s.sendall((json.dumps(dict(op=op, **fields)) + '\n').encode())
            r = json.loads(s.makefile().readline())
        assert 'error' not in r, r
        return r['ok']

    def until(pred, what):
        for _ in range(600):
            if pred(): return
            if daemon is not None: assert daemon.poll() is None, f'{what}: daemon exited'
            time.sleep(.025)
        raise AssertionError((what, messages[-6:], [p.read_text()[-1500:] for p in root.glob('server-*.log')]))

    def put(text=None, callback=None):
        global uid
        uid += 1
        if callback:
            u = {'callback_query': {'id': str(uid), 'from': {'id': 42},
                                    'message': {'message_id': uid, 'chat': {'id': 42, 'type': 'private'}}, 'data': callback}}
        else:
            u = {'message': {'message_id': uid, 'date': int(time.time()), 'chat': {'id': 42, 'type': 'private'},
                             'from': {'id': 42}, 'text': text}}
        updates.append(dict(update_id=uid, **u))

    def command(text, expected):
        n = len(messages); put(text)
        until(lambda: any(expected.lower() in json.dumps(m).lower() for m in messages[n:]), text)
        return messages[-1]

    def click(data, expected):
        n = len(messages); put(callback=data)
        until(lambda: any(expected.lower() in json.dumps(m).lower() for m in messages[n:]), data)
        return messages[-1]

    def buttons(msg):
        return [b for row in msg.get('reply_markup', {}).get('inline_keyboard', []) for b in row]

    def state(msg):
        return next((line for line in msg['text'].splitlines() if line.startswith('state:')), '')

    def holds_port(p):
        with socket.socket() as s:
            s.settimeout(1)
            return s.connect_ex(('127.0.0.1', p)) == 0

    def restart_stop():
        req('shutdown')
        daemon.wait(timeout=20)
        until(lambda: not holds_port(port), 'shutdown released the port')

    def restart(label):
        restart_stop()
        start_daemon(label)

    def rpc(url, method, body):
        request = urllib.request.Request(url, data=json.dumps(body).encode(), headers={'content-type': 'application/json'})
        with urllib.request.urlopen(request, timeout=5) as r:
            return r.status, json.loads(r.read())

    def doctor():
        out = subprocess.run([binary, 'doctor'], env=env, capture_output=True, text=True)
        return [line for line in (out.stdout + out.stderr).splitlines() if line.startswith('MCP SERVER')]

    try:
        start_daemon('a')
        card = command('/mcp_server', 'outward mcp server')
        text = card['text']
        assert 'state: stopped' in text and f'127.0.0.1:{port}' in text, text
        labels = [b['callback_data'] for b in buttons(card)]
        assert labels == ['msv:on', 'msv:off', 'msv:url'], card.get('reply_markup')
        print('card:', repr(state(card)), '| buttons:', labels)
        print('doctor (off):', doctor())

        # Enable: the daemon binds the port and serves MCP there.
        running = click('msv:on', 'state: running')
        until(lambda: holds_port(port), 'listener up')
        url = req('mcp_server_url')['url']
        status, init = rpc(url, 'initialize', {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize',
                                              'params': {'protocolVersion': '2025-06-18', 'capabilities': {},
                                                         'clientInfo': {'name': 'acceptance', 'version': '1'}}})
        assert status == 200 and init['result']['serverInfo']['name'] == 'kurisu-mcp', init
        status, listed = rpc(url, 'tools/list', {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list', 'params': {}})
        tools = [t['name'] for t in listed['result']['tools']]
        assert status == 200 and len(tools) == 4, (status, tools)
        print('enable:', repr(state(running)), '| http', status, 'tools', tools)
        assert any('Enabled' in a.get('text', '') for a in answers), answers
        assert req('mcp_server_status')['token_mode'] == '0600'
        assert url.rsplit('/', 1)[1] not in running['text'], 'the card must never carry the whole token'
        print('doctor (running):', doctor())

        # Show URL: its own message, the only place the full token appears.
        shown = click('msv:url', url)
        assert url in shown['text'] and 'keep it secret' in shown['text'], shown['text']

        # Disable: stored off, port closed, and a restart does not bring it back.
        stopped = click('msv:off', 'state: stopped')
        until(lambda: not holds_port(port), 'listener down')
        assert json.loads(config.read_text())['enabled'] is False
        print('disable:', repr(state(stopped)), '| port closed')
        print('doctor (off):', doctor())
        restart('b')
        assert not holds_port(port), 'a disabled listener must not come back'
        card = command('/mcp_server', 'state: stopped')
        print('restart while off:', repr(state(card)), '| port stays closed')

        # A restart with the switch on re-arms it with no tap.
        click('msv:on', 'state: running')
        restart('c')
        # No tap: the listener is back by itself and really answers MCP.
        status, again = rpc(url, 'initialize', {'jsonrpc': '2.0', 'id': 3, 'method': 'initialize',
                                               'params': {'protocolVersion': '2025-06-18', 'capabilities': {},
                                                          'clientInfo': {'name': 'acceptance', 'version': '1'}}})
        assert status == 200 and again['result']['serverInfo']['name'] == 'kurisu-mcp', again
        card = command('/mcp_server', 'state: running')
        print('restart while on:', repr(state(card)), '| http', status)
        print('doctor (after restart):', doctor())

        # Someone else's listener on the port: report it, kill nothing, switch back to off.
        restart_stop()
        foreign = socket.socket(); foreign.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        foreign.bind(('127.0.0.1', port)); foreign.listen(4)
        switch(True)
        blocked = doctor()
        assert blocked and 'FAIL' in blocked[0] and 'held by pid' in blocked[0], blocked
        print('doctor (foreign holder):', blocked)
        start_daemon('d')
        card = command('/mcp_server', 'port in use')
        assert 'state: stopped' in card['text'] and json.loads(config.read_text())['enabled'] is False, card['text']
        assert foreign.fileno() >= 0 and holds_port(port), 'the foreign listener must be untouched'
        print('foreign port:', repr(state(card)), repr(next(l for l in card['text'].splitlines() if l.startswith('note:'))))
        foreign.close()
        print('PASS /mcp_server card, enable, disable, show url, restart re-arm, foreign port refused')
    finally:
        if daemon is not None and daemon.poll() is None:
            daemon.terminate()
            try: daemon.wait(timeout=10)
            except subprocess.TimeoutExpired: daemon.kill()
        api.shutdown()
