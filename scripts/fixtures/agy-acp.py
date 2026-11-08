#!/usr/bin/env python3
"""ACP-only fixture. Keeps running until stdin closes; no provider or credentials."""
import json
import os
import sys
import time

attempts = 0
pending_prompt = None
session_id = "agy-conversation-123"

def emit(value):
    print(json.dumps({"jsonrpc": "2.0", **value}), flush=True)

def update(value):
    emit({"method": "session/update", "params": {"sessionId": session_id, "update": value}})

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    params = request.get("params", {})
    rid = request.get("id")
    if not method:
        assert request["result"]["outcome"]["optionId"] == "once"
        emit({"id": pending_prompt, "result": {"stopReason": "end_turn"}})
        continue
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}}
    elif method in ("session/new", "session/load"):
        attempts += 1
        if attempts <= int(os.environ.get("AGY_FIXTURE_AUTH_ATTEMPTS", "2")):
            emit({"id": rid, "error": {"code": -32000, "message": "Authentication required"}})
            continue
        if method == "session/load":
            assert params["sessionId"] == session_id
        result = {"sessionId": session_id, "configOptions": [], "attempts": attempts}
    elif method == "session/set_config_option":
        assert params["configId"] in ("model", "thought_level")
        if params["configId"] == "model":
            assert params["value"] == "gemini-3.8-flash"
        else:
            assert params["value"] in ("high", "medium", "low")
        result = {}
    elif method == "session/set_mode":
        assert params["modeId"] in ("default", "accept-edits", "plan")
        result = {}
    elif method == "session/prompt":
        assert params["sessionId"] == session_id
        if params["prompt"][-1].get("text") == "permission":
            pending_prompt = rid
            emit({"id": "approval-1", "method": "session/request_permission", "params": {
                "sessionId": session_id, "toolCall": {"toolCallId": "tool-permission", "title": "Read fixture"},
                "options": [{"optionId": "once", "name": "Allow", "kind": "allow_once"}]}})
            continue
        time.sleep(0.05)
        for kind in ("agent_thought_chunk", "agent_message_chunk"):
            update({"sessionUpdate": kind, "content": {"type": "text", "text": "PONG"}})
        update({"sessionUpdate": "tool_call", "toolCallId": "tool-1", "title": "Read fixture", "kind": "read", "status": "in_progress"})
        update({"sessionUpdate": "tool_call_update", "toolCallId": "tool-1", "status": "completed"})
        result = {"stopReason": "end_turn", "usage": {"input_tokens": 10, "output_tokens": 2, "cost_usd": 0.0}}
    elif method == "session/close":
        result = {}
    elif method == "session/cancel":
        continue
    else:
        raise AssertionError(method)
    emit({"id": rid, "result": result})
