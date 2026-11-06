"""Broker error types.

Every failure mode that must fail *closed* has a dedicated exception so that
callers cannot accidentally treat a denial as an empty success.
"""

from __future__ import annotations


class BrokerError(Exception):
    """Base class for all broker failures."""


class ConfigError(BrokerError):
    """Configuration is missing, malformed, or still contains placeholders."""


class RegistryError(BrokerError):
    """The server-enforced user -> session registry is invalid."""


class UnauthorizedError(BrokerError):
    """An inbound identity is not on the allowlist, or is ambiguous."""


class ProtocolError(BrokerError):
    """A wire message is malformed, oversized, or carries forbidden fields."""


class SandboxError(BrokerError):
    """The sandbox could not be established. Never degrade to no sandbox."""


class LimitExceededError(BrokerError):
    """A per-user or per-session resource budget was exhausted."""


class SessionBackendError(BrokerError):
    """The guest session backend failed or is not configured."""
