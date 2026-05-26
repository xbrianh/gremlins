class Passthrough(dict[str, str]):
    """format_map helper: unknown {key} passes through unchanged."""

    def __missing__(self, key: str) -> str:
        return "{" + key + "}"
