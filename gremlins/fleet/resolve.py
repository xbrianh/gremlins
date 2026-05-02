"""Gremlin resolution by id prefix."""

from gremlins.fleet.state import iter_state_files

GREMLIN_STAGES = {
    "localgremlin": ["plan", "implement", "review-code", "address-code"],
    "ghgremlin": ["plan", "implement", "commit-pr", "request-copilot", "ghreview", "wait-copilot", "ghaddress"],
    "bossgremlin": ["handoff", "waiting", "landing", "rescuing"],
}


def resolve_gremlin(target: str):
    """Resolve id prefix to a single (gr_id, sf, wdir) or print error and return None."""
    matches = []
    for gr_id, sf, wdir in iter_state_files():
        if target in gr_id:
            matches.append((gr_id, sf, wdir))
    if not matches:
        print(f"no gremlin matched: {target}")
        return None
    if len(matches) > 1:
        print(f"ambiguous id '{target}' matched {len(matches)} gremlins — use a longer prefix:")
        for gr_id, _, _ in matches:
            print(f"  {gr_id}")
        return None
    return matches[0]
