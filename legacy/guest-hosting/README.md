# Legacy: guest-hosting pilot (frozen)

This tree is the original Kurisu design: an operator hosts isolated
Omnigent sessions for WhatsApp guests. It is frozen at tag `pilot-freeze` and
kept for reference and reuse. Its tests still run:

```bash
cd legacy/guest-hosting/broker && .venv/bin/python -m pytest -q -p no:cacheprovider
cd legacy/guest-hosting/transport/whatsmeow-gateway && go test ./... && go vet ./...
```

Why it is frozen: see `docs/REMAINING-STEPS.md` (freeze notice) and the
repository-level `docs/terms-check.md`. One operator account serving other
people conflicts with Claude subscription terms unless each guest brings their
own credential.

Parts that the self-host core reuses or ports: the whatsmeow gateway (optional
WhatsApp channel), Google device-flow sign-in, the Podman sandbox plans with the
widening guard, the model proxy, and the restart-identity pattern
(`broker/guest_broker/instance.py`).

Live runtime state (`runtime/` and `config/*.local.*` at the repository root) was
not moved and is not touched by the pivot.

The original pilot README is preserved as `README-pilot.md` (paths in it are relative to this folder).
