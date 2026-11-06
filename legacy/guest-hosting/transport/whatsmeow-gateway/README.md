# whatsmeow gateway (Kurisu transport)

Isolated WhatsApp transport for a **secondary** account. It relays allowlisted
one-to-one messages to the Kurisu broker and sends back only the broker's reply.

## What it does not do

- no pairing during `run` (pairing needs the explicit `pair` subcommand),
- no group chats, no self-chats, no broadcasts, no newsletters,
- no session ids, model names, workspaces or commands on the wire (the protocol
  rejects those fields as a hard error),
- no Omnigent credentials, no broker state, no Google tokens.

## Commands

```bash
go build -o bin/whatsmeow-gateway ./cmd/whatsmeow-gateway

./bin/whatsmeow-gateway status --config ../../config/gateway.local.json
./bin/whatsmeow-gateway pair   --config ../../config/gateway.local.json --confirm-secondary
./bin/whatsmeow-gateway run    --config ../../config/gateway.local.json
./bin/whatsmeow-gateway run    --config ../../config/gateway.local.json --dry-run
```

`pair` renders the QR code in the terminal only (never to a file). `--dry-run`
performs local policy checks on the allowlisted identity and never opens the
device store or the broker socket.

## Layout

```
cmd/whatsmeow-gateway/main.go   whatsmeow wiring: store, QR, events, send
internal/policy/                allowlist: default deny, JID normalisation, fingerprint
internal/protocol/              NDJSON frames, strict decode, forbidden-field rejection
internal/gateway/               broker client + local policy + reply routing
internal/config/                JSON config, placeholders rejected, 0700 state dir
```

## Tests

```bash
go test ./...
```

Covered: JID normalisation, default deny, duplicate/disabled identities,
allowlist fingerprint parity with the Python broker, forbidden fields, unknown
fields, wrong protocol version, oversize frames, handshake rejection, account
pinning, group/self/empty/overlong drops, "chat must be the sender's own chat",
denial handling, reply truncation, config placeholder/permission rejection.

## Credentials

The device store lives in `state_dir` and contains the paired secondary account's
keys - never copy it into the guest, the broker, or git.

Permissions are enforced by the binary, not left to the operator: it applies a
`0077` umask before opening the database (so the DB and SQLite's `-wal`/`-shm`/
`-journal` companions are created `0600`) and re-`chmod`s the state directory to
`0700` plus every existing store file to `0600` on each `run`/`pair`/`status`.
That repairs a store created by an older build. It never reads or prints the
contents.
