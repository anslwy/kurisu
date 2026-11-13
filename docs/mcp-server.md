# The outward MCP server (`ku mcp serve`)

Bare `ku mcp` is the stdio `kurisu-mcp` server injected into every session. This is
the other direction: a streamable-HTTP MCP endpoint that an app on another device
(Vicuna, an editor, a second agent) calls over Tailscale to see the team and talk to
it. It has the shape of any remote MCP server: a URL plus a token.

```
ku mcp serve [--host IP] [--port N]   run the listener (default 100.118.224.35:8793)
ku mcp token                          create the token if missing and print it
ku mcp url [--host IP] [--port N]     print http://<host>:<port>/mcp/<token>
```

`ku mcp serve` runs in the foreground (Ctrl-C or SIGTERM stops it) and talks to the
running daemon over its unix socket. `--host` must be an explicit IP address: a
hostname, `0.0.0.0` or `::` is refused, so it never listens on every interface by
accident. The default is this box's Tailscale address.

## Endpoint

```
POST http://<host>:<port>/mcp/<token>    JSON-RPC 2.0, answered as application/json
POST http://<host>:<port>/mcp            same, with Authorization: Bearer <token>
```

The token is `<KURISU_HOME>/mcp-server-token`, mode 0600, created on first use. It
travels in the URL so a client that cannot set headers still works; the bearer
header is accepted too. `Accept: application/json, text/event-stream` is fine; the
answer is always JSON. `GET` answers 405 (no server-to-client SSE stream). On a
loopback bind a non-loopback `Host` header is refused (DNS rebinding).

`initialize`, `tools/list`, `tools/call` and `ping` are implemented; notifications
get `202` with no body. The endpoint is stateless: no `Mcp-Session-Id` is issued.

## Tools

- `sessions`: every session's name, harness, status (plus role, description, last activity).
- `peers`: the same list.
- `send {to, text}`: queue a message, return `{seq}` at once; never waits for the reply.
- `read {session, after?, limit?, wait_secs?}`: messages after a seq, ascending.
  `wait_secs` polls for something new, capped at 60; text per message is capped at 6000 chars.

## Safety

An outward call has no caller session, so `send` is recorded as the owner's own chat
message (like text typed in Telegram) and the session runs a normal turn on it.
Nothing else is reachable: no session create/stop, no lifecycle, harness, model or
MCP-server changes, and no approvals. The tool set is a whitelist in code
(`mcp_out::to_request`).

## Blocking

A real MCP client gives up at 120 s. `send` queues and returns a seq, `read` waits at
most 60 s, and every tool call is cut off at 55 s.
