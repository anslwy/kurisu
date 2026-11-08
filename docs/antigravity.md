# Antigravity CLI harness

Install and log in with Antigravity itself:

```sh
curl -fsSL https://antigravity.google/cli/install.sh | bash
agy acp --login
ku new agy-worker --harness agy --model gemini-3.8-flash
ku send agy-worker "Inspect this workspace"
```

Kurisu starts `agy acp --disable-telemetry` over stdio. It never reads or stores
credentials. `KURISU_AGY_BIN` overrides the executable for tests or custom installs.
OAuth startup may briefly report `Authentication required`; session creation and
load retry for up to 15 seconds before showing the login command.

`ku models agy` lists supported IDs. The default is `gemini-3.8-flash` with high
thinking. Its `-high`, `-medium`, and `-low` aliases select `thought_level`; an
explicit Kurisu effort overrides the suffix. Other supported IDs are
`claude-sonnet-4-6`, `claude-opus-4-6-thinking`, and `gpt-oss-120b-medium`.

ACP session IDs persist in Kurisu's store for continuation. Message, thought,
and tool updates use the normal stream; permission requests use normal approval
cards. Modes map to ACP `default`, `accept-edits`, and `plan`; Kurisu's `auto`
uses `default` upstream permissions. Unsupported bypass mode is rejected.

CLI-reported usage and USD costs are retained when ACP supplies them. Missing
usage or prices remain unavailable; Kurisu does not infer provider prices.

Tests use a local JSON-lines fixture. The live ACP handshake test skips if `agy`
is missing or not logged in, and consumes no prompt quota.

For an explicit provider round trip and continuation check, run
`ku smoke agy /absolute/workspace`. This sends two short prompts and uses the
CLI's own quota.
