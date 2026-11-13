# The outward MCP server (`ku mcp serve`)

Bare `ku mcp` is the stdio `kurisu-mcp` server injected into every session. This is
the other direction: a streamable-HTTP MCP endpoint that an app on another device
(Vicuna, an editor, a second agent) calls over Tailscale to see the team and talk to
it. It has the shape of any remote MCP server: a URL plus a token.

```
ku mcp serve [--host IP] [--port N]   run the listener in the foreground
ku mcp token                          create the token if missing and print it
ku mcp url [--host IP] [--port N]     print http://<host>:<port>/mcp/<token>
```

`ku mcp serve` runs in the foreground (Ctrl-C or SIGTERM stops it) and talks to the
running daemon over its unix socket. `--host` must be an explicit IP address: a
hostname, `0.0.0.0` or `::` is refused, so it never listens on every interface by
accident. The default is this box's Tailscale address.

## The Telegram card (`/mcp_server`)

The daemon can host the same endpoint in-process, which is what the owner usually
uses: send `/mcp_server` in the owner's DM for a status card with Enable, Disable and
Show URL. Enable binds the listener inside the daemon on `<KURISU_HOME>/mcp-server.json`'s
`host` and `port` (same address, same token file, so the URL does not change), and a
restart brings it back by itself while the stored switch is on. Disable stops only
that listener and clears the switch. The status line carries the address, the token
file and its mode, the daemon pid and uptime, and the URL with the token masked: the
full URL only appears in the message Show URL sends.

A `ku mcp serve` process you started by hand keeps working and is never killed from
the card: if it already holds the port, Enable reports `port in use by pid N` and
leaves the stored switch off. `ku doctor` has an `MCP SERVER` row: OK when the switch
is on and the daemon really holds the port, OFF when it is off, FAIL when it is on but
nothing listens or a foreign process owns the port.

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
