class KurisuError(Exception):
    """Base error."""


class ConfigError(KurisuError):
    """Invalid kurisu.yaml or roster.yaml."""
