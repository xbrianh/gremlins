"""State directory helpers and liveness classifier."""

import datetime
import json
import os
import re
import time
from collections.abc import Iterator
from typing import cast

import gremlins.fleet.constants as _constants
from gremlins import paths
from gremlins.utils import git as _git_mod


def iso_to_epoch(iso: str) -> float | None:
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


def display_id(gremlin_id: str) -> str:
    """Compact old-format IDs to their trailing rand6 hex; pass new-format through."""
    if re.match(r"^[0-9]{8}-[0-9]{6}-[0-9]+-([a-f0-9]{6}|xxxxxx)$", gremlin_id):
        return gremlin_id.rsplit("-", 1)[-1]
    return gremlin_id


def render_sub_stage(sub: str | dict[str, object] | None) -> str:
    """Format sub_stage: dict → key=val,... ; string passthrough; empty → ''."""
    if sub is None or sub == "":
        return ""
    if isinstance(sub, dict):
        if not sub:
            return ""
        return ",".join(f"{k}={json.dumps(v)}" for k, v in sub.items())
    return str(sub)


def _fmt_duration(secs: int) -> str:
    # Intentionally more precise than humanize_age (shows seconds within the hour).
    if secs < 60:
        return f"{secs}s"
    m, s = divmod(secs, 60)
    if m < 60:
        return f"{m}m{s:02d}s"
    h, m = divmod(m, 60)
    return f"{h}h{m:02d}m"


def liveness_of_state_file(sf: str, state: dict[str, object] | None = None) -> str:
    """
    Classify a gremlin's liveness from its state.json path.
    Returns one of: running, waiting (<duration>), finished, dead:<reason>, stalled:<reason>.
    Replicates liveness.sh inline — no shell-out.
    Pass an already-loaded state dict to avoid a second JSON parse.
    """
    if not os.path.isfile(sf):
        return ""
    wdir = os.path.dirname(sf)
    if state is None:
        try:
            with open(sf, encoding="utf-8") as fh:
                state = cast(dict[str, object], json.load(fh))
        except Exception:
            return ""

    gr_status = state.get("status")
    gr_pid = state.get("pid")
    gr_exit_code = state.get("exit_code")
    gr_bail_reason = state.get("bail_reason")
    is_boss = str(state.get("kind") or "") == "bossgremlin"

    # Terminal: finish.sh wrote the `finished` marker. A bail_reason takes
    # precedence over the generic exit code so listings show *why* a gremlin
    # gave up rather than just "dead:exit 2".
    if os.path.isfile(os.path.join(wdir, "finished")):
        if gr_bail_reason:
            return f"dead:bailed:{gr_bail_reason}"
        if gr_exit_code is not None and gr_exit_code != 0 and gr_exit_code != "null":
            return f"dead:exit {gr_exit_code}"
        return "finished"

    if gr_status == "running":
        # PID gone but no finish marker → crashed silently.
        if gr_pid is not None and gr_pid != "null" and isinstance(gr_pid, (int, str)):
            try:
                os.kill(int(gr_pid), 0)
            except (OSError, ValueError):
                workdir = str(state.get("workdir") or "")
                if workdir and not os.path.isdir(workdir):
                    return "dead:host-terminated"
                return f"dead:crashed (pid {gr_pid} gone)"

        # Boss gremlins are idle by design while waiting for a child; skip
        # the stall heuristic entirely and show time since last log write.
        if is_boss:
            stage = str(state.get("stage") or "")
            if stage == "waiting":
                log_path = os.path.join(wdir, "log")
                if os.path.isfile(log_path):
                    try:
                        age_secs = max(0, int(time.time() - os.path.getmtime(log_path)))
                        return f"waiting ({_fmt_duration(age_secs)})"
                    except OSError:
                        pass
                return "waiting"
            # Non-waiting boss stages (handoff, landing) show running unconditionally;
            # a hung boss during handoff won't surface as stalled. Acceptable trade-off:
            # these stages are brief and boss PIDs are always live while running.
            return "running"

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


def parse_liveness(live: str) -> dict[str, object]:
    """Convert a liveness string to a structured dict for JSON output."""
    if live in ("running", "finished", "waiting", ""):
        return {"state": live or "unknown"}
    if live.startswith("waiting (") and live.endswith(")"):
        return {"state": "waiting", "duration": live[9:-1]}
    if live.startswith("stalled:"):
        return {"state": "stalled", "detail": live[8:]}
    if live.startswith("dead:"):
        rest = live[5:]
        if rest.startswith("exit "):
            try:
                return {"state": "dead", "reason": "exit", "exit_code": int(rest[5:])}
            except ValueError:
                pass
        if rest.startswith("bailed:"):
            return {"state": "dead", "reason": "bailed", "bail_reason": rest[7:]}
        if rest.startswith("crashed "):
            return {"state": "dead", "reason": "crashed", "detail": rest[8:]}
        return {"state": "dead", "reason": rest}
    return {"state": live}


def iter_state_files() -> Iterator[tuple[str, str, str]]:
    """Yield (gremlin_id, state_file_path, wdir) for every gremlin in the state root."""
    state_root = str(paths.state_root())
    if not os.path.isdir(state_root):
        return
    try:
        entries = sorted(os.listdir(state_root))
    except OSError:
        return
    for name in entries:
        wdir = os.path.join(state_root, name)
        sf = os.path.join(wdir, "state.json")
        if os.path.isfile(sf):
            yield name, sf, wdir


def load_state(sf: str) -> dict[str, object] | None:
    """Load state.json, returning a dict or None on failure."""
    try:
        with open(sf, encoding="utf-8") as fh:
            return json.load(fh)  # type: ignore[no-any-return]
    except Exception:
        return None


def atomic_patch_state(sf: str, patch: dict[str, object]) -> bool:
    """Merge patch into state.json atomically. Returns True on success."""
    tmp = f"{sf}.patch.tmp.{os.getpid()}"
    try:
        with open(sf, encoding="utf-8") as fh:
            state = json.load(fh)
        state.update(patch)
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump(state, fh, indent=2)
        os.replace(tmp, sf)
        return True
    except Exception:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        return False


def git_toplevel() -> str:
    """Return the git toplevel of cwd, or cwd itself if not in a repo."""
    try:
        return _git_mod.toplevel()
    except Exception:
        return str(paths.project_root())
