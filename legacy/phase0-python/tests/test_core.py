"""Phase 0 core: config/roster schemas, router policy, approvals, state."""

from __future__ import annotations

import os
from pathlib import Path

import pytest
import yaml

from kurisu import backup, channels, sandbox
from kurisu.approvals import ApprovalBook, needs_approval
from kurisu.cli import main as cli_main
from kurisu.config import load_config, parse_config
from kurisu.errors import ConfigError, KurisuError
from kurisu.memory import SessionRegistry, StateStore
from kurisu.roster import parse_roster
from kurisu.router import Router

REPO = Path(__file__).resolve().parents[1]


def _cfg(**over):
    data = {
        "version": 1,
        "runtime": {"server_url": "http://127.0.0.1:6767"},
        "channels": {"telegram": {"enabled": True, "token_env": "KURISU_TELEGRAM_TOKEN", "owner_id": "1000"}},
    }
    data.update(over)
    return data


# --------------------------------------------------------------- config
def test_minimal_config_and_xdg_default(tmp_path):
    cfg = parse_config(_cfg(), base_dir=tmp_path, env={"HOME": "/h"})
    assert cfg.data_dir == Path("/h/.local/share/kurisu")
    assert cfg.roster_path == tmp_path / "roster.yaml"
    assert cfg.channel("telegram").owner_id == "1000"
    assert cfg.default_sandbox == "none" and cfg.budget.daily_usd == 0


@pytest.mark.parametrize("data,match", [
    ({"version": 2}, "version"),
    ({"surprise": 1}, "unknown keys"),
    ({"runtime": {"server_url": "http://0.0.0.0:6767"}}, "not loopback"),
    ({"runtime": {"server_url": "http://example.com:6767"}}, "loopback"),
    ({"channels": {}}, "at least one"),
    ({"channels": {"telegram": {"enabled": True, "token_env": "123:ABC", "owner_id": "1"}}}, "NAME of an environment"),
    ({"channels": {"telegram": {"enabled": True, "token_env": "TOK", "owner_id": ""}}}, "owner_id"),
    ({"channels": {"web": {"enabled": True, "bind": "0.0.0.0"}}}, "not loopback"),
    ({"channels": {"whatsapp": {"enabled": True, "owner_id": "x"}}}, "accept_unofficial_client_risk"),
    ({"channels": {"signal": {"enabled": True}}}, "unknown channel"),
    ({"budget": {"daily_usd": -1}}, "daily_usd"),
    ({"sandbox": {"default": "docker"}}, "sandbox.default"),
    ({"backup": {"age_recipient": "not-a-key"}}, "age public key"),
])
def test_config_fails_closed(tmp_path, data, match):
    with pytest.raises(ConfigError, match=match):
        parse_config(_cfg(**data), base_dir=tmp_path, env={"HOME": "/h"})


def test_whatsapp_requires_explicit_risk_ack(tmp_path):
    cfg = parse_config(_cfg(channels={"whatsapp": {"enabled": True, "owner_id": "me",
                                                   "accept_unofficial_client_risk": True}}),
                       base_dir=tmp_path, env={})
    assert cfg.channel("whatsapp").accept_unofficial_client_risk


def test_example_files_parse_after_placeholder_fill(tmp_path):
    text = (REPO / "kurisu.example.yaml").read_text().replace("<OWNER_ID>", "1000")
    (tmp_path / "kurisu.yaml").write_text(text)
    (tmp_path / "roster.yaml").write_text((REPO / "roster.example.yaml").read_text())
    assert cli_main(["check", "--config", str(tmp_path / "kurisu.yaml")]) == 0


def test_example_config_with_placeholder_is_rejected(tmp_path):
    with pytest.raises(ConfigError, match="owner_id"):
        load_config(REPO / "kurisu.example.yaml")


# --------------------------------------------------------------- roster
def _roster(**roles):
    return {"version": 1, "roles": roles, "gates": {"merge": ["qa"]}}


GOOD = dict(
    lead={"kind": "lead", "folder": "/k/lead"},
    dev={"kind": "dev", "folder": "/code/app"},
    qa={"kind": "qa", "folder": "/code/app", "tier": "mechanical"},
)


def test_roster_valid(tmp_path):
    r = parse_roster(_roster(**GOOD), base_dir=tmp_path)
    assert r.lead.name == "lead"
    assert [s.name for s in r.staff] == ["dev", "qa"]
    assert r.gates_for("merge") == ("qa",)


