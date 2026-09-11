"""Gremlin resolution by id prefix."""

import logging

from gremlins.fleet.state import iter_state_files

logger = logging.getLogger(__name__)


def collect_gremlin_matches(
    target: str,
) -> tuple[list[tuple[str, str, str]], tuple[str, str, str] | None]:
    """Return (all_substring_matches, exact_match_or_none) for target."""
    matches = [(gid, sf, wdir) for gid, sf, wdir in iter_state_files() if target in gid]
    exact = next((m for m in matches if m[0] == target), None)
    return matches, exact


def resolve_gremlin(target: str) -> tuple[str, str, str] | None:
    """Resolve id substring to a single (gremlin_id, sf, wdir) or print error and return None."""
    matches, exact = collect_gremlin_matches(target)
    logger.debug(
        "resolve_gremlin: target=%s matches=%d exact=%s",
        target,
        len(matches),
        exact is not None,
    )
    if not matches:
        print(f"no gremlin matched: {target}")
        return None
    if exact is not None:
        return exact
    if len(matches) > 1:
        print(
            f"ambiguous id '{target}' matched {len(matches)} gremlins — use a longer prefix:"
        )
        for gremlin_id, _, _ in matches:
            print(f"  {gremlin_id}")
        return None
    return matches[0]
