"""State directory helpers and liveness classifier."""

import datetime
import json
import os
import re
import subprocess
import time

from gremlins.fleet import constants as _constants


def iso_to_epoch(iso: str):
    """Parse ISO-8601 string to a UTC epoch float. Returns None on failure."""
    if not iso:
        return None
    try:
        # Python < 3.11 does not accept 'Z' suffix directly.
        dt = datetime.datetime.fromisoformat(iso.replace("Z", "+00:00"))
        return dt.timestamp()
    except (ValueError, TypeError):
        return None


def humanize_age(started_at: str) -> str:
    """Return a human-readable age string like 5s, 12m, 3h, 2d."""
    epoch = iso_to_epoch(started_at)
    if epoch is None:
        return "-"
    diff = int(time.time() - epoch)
    if diff < 60:
        return f"{diff}s"
    if diff < 3600:
        return f"{diff // 60}m"
    if diff < 86400:
        return f"{diff // 3600}h"
    return f"{diff // 86400}d"


def display_id(gr_id: str) -> str:
    """Compact old-format IDs to their trailing rand6 hex; pass new-format through."""
    if re.match(r"^[0-9]{8}-[0-9]{6}-[0-9]+-([a-f0-9]{6}|xxxxxx)$", gr_id):
        return gr_id.rsplit("-", 1)[-1]
    return gr_id


def render_sub_stage(sub) -> str:
    """Format sub_stage: dict → key=val,... ; string passthrough; empty → ''."""
    if sub is None or sub == "":
        return ""
    if isinstance(sub, dict):
        if not sub:
            return ""
        return ",".join(f"{k}={json.dumps(v)}" for k, v in sub.items())
    return str(sub)


def liveness_of_state_file(sf: str, state=None) -> str:
    """
    Classify a gremlin's liveness from its state.json path.
    Returns one of: running, dead:<reason>, stalled:<reason>.
    Replicates liveness.sh inline — no shell-out.
    Pass an already-loaded state dict to avoid a second JSON parse.
    """
    if not os.path.isfile(sf):
        return ""
    wdir = os.path.dirname(sf)
    if state is None:
        try:
            with open(sf, encoding="utf-8") as fh:
                state = json.load(fh)
        except Exception:
            return ""

    gr_status = state.get("status")
    gr_pid = state.get("pid")
    gr_exit_code = state.get("exit_code")
    gr_bail_reason = state.get("bail_reason")

    # Terminal: finish.sh (or headless rescue's bail path) wrote the
    # `finished` marker. A bail_reason takes precedence over the generic
    # exit code so listings show *why* rescue gave up rather than just
    # "dead:exit 2".
    if os.path.isfile(os.path.join(wdir, "finished")):
        if gr_bail_reason:
            return f"dead:bailed:{gr_bail_reason}"
        if gr_exit_code is not None and gr_exit_code != 0 and gr_exit_code != "null":
            return f"dead:exit {gr_exit_code}"
        return "dead:finished"

    if gr_status == "running":
        # PID gone but no finish marker → crashed silently.
        if gr_pid is not None and gr_pid != "null":
            try:
                os.kill(int(gr_pid), 0)
            except (OSError, ValueError):
                workdir = state.get("workdir") or ""
                if workdir and not os.path.isdir(workdir):
                    return "dead:host-terminated"
                return f"dead:crashed (pid {gr_pid} gone)"

        # Stall heuristic: log file hasn't moved in BG_STALL_SECS.
        log_path = os.path.join(wdir, "log")
        if os.path.isfile(log_path):
            try:
                mtime = os.path.getmtime(log_path)
                age = int(time.time() - mtime)
                if age > _constants.BG_STALL_SECS:
                    return f"stalled:no log update {age // 60}m"
            except OSError:
                pass

        return "running"

    # Non-running status without a finished marker.
    if gr_exit_code is not None and gr_exit_code != 0 and gr_exit_code != "null":
        return f"dead:exit {gr_exit_code}"
    return f"dead:{gr_status or 'unknown'}"


def iter_state_files():
    """Yield (gr_id, state_file_path, wdir) for every gremlin in STATE_ROOT."""
    if not os.path.isdir(_constants.STATE_ROOT):
        return
    try:
        entries = sorted(os.listdir(_constants.STATE_ROOT))
    except OSError:
        return
    for name in entries:
        wdir = os.path.join(_constants.STATE_ROOT, name)
        sf = os.path.join(wdir, "state.json")
        if os.path.isfile(sf):
            yield name, sf, wdir


def load_state(sf: str):
    """Load state.json, returning a dict or None on failure."""
    try:
        with open(sf, encoding="utf-8") as fh:
            return json.load(fh)
    except Exception:
        return None


def kind_short(kind: str) -> str:
    if kind == "localgremlin":
        return "local"
    if kind == "ghgremlin":
        return "gh"
    if kind == "bossgremlin":
        return "boss"
    return kind or ""


def git_toplevel() -> str:
    """Return the git toplevel of cwd, or cwd itself if not in a repo."""
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, check=True,
        )
        return result.stdout.strip()
    except Exception:
        return os.getcwd()
