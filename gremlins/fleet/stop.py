"""Stop subcommand."""

import datetime
import json
import os
import pathlib
import signal
import subprocess
import time

from gremlins.fleet.resolve import resolve_gremlin
from gremlins.fleet.state import liveness_of_state_file, load_state


def do_stop(target: str) -> bool:
    match = resolve_gremlin(target)
    if match is None:
        return False

    gr_id, sf, wdir = match
    state = load_state(sf)
    if not state:
        print(f"error: could not read state for {gr_id}")
        return False

    live = liveness_of_state_file(sf, state)

    if live == "finished":
        print(f"gremlin {gr_id} already finished successfully — nothing to stop")
        return False
    if live == "dead:stopped":
        print(f"gremlin {gr_id} was already stopped")
        return False
    if live.startswith("dead:"):
        print(f"gremlin {gr_id} is already dead ({live})")
        print("Use 'rescue' to diagnose and continue from the failed stage.")
        return False

    stage = state.get("stage") or "-"
    pid = state.get("pid")

    if pid is None:
        print(f"error: no PID in state for {gr_id}")
        return False
    if not isinstance(pid, (int, str)):
        print(f"error: invalid PID {pid!r} in state for {gr_id}")
        return False
    try:
        pid = int(pid)
    except (ValueError, TypeError):
        print(f"error: invalid PID {pid!r} in state for {gr_id}")
        return False

    # Derive process group and send SIGTERM to the whole group.
    pgid = None
    try:
        ps_result = subprocess.run(
            ["ps", "-o", "pgid=", "-p", str(pid)],
            capture_output=True,
            text=True,
        )
        pgid_str = ps_result.stdout.strip()
        if pgid_str:
            pgid = int(pgid_str)
    except Exception:
        pass

    if pgid:
        try:
            os.killpg(pgid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        except Exception as e:
            print(f"warning: could not signal process group {pgid}: {e}")
    else:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        except Exception as e:
            print(f"warning: could not signal pid {pid}: {e}")

    # Poll for finish.sh to write the finished marker.
    finished_path = os.path.join(wdir, "finished")
    deadline = time.time() + 6.0
    while time.time() < deadline:
        if os.path.isfile(finished_path):
            break
        time.sleep(0.5)

    # If still absent, write it and patch state.json manually.
    if not os.path.isfile(finished_path):
        try:
            pathlib.Path(finished_path).touch()
        except OSError:
            pass
        now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
        state["status"] = "stopped"
        state["exit_code"] = 130
        state["ended_at"] = now_iso
        try:
            with open(sf, "w", encoding="utf-8") as fh:
                json.dump(state, fh, indent=2)
        except OSError as e:
            print(f"warning: could not patch state.json: {e}")

    print(f"stopped gremlin {gr_id} (stage: {stage})")
    return True
