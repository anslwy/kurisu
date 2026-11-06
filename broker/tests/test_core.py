"""Allowlist, registry, protocol and config invariants."""

from __future__ import annotations

import json
import os
import stat
from pathlib import Path

import pytest

from guest_broker import config as config_mod
from guest_broker import protocol
from guest_broker.allowlist import Allowlist, Identity, load_allowlist, normalize_jid
from guest_broker.errors import ConfigError, ProtocolError, RegistryError, UnauthorizedError
from guest_broker.registry import (
    GuestBinding,
    UserRegistry,
    load_registry,
    provision,
    validate_binding,
)

from .conftest import GUEST_JID, SESSION_ID, make_config, write_allowlist, write_registry


# ---------------------------------------------------------------- allowlist
def test_jid_normalisation_strips_device_suffix():
    assert normalize_jid("15551230001:12@s.whatsapp.net") == "15551230001@s.whatsapp.net"
    assert normalize_jid(" 15551230001@s.whatsapp.net ") == "15551230001@s.whatsapp.net"


@pytest.mark.parametrize(
    "jid",
    ["12345-67890@g.us", "status@broadcast", "abc@newsletter", "not-a-jid", "", "1@s.whatsapp.net-extra"],
)
def test_jid_normalisation_rejects_groups_and_junk(jid):
    with pytest.raises(UnauthorizedError):
        normalize_jid(jid)


def test_empty_allowlist_denies_everything():
    allow = Allowlist(())
    assert len(allow) == 0
    with pytest.raises(UnauthorizedError):
        allow.resolve(GUEST_JID)


def test_allowlist_default_deny_and_exact_match():
    allow = Allowlist((Identity(wa_jid=GUEST_JID, user_id="pilot-001"),))
    assert allow.resolve("15551230001:7@s.whatsapp.net") == "pilot-001"
    with pytest.raises(UnauthorizedError):
        allow.resolve("15551230002@s.whatsapp.net")


def test_allowlist_rejects_two_identities_for_one_user():
    with pytest.raises(UnauthorizedError):
        Allowlist(
            (
                Identity(wa_jid="15551230001@s.whatsapp.net", user_id="pilot-001"),
                Identity(wa_jid="15551230002@s.whatsapp.net", user_id="pilot-001"),
            )
        )


def test_disabled_identity_is_denied():
    allow = Allowlist((Identity(wa_jid=GUEST_JID, user_id="pilot-001", enabled=False),))
    with pytest.raises(UnauthorizedError):
        allow.resolve(GUEST_JID)


def test_allowlist_file_must_be_private(tmp_path: Path):
    path = write_allowlist(
        tmp_path / "allowlist.json",
        [{"wa_jid": GUEST_JID, "user_id": "pilot-001"}],
        mode=0o644,
    )
    with pytest.raises(UnauthorizedError, match="group/world accessible"):
        load_allowlist(path)
    os.chmod(path, 0o600)
    assert len(load_allowlist(path)) == 1


def test_allowlist_file_rejects_unknown_keys(tmp_path: Path):
    path = write_allowlist(
        tmp_path / "allowlist.json",
        [{"wa_jid": GUEST_JID, "user_id": "pilot-001", "session_id": "conv_x"}],
    )
    with pytest.raises(UnauthorizedError, match="unknown key"):
        load_allowlist(path)


# ---------------------------------------------------------------- registry
def test_registry_binds_exactly_one_session(layout, mappings):
    registry = load_registry(mappings["registry"], layout["guests_root"])
    binding = registry.get("pilot-001")
    assert binding.session_id == SESSION_ID
    assert binding.workspace == layout["workspace"]
    assert registry.session_owner(SESSION_ID) == "pilot-001"


def test_registry_refuses_one_session_for_two_users(layout, mappings):
    write_registry(
        mappings["registry"],
        layout["guests_root"],
        users={"pilot-001": SESSION_ID, "pilot-002": SESSION_ID},
    )
    (layout["guests_root"] / "pilot-002" / "workspace").mkdir(parents=True)
    with pytest.raises(RegistryError, match="bound to both"):
        load_registry(mappings["registry"], layout["guests_root"])


