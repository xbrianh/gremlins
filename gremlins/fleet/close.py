"""Close subcommand."""

import os

from gremlins.fleet.resolve import resolve_gremlin
from gremlins.fleet.state import liveness_of_state_file, load_state


def do_close(target: str) -> bool:
    match = resolve_gremlin(target)
    if match is None:
        return False

    gremlin_id, sf, wdir = match
    state = load_state(sf)
    if not state:
        print(f"error: could not read state for {gremlin_id}")
        return False

    live = liveness_of_state_file(sf, state)
    if live == "running" or live.startswith("stalled:"):
        print(
            f"gremlin {gremlin_id} is still live ({live}) — use 'stop' first, then close"
        )
        return False

    closed_marker = os.path.join(wdir, "closed")
    if os.path.isfile(closed_marker):
        print(f"gremlin {gremlin_id} already closed")
        return True

    try:
        with open(closed_marker, "a"):
            pass
    except OSError as e:
        print(f"error: could not write closed marker: {e}")
        return False

    print(f"closed {gremlin_id} ({live})")
    return True
