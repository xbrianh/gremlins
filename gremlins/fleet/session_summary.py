"""SessionStart / UserPromptSubmit hook: report background gremlins for the current project.

Entry point: ``python -m gremlins.cli session-summary``

Running gremlins are shown at SessionStart; newly-finished gremlins are shown
in both hooks (and marked ``summarized`` on first show so they are not
re-announced). Degrades silently on any unexpected condition — hooks must
never break a session.
"""

from __future__ import annotations

import json
import os
import pathlib
import shutil
import subprocess
import sys
import time

from gremlins.fleet.state import liveness_of_state_file


def main(argv) -> int:
    """Run the session-summary hook. Always returns 0."""
    try:
        return _run()
    except Exception:
        return 0


def _run() -> int:
    if os.environ.get("GREMLIN_SKIP_SUMMARY") == "1":
        return 0

    state_root = _get_state_root()
    if not os.path.isdir(state_root):
        return 0

    hook_input = _read_stdin()
    hook_event = hook_input.get("hook_event_name") or ""
    cwd_from_input = hook_input.get("cwd") or ""

    project_root = _resolve_project_root(cwd_from_input)
    running, finished, newly_summarized_dirs = _collect_gremlins(state_root, project_root)

    # UserPromptSubmit with no new finishes → emit nothing (don't spam every prompt).
    if hook_event == "UserPromptSubmit" and not finished:
        return 0

    show_running = hook_event != "UserPromptSubmit"
    raw_summary = _render_summary(
        running if show_running else [],
        finished,
    )

    if raw_summary:
        _emit(hook_event, raw_summary)
        _mark_summarized(newly_summarized_dirs)

    _prune_old_state(state_root)
    return 0


def _get_state_root() -> str:
    xdg = os.environ.get("XDG_STATE_HOME") or os.path.join(
        os.path.expanduser("~"), ".local", "state"
    )
    return os.path.join(xdg, "claude-gremlins")


def _read_stdin() -> dict:
    try:
        if not sys.stdin.isatty():
            raw = sys.stdin.read()
            if raw:
                parsed = json.loads(raw)
                if isinstance(parsed, dict):
                    return parsed
    except Exception:
        pass
    return {}


def _resolve_project_root(cwd_from_input: str) -> str:
    root = os.environ.get("CLAUDE_PROJECT_DIR") or cwd_from_input or os.getcwd()
    if root:
        try:
            result = subprocess.run(
                ["git", "-C", root, "rev-parse", "--show-toplevel"],
                capture_output=True, text=True, check=True,
            )
            top = result.stdout.strip()
            if top:
                return top
        except Exception:
            pass
    return root


def _collect_gremlins(state_root: str, project_root: str):
    """Walk state files and split into running / newly-finished lists.

    Returns (running, finished, newly_summarized_dirs).
    """
    running = []
    finished = []
    newly_summarized_dirs = []

    try:
        entries = sorted(os.listdir(state_root))
    except OSError:
        return running, finished, newly_summarized_dirs

    for name in entries:
        wdir = os.path.join(state_root, name)
        sf = os.path.join(wdir, "state.json")
        if not os.path.isfile(sf):
            continue

        try:
            with open(sf, encoding="utf-8") as fh:
                state = json.load(fh)
        except Exception:
            continue

        if state.get("project_root") != project_root:
            continue

        gr_id = state.get("id") or name
        kind = state.get("kind") or ""
        gr_status = state.get("status") or ""
        pid = state.get("pid")
        exit_code = state.get("exit_code")
        stage = state.get("stage") or ""
        description = state.get("description") or state.get("instructions") or ""
        log = os.path.join(wdir, "log")

        finished_marker = os.path.join(wdir, "finished")
        summarized_marker = os.path.join(wdir, "summarized")
        closed_marker = os.path.join(wdir, "closed")

        if (
            os.path.isfile(finished_marker)
            and not os.path.isfile(summarized_marker)
            and not os.path.isfile(closed_marker)
        ):
            # exit_code None → empty string (no suffix rendered)
            exit_code_str = "" if exit_code is None else str(exit_code)
            finished.append({
                "id": gr_id,
                "kind": kind,
                "status": gr_status,
                "exit_code": exit_code_str,
                "description": description,
                "log": log,
                "wdir": wdir,
            })
            newly_summarized_dirs.append(wdir)
            continue

        if gr_status == "running":
            live = liveness_of_state_file(sf, state=state)
            running.append({
                "id": gr_id,
                "kind": kind,
                "live": live,
                "stage": stage,
                "pid": str(pid) if pid is not None else "",
                "description": description,
                "log": log,
            })

    return running, finished, newly_summarized_dirs