def test_registry_refuses_workspace_outside_guest_tree(layout):
    other = layout["root"] / "elsewhere"
    other.mkdir()
    binding = GuestBinding(user_id="pilot-001", session_id=SESSION_ID, workspace=other)
    with pytest.raises(RegistryError, match="must be exactly"):
        validate_binding(binding, layout["guests_root"])


def test_registry_refuses_symlinked_workspace(layout):
    real = layout["root"] / "real-workspace"
    real.mkdir()
    link = layout["guests_root"] / "pilot-001" / "workspace"
    link.rmdir()
    link.symlink_to(real)
    binding = GuestBinding(user_id="pilot-001", session_id=SESSION_ID, workspace=link)
    with pytest.raises(RegistryError, match="symlink"):
        validate_binding(binding, layout["guests_root"])


def test_registry_refuses_invalid_session_id(layout):
    binding = GuestBinding(
        user_id="pilot-001",
        session_id="../../etc/passwd",
        workspace=layout["workspace"],
    )
    with pytest.raises(RegistryError, match="invalid session_id"):
        validate_binding(binding, layout["guests_root"])


def test_registry_file_must_be_private(layout, mappings):
    os.chmod(mappings["registry"], 0o644)
    with pytest.raises(RegistryError, match="group/world accessible"):
        load_registry(mappings["registry"], layout["guests_root"])


def test_provision_is_operator_only_and_refuses_rebind(layout, mappings):
    (layout["guests_root"] / "pilot-002" / "workspace").mkdir(parents=True)
    first = provision(mappings["registry"], layout["guests_root"], "pilot-002", "conv_aaaabbbbccccdddd")
    assert first.session_id == "conv_aaaabbbbccccdddd"
    with pytest.raises(RegistryError, match="refusing to rebind"):
        provision(mappings["registry"], layout["guests_root"], "pilot-002", "conv_eeeeffff00001111")
    # an already-bound user is protected too
    with pytest.raises(RegistryError, match="refusing to rebind"):
        provision(mappings["registry"], layout["guests_root"], "pilot-001", "conv_eeeeffff00001111")
    with pytest.raises(RegistryError, match="create the guest workspace first"):
        provision(mappings["registry"], layout["guests_root"], "pilot-999", "conv_9999888877776666")
    mode = stat.S_IMODE(mappings["registry"].stat().st_mode)
    assert mode == 0o600


def test_registry_rejects_unknown_session_field_in_file(layout, mappings):
    mappings["registry"].write_text(
        "version: 1\nusers:\n  pilot-001:\n    session_id: conv_0123456789abcdef\n"
        f"    workspace: {layout['workspace']}\n    sessions: [conv_other]\n",
        encoding="utf-8",
    )
    os.chmod(mappings["registry"], 0o600)
    with pytest.raises(RegistryError, match="unknown key"):
        load_registry(mappings["registry"], layout["guests_root"])


# ---------------------------------------------------------------- protocol
def test_protocol_rejects_session_selection_fields():
    for field in ("session_id", "user_id", "model", "workspace", "command"):
        frame = {"v": 1, "type": "inbound", "msg_id": "abcdef1", "wa_jid": GUEST_JID, "text": "hi", field: "x"}
        with pytest.raises(ProtocolError, match="forbidden field"):
            protocol.decode_line(json.dumps(frame), max_message_chars=100)


def test_protocol_rejects_unknown_fields():
    frame = {"v": 1, "type": "inbound", "msg_id": "abcdef1", "wa_jid": GUEST_JID, "text": "hi", "extra": 1}
    with pytest.raises(ProtocolError, match="unknown field"):
        protocol.decode_line(json.dumps(frame), max_message_chars=100)


