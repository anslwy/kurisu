"""Container-capability policy: the prohibited shortcuts stay prohibited."""

from __future__ import annotations

import pytest

from guest_broker import container_policy as cp


def test_host_sockets_are_denied_whatever_the_context():
    hostile = cp.RequestContext(
        outer_sandbox_mode="podman",
        host_has_kvm=True,
        operator_opt_in=True,
        userns_nesting_available=True,
        dedicated_budget=True,
    )
    for request in ("host-docker-socket", "host-podman-socket", "privileged-dind"):
        decision = cp.evaluate(request, hostile)
        assert decision.verdict == cp.DENY, request
        assert decision.reasons
        assert not decision.allowed


def test_privileged_dind_is_not_an_isolation_boundary():
    decision = cp.evaluate("privileged-dind")
    assert decision.verdict == cp.DENY
    assert any("not an isolation boundary" in reason for reason in decision.reasons)


def test_rootless_dind_in_sandbox_is_documented_but_off():
    decision = cp.evaluate("rootless-dind-in-sandbox")
    assert decision.verdict == cp.NOT_ENABLED
    assert decision.requirements
    assert any("nested user namespaces" in requirement for requirement in decision.requirements)


def test_per_user_rootless_daemon_is_denied():
    decision = cp.evaluate("per-user-rootless-daemon")
    assert decision.verdict == cp.DENY


def test_microvm_requires_kvm_and_opt_in():
    without_kvm = cp.evaluate("microvm", cp.RequestContext(host_has_kvm=False, operator_opt_in=True))
    assert without_kvm.verdict == cp.NOT_ENABLED

    without_optin = cp.evaluate("microvm", cp.RequestContext(host_has_kvm=True, operator_opt_in=False))
    assert without_optin.verdict == cp.NOT_ENABLED

    ready = cp.evaluate("microvm", cp.RequestContext(host_has_kvm=True, operator_opt_in=True))
    assert ready.verdict == cp.ALLOW
    assert ready.allowed


def test_no_request_grants_access_by_default():
    assert cp.allowed_requests() == ()
    decisions = cp.evaluate_all()
    assert len(decisions) == len(cp.REQUESTS)
    assert all(decision.verdict != cp.ALLOW for decision in decisions)


def test_unknown_request_is_rejected():
    with pytest.raises(KeyError):
        cp.evaluate("just-mount-the-socket")


def test_summary_is_reviewable():
    text = cp.summary()
    assert "DENY" in text and "host-docker-socket" in text
    assert "microvm" in text
