"""ack and skip subcommands — record operator decision on a bailed child."""

import sys

from gremlins.fleet.resolve import resolve_gremlin
from gremlins.fleet.state import atomic_patch_state, load_state


def do_ack(target: str) -> bool:
    """Assert the bailed child's work is in main. Writes external_outcome=landed."""
    return _set_external_outcome(target, "landed")


def do_skip(target: str) -> bool:
    """Give up on the bailed child's work. Writes external_outcome=abandoned."""
    return _set_external_outcome(target, "abandoned")


def _set_external_outcome(target: str, outcome: str) -> bool:
    match = resolve_gremlin(target)
    if match is None:
        return False

    gr_id, sf, _wdir = match
    state = load_state(sf)
    if not state:
        print(f"error: could not read state for {gr_id}", file=sys.stderr)
        return False

    status = state.get("status")
    if status != "bailed":
        print(
            f"error: {gr_id} is not bailed (status={status!r})"
            " — ack/skip only apply to bailed gremlins",
            file=sys.stderr,
        )
        return False

    if not atomic_patch_state(sf, {"external_outcome": outcome}):
        print(f"error: could not write to {sf}", file=sys.stderr)
        return False

    print(f"{gr_id}: external_outcome={outcome!r}")
    return True