def test_protocol_rejects_oversize_message():
    frame = {"v": 1, "type": "inbound", "msg_id": "abcdef1", "wa_jid": GUEST_JID, "text": "x" * 101}
    with pytest.raises(ProtocolError, match="exceeds limit"):
        protocol.decode_line(json.dumps(frame), max_message_chars=100)


def test_protocol_rejects_huge_frame():
    with pytest.raises(ProtocolError, match="too large"):
        protocol.decode_line(b"x" * (protocol.MAX_FRAME_BYTES + 1), max_message_chars=100)


def test_protocol_roundtrip_reply():
    reply = protocol.Reply(msg_id="abcdef1", wa_jid=GUEST_JID, text="hello")
    decoded = json.loads(protocol.encode(reply))
    assert decoded["type"] == "reply"
    assert decoded["text"] == "hello"
    assert decoded["v"] == protocol.PROTOCOL_VERSION


def test_allowlist_fingerprint_is_order_independent():
    assert protocol.allowlist_fingerprint(["a", "b"]) == protocol.allowlist_fingerprint(["b", "a"])


# ---------------------------------------------------------------- config
def test_config_rejects_placeholders(layout, mappings):
    path = make_config(layout, mappings)
    text = path.read_text(encoding="utf-8").replace("model: deepseek-v4.1-flash:cloud", "model: __REQUIRED__")
    path.write_text(text, encoding="utf-8")
    with pytest.raises(ConfigError, match="placeholder"):
        config_mod.load_config(path)


def test_config_rejects_unknown_keys(layout, mappings):
    path = make_config(layout, mappings)
    path.write_text(path.read_text(encoding="utf-8") + "\nlimits_extra:\n  memory_max: 1G\n", encoding="utf-8")
    with pytest.raises(ConfigError, match="unknown key"):
        config_mod.load_config(path)


def test_config_rejects_model_lists(layout, mappings):
    path = make_config(layout, mappings)
    path.write_text(
        path.read_text(encoding="utf-8").replace("model: deepseek-v4.1-flash:cloud", "model: 'a:cloud,b:cloud'"),
        encoding="utf-8",
    )
    with pytest.raises(ConfigError, match="exactly one model"):
        config_mod.load_config(path)


def test_config_rejects_shared_network(layout, mappings):
    path = make_config(layout, mappings)
    path.write_text(path.read_text(encoding="utf-8").replace("share_net: false", "share_net: true"), encoding="utf-8")
    with pytest.raises(ConfigError, match="share_net"):
        config_mod.load_config(path)


def test_config_rejects_state_inside_guest_tree(layout, mappings):
    path = make_config(layout, mappings)
    inside = layout["workspace"] / "state"
    path.write_text(
        path.read_text(encoding="utf-8").replace(f"state_dir: {layout['operator'] / 'state'}", f"state_dir: {inside}"),
        encoding="utf-8",
    )
    with pytest.raises(ConfigError, match="guest-writable"):
        config_mod.load_config(path)


def test_config_rejects_missing_limit_key(layout, mappings):
    path = make_config(layout, mappings)
    path.write_text(path.read_text(encoding="utf-8").replace("  io_weight: 100\n", ""), encoding="utf-8")
    with pytest.raises(ConfigError, match="missing required key"):
        config_mod.load_config(path)


def test_config_expands_env_vars(layout, mappings, monkeypatch):
    path = make_config(layout, mappings)
    path.write_text(
        path.read_text(encoding="utf-8").replace(
            "token_env: OMNIGENT_OPERATOR_TOKEN", "token_env: OMNIGENT_OPERATOR_TOKEN"
        ).replace('base_url: ""', 'base_url: "${PILOT_BASE_URL}"'),
        encoding="utf-8",
    )
    monkeypatch.setenv("PILOT_BASE_URL", "http://127.0.0.1:9999")
    cfg = config_mod.load_config(path)
    assert cfg.runtime.omnigent_base_url == "http://127.0.0.1:9999"
