#!/usr/bin/env python3
"""Exercise /usage and MCP against fake CLIs and a local Telegram API.

Run after cargo build --bin ku: python3 scripts/test-usage.py target/debug/ku
Uses only fresh /tmp homes, a Unix socket and an ephemeral mock HTTP port.
No credentials, real CLIs, model calls, or production ports are used.
"""
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time


BINARY = str(Path(sys.argv[1]).resolve())


def check_case(claude, codex, mode="ok"):
    with tempfile.TemporaryDirectory(prefix="ku-usage-", dir="/tmp") as tmp:
        root = Path(tmp)
        bins = root / "bin"
        bins.mkdir()
        messages = []
        delivered = threading.Event()

        class Telegram(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                data = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or "{}")
                method = self.path.rsplit("/", 1)[-1]
                result = True
                if method == "getUpdates":
                    if data.get("offset", -1) >= 0 and not delivered.is_set():
                        delivered.set()
                        result = [{"update_id": 1, "message": {"message_id": 1, "date": int(time.time()),
                                  "chat": {"id": 42, "type": "private"}, "from": {"id": 42}, "text": "/usage"}}]
                    else:
                        time.sleep(0.05)
                        result = []
                elif method == "getMe":
                    result = {"id": 7, "username": "local_test_bot", "is_bot": True}
                elif method == "sendMessage":
                    messages.append(data["text"])
                    result = {"message_id": len(messages)}
                body = json.dumps({"ok": True, "result": result}).encode()
                self.send_response(200)
                self.end_headers()
                try:
                    self.wfile.write(body)
                except BrokenPipeError:
                    pass  # The test server cancels its long poll during teardown.

            do_GET = do_POST

        api = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Telegram)
        assert api.server_port != 8791
        threading.Thread(target=api.serve_forever, daemon=True).start()
        (root / "telegram.json").write_text(json.dumps({"token": "local-fake", "chat_id": "42"}))
        for name, installed in [("claude", claude), ("codex", codex), ("claude-monitor", True)]:
            if not installed:
                continue
            exe = bins / name
            if name == "codex":
                exe.write_text(f'''#!{sys.executable}
import json,sys,time
for line in sys.stdin:
    request=json.loads(line)
    if "id" not in request: continue
    if request["method"] == "initialize": result={{"id":request["id"],"result":{{}}}}
    elif {mode!r} == "error": result={{"id":request["id"],"error":{{"code":-32000,"message":"not authenticated"}}}}
    elif {mode!r} == "malformed": result={{"id":request["id"],"result":{{"rateLimits":{{"primary":{{"usedPercent":"bad"}}}}}}}}
    elif {mode!r} == "timeout": time.sleep(30); continue
    else: result={{"id":request["id"],"result":{{"rateLimits":{{"primary":{{"usedPercent":4,"windowDurationMins":300}}}}}}}}
    print(json.dumps(result),flush=True)
''')
            elif name == "claude-monitor":
                exe.write_text('#!/bin/sh\nprintf \'%s\\n\' \'{"local":{"cost_usd":1.5}}\'\n')
            else:
                exe.write_text("#!/bin/sh\nexit 1\n")
            exe.chmod(0o755)
        env = dict(os.environ, KURISU_HOME=tmp, PATH=str(bins), KURISU_TELEGRAM_API=f"http://127.0.0.1:{api.server_port}")
        with (root / "server.log").open("w") as log:
            server = subprocess.Popen([BINARY, "serve"], env=env, stdout=log, stderr=log)
            try:
                for _ in range(100):
                    if (root / "kurisu.sock").exists(): break
                    assert server.poll() is None, "test server exited"
                    time.sleep(0.05)
                request = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "usage", "arguments": {}}}
                completed = subprocess.run([BINARY, "mcp"], input=json.dumps(request)+"\n", text=True,
                                           capture_output=True, env=env, timeout=20, check=True)
                mcp = json.loads(completed.stdout)["result"]
                assert not mcp["isError"], mcp
                report = json.loads(mcp["content"][0]["text"])
                expected = {name for name, installed in [("claude", claude), ("codex", codex)] if installed}
                assert set(report["harnesses"]) == expected, report
                assert (report["local_estimate"] is not None) == claude, report
                for _ in range(300):
                    if any(text == report["text"] for text in messages): break
                    time.sleep(0.05)
                else: raise AssertionError(("Telegram and MCP differ", report, messages, (root/"server.log").read_text()))
                if codex:
                    assert report["harnesses"]["codex"]["confidence"] == ("official" if mode == "ok" else "unavailable"), report
                    assert ("4% used" in report["text"]) == (mode == "ok"), report
                print(f"PASS claude={claude} codex={codex} mode={mode}: Telegram == MCP")
            finally:
                server.terminate()
                server.wait(timeout=10)
                api.shutdown()
                api.server_close()


for claude, codex in [(False, False), (True, False), (False, True), (True, True)]:
    check_case(claude, codex)
for mode in ["error", "malformed", "timeout"]:
    check_case(True, True, mode)