@pytest.mark.parametrize("mutate,match", [
    (lambda r: r["roles"].pop("lead"), "exactly one"),
    (lambda r: r["roles"].update(lead2={"kind": "lead", "folder": "/x"}), "exactly one"),
    (lambda r: r["roles"].update({"Bad Name": {"kind": "dev", "folder": "/y"}}), "name must match"),
    (lambda r: r["roles"].update(dev2={"kind": "dev", "folder": "/code/app"}), "already used"),
    (lambda r: r["roles"]["dev"].update(tier="genius"), "tier"),
    (lambda r: r["roles"]["dev"].update(sandbox="vm"), "sandbox"),
    (lambda r: r["roles"]["dev"].pop("folder"), "folder"),
    (lambda r: r["gates"].update(merge=["ghost"]), "unknown role"),
    (lambda r: r["gates"].update(merge=["lead"]), "lead cannot gate"),
    (lambda r: r["gates"].update(launch=["qa"]), "gates.launch"),
])
def test_roster_fails_closed(tmp_path, mutate, match):
    data = yaml.safe_load(yaml.safe_dump(_roster(**GOOD)))
    mutate(data)
    with pytest.raises(ConfigError, match=match):
        parse_roster(data, base_dir=tmp_path)


# --------------------------------------------------------------- router
def test_router_escalates_next_task_only_and_caps():
    r = Router()
    d1 = r.decide("dev", "t1", "normal")
    assert (d1.tier, d1.model, d1.effort) == ("normal", "sonnet", "medium")
    r.report_struggle("dev")
    # mid-task switch refused (prompt cache)
    with pytest.raises(RuntimeError, match="between tasks"):
        r.decide("dev", "t2", "normal")
    r.finish("dev", "t1")
    d2 = r.decide("dev", "t2", "normal")
    assert d2.tier == "hard" and "struggle+1" in d2.reason
    r.finish("dev", "t2")
    for _ in range(5):
        r.report_struggle("dev")
    d3 = r.decide("dev", "t3", "normal")
    assert d3.tier == "hard" and d3.effort == "xhigh"  # capped at 2 escalations: top tier, effort +1
    r.finish("dev", "t3")
    r.report_success("dev")
    assert r.decide("dev", "t4", "mechanical").model == "haiku"


def test_router_same_task_is_idempotent():
    r = Router()
    assert r.decide("qa", "t1", "mechanical") == r.decide("qa", "t1", "mechanical")


# ------------------------------------------------------------ approvals
def test_approvals_single_use_expiry_and_audit():
    now = [1000.0]
    book = ApprovalBook(ttl_seconds=60, clock=lambda: now[0])
    a = book.request("dev", "push", "push main")
    book.resolve(a.id, True)
    assert book.is_approved(a.id)
    with pytest.raises(ValueError, match="single-use"):
        book.resolve(a.id, True)
    b = book.request("dev", "deploy", "deploy")
    now[0] += 61
    with pytest.raises(ValueError, match="expired"):
        book.resolve(b.id, True)
    assert not book.is_approved(b.id)
    assert [e["event"] for e in book.audit] == ["requested", "approved", "requested", "expired"]
    assert needs_approval("push") and not needs_approval("read_file")


# ----------------------------------------------------- memory / identity
def test_state_store_is_private_and_session_identity_sticks(tmp_path):
    store = StateStore(tmp_path / "state" / "kurisu.json")
    reg = SessionRegistry(store)
    reg.record("lead", "conv_a")
    assert oct(os.stat(store.path).st_mode & 0o777) == "0o600"
    reg.record("lead", "conv_a")  # idempotent
    with pytest.raises(KurisuError, match="refusing to replace"):
        reg.record("lead", "conv_b")
    assert SessionRegistry(StateStore(store.path)).get("lead") == "conv_a"
    os.chmod(store.path, 0o644)
    with pytest.raises(KurisuError, match="0600"):
        store.load()


# ------------------------------------------------------- misc skeleton
def test_backup_requires_encryption_and_excludes_credentials(tmp_path):
    with pytest.raises(ValueError):
        backup.backup_plan(tmp_path, tmp_path, "")
    tar, age = backup.backup_plan(tmp_path, tmp_path, "age1" + "q" * 58, now=0)
    assert "--exclude=.claude.json" in tar and age[:2] == ["age", "-r"]


def test_owner_gate_fails_closed():
    assert channels.is_owner("42", "42")
    assert not channels.is_owner("", "")
    assert not channels.is_owner("42", "43")


def test_sandbox_profiles():
    assert "trusted" in sandbox.describe("none")
    with pytest.raises(ValueError):
        sandbox.describe("vm")