def _render_summary(running: list, finished: list) -> str:
    """Build the markdown summary block. Returns empty string if nothing to show."""
    running_block = ""
    for g in running:
        desc_suffix = f" — _{g['description']}_" if g["description"] else ""
        pid_str = g["pid"] or "?"
        stage_disp = g["stage"] or "?"
        live = g["live"]
        if live.startswith("dead:"):
            reason = live[len("dead:"):]
            running_block += (
                f"- `{g['id']}` ({g['kind']}): **{reason}**"
                f" (stage: {stage_disp}){desc_suffix} — log: {g['log']}\n"
            )
        elif live.startswith("stalled:"):
            reason = live[len("stalled:"):]
            running_block += (
                f"- `{g['id']}` ({g['kind']}): **stalled?**"
                f" ({reason}, stage: {stage_disp}, pid {pid_str}){desc_suffix} — log: {g['log']}\n"
            )
        else:
            running_block += (
                f"- `{g['id']}` ({g['kind']}): running"
                f" (stage: {stage_disp}, pid {pid_str}){desc_suffix} — log: {g['log']}\n"
            )

    finished_block = ""
    for g in finished:
        desc_suffix = f" — _{g['description']}_" if g["description"] else ""
        exit_suffix = f" (exit {g['exit_code']})" if g["exit_code"] else ""
        finished_block += (
            f"- `{g['id']}` ({g['kind']}): **{g['status']}**"
            f"{exit_suffix}{desc_suffix} — log: {g['log']}\n"
        )

    summary = ""
    if running_block:
        summary += f"**Background gremlins — running:**\n{running_block}"
    if finished_block:
        if summary:
            summary += "\n"
        summary += f"**Background gremlins — finished since last check:**\n{finished_block}"

    return summary


def _emit(hook_event: str, raw_summary: str) -> None:
    """Write raw summary to stderr and JSON envelope to stdout."""
    directive = (
        "IMPORTANT: Before doing anything else in your next response, "
        "surface the following background-gremlin status to the user verbatim "
        "(as a markdown block, no paraphrasing):\n\n"
    )
    full = directive + raw_summary
    event_out = hook_event or "SessionStart"

    sys.stderr.write(raw_summary)
    sys.stderr.flush()

    output = json.dumps({
        "hookSpecificOutput": {
            "hookEventName": event_out,
            "additionalContext": full,
        }
    })
    sys.stdout.write(output + "\n")
    sys.stdout.flush()


def _mark_summarized(state_dirs: list) -> None:
    for d in state_dirs:
        try:
            pathlib.Path(d, "summarized").touch()
        except OSError:
            pass


def _prune_old_state(state_root: str) -> None:
    """Remove closed state dirs and direct/ session dirs older than 14 days."""
    cutoff = time.time() - 14 * 86400

    try:
        entries = os.listdir(state_root)
    except OSError:
        return

    for name in entries:
        wdir = os.path.join(state_root, name)
        closed = os.path.join(wdir, "closed")
        if os.path.isfile(closed):
            try:
                if os.path.getmtime(closed) < cutoff:
                    shutil.rmtree(wdir, ignore_errors=True)
            except OSError:
                pass

    direct = os.path.join(state_root, "direct")
    if not os.path.isdir(direct):
        return
    try:
        d_entries = os.listdir(direct)
    except OSError:
        return
    for name in d_entries:
        d = os.path.join(direct, name)
        if os.path.isdir(d):
            try:
                if os.path.getmtime(d) < cutoff:
                    shutil.rmtree(d, ignore_errors=True)
            except OSError:
                pass
