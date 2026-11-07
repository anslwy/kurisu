#!/usr/bin/env python3
"""Live ACP v1 check over stdio (uses your real claude login; not run in CI).

Usage: acp_stdio_live.py <claude-cli-acp binary> <cwd>
"""
import json, subprocess, sys

proc = subprocess.Popen([sys.argv[1]], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
def call(i, method, params):
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n"); proc.stdin.flush()
    updates = []
    while True:
        msg = json.loads(proc.stdout.readline())
        if msg.get("id") == i:
            return msg, updates
        updates.append(msg)

init, _ = call(1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
print("initialize:", init["result"]["agentCapabilities"])
new, _ = call(2, "session/new", {"cwd": sys.argv[2], "mcpServers": []})
sid = new["result"]["sessionId"]
res, ups = call(3, "session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "Reply with exactly the word PONG."}]})
texts = [u["params"]["update"]["content"]["text"] for u in ups if u.get("method") == "session/update"
         and u["params"]["update"].get("sessionUpdate") == "agent_message_chunk"]
print("stopReason:", res["result"]["stopReason"], "updates:", texts)
proc.stdin.close(); proc.wait(timeout=10)
subprocess.run(["tmux", "-L", "kurisu-claude", "kill-session", "-t", "claude-" + sid[:8]])
assert res["result"]["stopReason"] == "end_turn" and "PONG" in "".join(texts)
print("ACP STDIO OK")
