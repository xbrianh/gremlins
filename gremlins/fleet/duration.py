"""Duration string parser for --since."""

import re


def parse_duration(s: str) -> int:
    """Parse a duration string like 30s, 5m, 2h, 1d into seconds."""
    m = re.fullmatch(r"(\d+)([smhd])", s.strip())
    if not m:
        raise ValueError(f"unrecognised duration: {s!r} (expected e.g. 30s, 5m, 2h, 1d)")
    value, unit = int(m.group(1)), m.group(2)
    multipliers = {"s": 1, "m": 60, "h": 3600, "d": 86400}
    return value * multipliers[unit]
