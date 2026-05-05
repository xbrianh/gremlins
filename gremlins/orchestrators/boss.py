"""Orchestrator entry point for the boss pipeline.

Drives a chain of child gremlins serially, invoking handoff between each step.
Chain state lives in boss_state.json.

Receives pipeline args forwarded by the launcher:
  boss_main --plan <spec-path> --chain-kind local|gh [--model <model>]
  [--resume-from <stage>]    ← added by the launcher resume path; ignored
                                (we use boss_state.json for resumption)

GR_ID env var is set by the launcher.
"""

from __future__ import annotations

import argparse
import datetime
import json
import logging
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import time
import types
from typing import Any, NoReturn, cast

from .. import git as _git_mod
from .. import handoff as _handoff_mod
from ..clients.claude import SubprocessClaudeClient
from ..clients.protocol import ClaudeClient
from ..gh_utils import get_repo, parse_issue_ref, view_issue
from ..launcher import launch as _launch
from ..logging_setup import configure_logging
from ..state import patch_state, set_stage

logger = logging.getLogger(__name__)

STATE_ROOT = os.path.join(
    os.environ.get(
        "XDG_STATE_HOME", os.path.join(os.path.expanduser("~"), ".local", "state")
    ),
    "claude-gremlins",
)

# Parent of the gremlins package — where ``python -m gremlins.cli`` needs to
# find ``gremlins/`` on PYTHONPATH when boss subprocesses out to handoff /
# fleet. Resolved from this file's path so the same code works whether the
# package lives under ``~/.claude/gremlins/`` (production) or under a test
# fake-home tree.
_GREMLINS_PARENT = str(pathlib.Path(__file__).resolve().parents[2])


def _gremlins_cli_cmd(*args: str) -> list[str]:
    return [sys.executable, "-m", "gremlins.cli", *args]


def _gremlins_cli_env() -> dict[str, str]:
    env = os.environ.copy()
    existing = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        f"{_GREMLINS_PARENT}{os.pathsep}{existing}" if existing else _GREMLINS_PARENT
    )
    env["PYTHONSAFEPATH"] = "1"
    return env


POLL_INTERVAL = 5  # seconds between finished-marker polls
HANDOFF_TIMEOUT = int(os.environ.get("BOSSGREMLIN_HANDOFF_TIMEOUT", "3600"))
# Bounds every interaction with `origin` (chain-start fetch of the default
# branch and per-handoff fetch of the target branch).
HANDOFF_FETCH_TIMEOUT = int(os.environ.get("BOSSGREMLIN_HANDOFF_FETCH_TIMEOUT", "60"))
GH_VIEW_TIMEOUT = 30  # seconds; bounds `gh repo view` at chain start

_current_proc = None
_stop_requested = False


def _sigterm_handler(signum: int, frame: types.FrameType | None) -> None:
    global _stop_requested
    _stop_requested = True
    logger.info("received SIGTERM — stopping after current operation")
    if _current_proc is not None:
        try:
            _current_proc.send_signal(signal.SIGTERM)
        except Exception:
            pass


def die(msg: str) -> NoReturn:
    logger.error("fatal: %s", msg)
    sys.exit(1)


def check_stop() -> None:
    if _stop_requested:
        logger.info("stop requested — exiting")
        sys.exit(130)


def load_json(path: str) -> dict[str, Any]:
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def save_json(path: str, data: dict[str, Any]) -> None:
    tmp = f"{path}.tmp.{os.getpid()}"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2)
    os.replace(tmp, path)


def run_proc(cmd: list[str], **kwargs: Any) -> int:
    """Run subprocess, returning exit code. Forwards SIGTERM on stop."""
    global _current_proc
    proc = subprocess.Popen(cmd, **kwargs)
    _current_proc = proc
    try:
        proc.wait()
    except Exception:
        proc.kill()
        proc.wait()
    finally:
        _current_proc = None
    return proc.returncode


def get_head_ref(project_root: str) -> str:
    r = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        cwd=project_root,
    )
    if r.returncode != 0:
        die(f"git rev-parse HEAD failed in {project_root}: {r.stderr.strip()}")
    return r.stdout.strip()


def get_current_branch(project_root: str) -> str:
    r = subprocess.run(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"],
        capture_output=True,
        text=True,
        cwd=project_root,
    )
    if r.returncode != 0:
        return ""
    branch = r.stdout.strip()
    return "" if branch == "HEAD" else branch


def get_default_branch(project_root: str) -> str:
    """Resolve the repo's default branch via gh CLI. Calls die() on failure."""
    try:
        r = subprocess.run(
            [
                "gh",
                "repo",
                "view",
                "--json",
                "defaultBranchRef",
                "-q",
                ".defaultBranchRef.name",
            ],
            capture_output=True,
            text=True,
            cwd=project_root,
            timeout=GH_VIEW_TIMEOUT,
        )
    except FileNotFoundError:
        die(
            "gh CLI not found on PATH — required to resolve default branch for gh chain"
        )
    except subprocess.TimeoutExpired:
        die(f"gh repo view timed out after {GH_VIEW_TIMEOUT}s in {project_root}")
    if r.returncode != 0:
        die(f"gh repo view failed in {project_root}: {r.stderr.strip()}")
    name = r.stdout.strip()
    if not name:
        die(f"gh repo view returned empty default branch in {project_root}")
    return name


def fetch_origin_branch(project_root: str, branch: str, *, context: str) -> None:
    """Fetch origin/<branch> with a bounded timeout. Calls die() on failure.

    Uses an explicit refspec so a branch name starting with `-` cannot be
    parsed by `git fetch` as an option.
    """
    refspec = f"refs/heads/{branch}:refs/remotes/origin/{branch}"
    try:
        fetch = subprocess.run(
            ["git", "fetch", "origin", refspec],
            capture_output=True,
            text=True,
            cwd=project_root,
            timeout=HANDOFF_FETCH_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        die(
            f"git fetch origin {refspec} timed out after {HANDOFF_FETCH_TIMEOUT}s {context}"
        )
    if fetch.returncode != 0:
        die(f"git fetch origin {refspec} failed {context}: {fetch.stderr.strip()}")


def get_remote_branch_sha(project_root: str, branch: str) -> str:
    """Fetch origin/<branch> and return its SHA. Calls die() on failure."""
    fetch_origin_branch(project_root, branch, context="at chain start")
    r = subprocess.run(
        ["git", "rev-parse", f"refs/remotes/origin/{branch}"],
        capture_output=True,
        text=True,
        cwd=project_root,
    )
    if r.returncode != 0:
        die(f"git rev-parse refs/remotes/origin/{branch} failed: {r.stderr.strip()}")
    return r.stdout.strip()


def init_boss_state(
    spec_path: str,
    chain_kind: str,
    chain_base_ref: str,
    target_branch: str,
    state_dir: str,
    issue_url: str = "",
    issue_num: str = "",
    test_cmd: str = "",
    test_max_attempts: int = 3,
    test_fix_model: str = "",
) -> dict[str, Any]:
    boss_state: dict[str, Any] = {
        "spec_path": spec_path,
        "chain_kind": chain_kind,
        "chain_base_ref": chain_base_ref,
        "target_branch": target_branch,
        "current_plan": spec_path,
        "handoff_count": 0,
        "current_child_id": None,
        "children": [],
        "handoff_records": [],
        # Source of the spec: empty for local-file inputs, populated when
        # --plan was a GitHub issue reference. Persisted so `/gremlins`
        # status can show the issue link and so resume never re-fetches.
        "issue_url": issue_url,
        "issue_num": issue_num,
        # Latest operator_followups list reported by handoff. Each handoff
        # rewrites this with the conservative carry-forward set the handoff
        # agent produced, so by chain-done it holds the final list of
        # operator tasks the human still owes between phase landings.
        "operator_followups": [],
        # Test command forwarded to every child local gremlin. Persisted so
        # a rescued boss continues forwarding the same command after a crash.
        "test_cmd": test_cmd,
        "test_max_attempts": test_max_attempts,
        "test_fix_model": test_fix_model,
    }
    save_json(os.path.join(state_dir, "boss_state.json"), boss_state)
    return boss_state


def load_boss_state(state_dir: str) -> dict[str, Any]:
    return load_json(os.path.join(state_dir, "boss_state.json"))


def save_boss_state(state_dir: str, boss_state: dict[str, Any]) -> None:
    save_json(os.path.join(state_dir, "boss_state.json"), boss_state)


def run_handoff(
    client: ClaudeClient,
    gr_id: str,
    state_dir: str,
    boss_state: dict[str, Any],
    project_root: str,
    boss_workdir: str,
    model: str,
) -> tuple[str, dict[str, Any]]:
    """Run handoff agent. Returns (exit_state, signal dict).

    Updates boss_state in place (handoff_count, current_plan, handoff_records).
    Calls die() on infrastructure failure.

    Children of a local chain squash-land into the boss's own workdir HEAD,
    while children of a gh chain push to origin/<target_branch>. Pick the
    right cwd/rev so handoff sees the actually-landed work, not a stale ref
    in the user's repo that may never advance during the chain.
    """
    set_stage(gr_id, "handoff")

    n = boss_state["handoff_count"] + 1
    out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
    signal_path = os.path.join(state_dir, f"handoff-{n:03d}.state.json")
    current_plan = boss_state["current_plan"]
    spec_path = boss_state["spec_path"]
    base_ref = boss_state["chain_base_ref"]
    chain_kind = boss_state.get("chain_kind")
    target_branch = boss_state.get("target_branch", "")

    handoff_cwd: str = ""
    if chain_kind == "local":
        if not boss_workdir or not os.path.isdir(boss_workdir):
            die(f"boss workdir not usable for local chain handoff: {boss_workdir!r}")
        handoff_cwd = boss_workdir
        rev_label = "HEAD"
        rev_val = None
    elif chain_kind == "gh":
        if not target_branch:
            die("gh chain has no target branch — cannot resolve remote ref for handoff")
        # Refresh the remote-tracking ref so we see PRs that landed on the
        # remote. Bound the fetch so an unreachable origin can't stall the
        # chain indefinitely between handoffs.
        fetch_origin_branch(project_root, target_branch, context="before handoff")
        handoff_cwd = project_root
        rev_label = f"origin/{target_branch}"
        rev_val = rev_label
    else:
        die(f"unknown chain_kind: {chain_kind!r}")

    # Only forward --spec once the rolling plan has diverged from the spec.
    # On handoff #1, init_boss_state seeds current_plan = spec_path, so
    # passing --spec would render the same document twice in the prompt.
    forward_spec = bool(spec_path) and spec_path != current_plan
    spec_log = spec_path if forward_spec else "(none)"
    logger.info(
        "handoff %d: plan=%s, spec=%s, base=%s, rev=%s, cwd=%s",
        n,
        current_plan,
        spec_log,
        base_ref[:12],
        rev_label,
        handoff_cwd,
    )

    # Switch to handoff_cwd before calling handoff.run() so git operations
    # resolve refs correctly
    saved_cwd = os.getcwd()
    try:
        os.chdir(handoff_cwd)
        args = argparse.Namespace(
            plan=current_plan,
            spec=spec_path if forward_spec else None,
            out=out_path,
            base=base_ref,
            model=model,
            timeout=HANDOFF_TIMEOUT,
            rev=rev_val,
        )
        rc = _handoff_mod.run(client, args)
    finally:
        os.chdir(saved_cwd)

    check_stop()

    if rc != 0:
        die(f"handoff agent exited {rc}")

    if not os.path.isfile(signal_path):
        die(f"handoff signal file not written: {signal_path}")

    try:
        sig = load_json(signal_path)
    except Exception as exc:
        die(f"could not parse handoff signal file {signal_path}: {exc}")

    exit_state = sig.get("exit_state")
    if exit_state not in ("next-plan", "chain-done", "bail"):
        die(f"handoff signal file has unrecognized exit_state: {exit_state!r}")

    # Coerce operator_followups to a list of strings. Old handoff signals
    # predating the field land here as None or absent; treat that as no
    # followups so a chain that started under an older handoff still reads.
    raw_followups = sig.get("operator_followups")
    if isinstance(raw_followups, list):
        followups = [
            str(item) for item in cast(list[Any], raw_followups) if str(item).strip()
        ]
    else:
        followups = []

    now = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
    boss_state["handoff_records"].append(
        {
            "timestamp": now,
            "n": n,
            "plan_in": current_plan,
            "plan_out": out_path,
            "signal_file": signal_path,
            "exit_state": exit_state,
            "child_plan": sig.get("child_plan"),
            "bail_reason": sig.get("reason"),
            "operator_followups": followups,
        }
    )
    boss_state["handoff_count"] = n
    boss_state["operator_followups"] = followups
    if os.path.isfile(out_path):
        boss_state["current_plan"] = out_path

    logger.info("handoff %d result: %s", n, exit_state)
    if followups:
        logger.info(
            "  operator follow-ups carried by handoff %d: %d", n, len(followups)
        )
        for item in followups:
            logger.info("    - %s", item)
    return exit_state, sig


def launch_child(gr_id: str, launch_kind: str, child_plan: str) -> str:
    """Launch a child gremlin via the Python launcher. Returns child gremlin ID."""
    gremlin_state = load_json(os.path.join(STATE_ROOT, gr_id, "state.json"))
    project_root = gremlin_state.get("project_root") or None
    base_ref = gremlin_state.get("current_head") or "HEAD"
    boss_state = load_boss_state(os.path.join(STATE_ROOT, gr_id))
    spec_path = boss_state.get("spec_path") or None

    test_cmd = boss_state.get("test_cmd") or None
    test_max = boss_state.get("test_max_attempts")
    test_model = boss_state.get("test_fix_model") or None
    extra: list[str] = []
    if test_cmd:
        extra += ["--cmd", test_cmd]
        if test_max is not None:
            extra += ["--test-max-attempts", str(test_max)]
        if test_model:
            extra += ["-t", test_model]

    logger.info(
        "launching child (%s): %s, base=%s", launch_kind, child_plan, base_ref[:12]
    )
    try:
        child_id = _launch(
            launch_kind,
            plan=child_plan,
            parent_id=gr_id,
            project_root=project_root,
            base_ref=base_ref,
            spec_path=spec_path,
            pipeline_args=tuple(extra),
        )
    except (ValueError, RuntimeError) as exc:
        die(f"launcher failed: {exc}")

    logger.info("child launched: %s", child_id)
    check_stop()
    return child_id


def wait_for_child(child_id: str, gr_id: str) -> bool:
    """Poll until child has a finished marker. Returns True on clean exit (exit_code 0).

    On stop request, sends stop to the child before exiting.
    """
    child_wdir = os.path.join(STATE_ROOT, child_id)
    finished_path = os.path.join(child_wdir, "finished")
    state_path = os.path.join(child_wdir, "state.json")

    logger.info("waiting for child %s...", child_id)
    while True:
        if _stop_requested:
            logger.info("stop requested — stopping child %s", child_id)
            subprocess.run(
                _gremlins_cli_cmd("stop", child_id),
                capture_output=True,
                env=_gremlins_cli_env(),
            )
            sys.exit(130)

        if os.path.isfile(finished_path):
            break

        if os.path.isfile(state_path):
            try:
                state = load_json(state_path)
                if state.get("status") == "running":
                    pid = state.get("pid")
                    if pid is not None:
                        try:
                            os.kill(int(pid), 0)
                        except (OSError, ValueError):
                            logger.info("child %s crashed (pid %s gone)", child_id, pid)
                            break
            except Exception:
                pass

        time.sleep(POLL_INTERVAL)

    if os.path.isfile(state_path):
        try:
            state = load_json(state_path)
            return state.get("exit_code") == 0
        except Exception:
            pass
    return False


def child_is_closed(child_id: str) -> bool:
    return os.path.isfile(os.path.join(STATE_ROOT, child_id, "closed"))


def get_child_bail_reason(child_id: str) -> str:
    state_path = os.path.join(STATE_ROOT, child_id, "state.json")
    if not os.path.isfile(state_path):
        return ""
    try:
        state = load_json(state_path)
        return state.get("bail_reason") or state.get("bail_class") or ""
    except Exception:
        return ""


def get_child_bail_detail(child_id: str) -> str:
    state_path = os.path.join(STATE_ROOT, child_id, "state.json")
    if not os.path.isfile(state_path):
        return ""
    try:
        state = load_json(state_path)
        return state.get("bail_detail") or ""
    except Exception:
        return ""


def _summarize_for_log(text: str, limit: int = 240) -> str:
    """Collapse to one line + cap length for boss-log readability.

    bail_detail is whatever the headless rescue agent chose to put there.
    Keep the boss log resilient against multi-line or runaway text without
    losing the underlying field in state.json (which we don't truncate).
    """
    if not text:
        return ""
    one_line = " ".join(text.split()).strip()
    if len(one_line) > limit:
        return one_line[: limit - 3] + "..."
    return one_line


def _classify_from_child_state(child_id: str) -> str:
    """Return verdict for a bailed child based on its state.json.

    Verdicts: "running", "landed", "landed-externally", "abandoned", "no-decision".
    """
    state_path = os.path.join(STATE_ROOT, child_id, "state.json")
    try:
        s = load_json(state_path)
    except Exception:
        return "no-decision"
    if s.get("status") == "running":
        return "running"
    if s.get("status") == "done" and s.get("exit_code") == 0:
        return "landed"
    ext = s.get("external_outcome")
    if ext == "landed":
        return "landed-externally"
    if ext == "abandoned":
        return "abandoned"
    return "no-decision"


def _last_bailed_child(boss_state: dict[str, Any]) -> dict[str, Any] | None:
    """Return the most recent bailed child entry, or None."""
    for entry in reversed(boss_state.get("children", [])):
        if str(entry.get("outcome", "")).startswith("bailed"):
            return entry
    return None


def _is_fresh_rescue(state_dir: str) -> bool:
    """True when the boss was just rescued (rescued_at newer than boss_state.json mtime).

    The launcher writes rescued_at to state.json on resume. boss_state.json is
    saved on every loop iteration, so after the first iteration post-rescue its
    mtime overtakes rescued_at and this returns False.
    """
    state_path = os.path.join(state_dir, "state.json")
    boss_state_path = os.path.join(state_dir, "boss_state.json")
    try:
        state = load_json(state_path)
        rescued_at_str = state.get("rescued_at")
        if not rescued_at_str:
            return False
        rescued_dt = datetime.datetime.fromisoformat(
            rescued_at_str.replace("Z", "+00:00")
        )
        boss_mtime = os.path.getmtime(boss_state_path)
        boss_dt = datetime.datetime.fromtimestamp(boss_mtime, tz=datetime.UTC)
        return rescued_dt > boss_dt
    except Exception:
        return False


def _format_no_decision_message(child_id: str) -> str:
    return (
        f"chain halted: child {child_id} bailed with no operator decision recorded.\n"
        f"  Choose one of:\n"
        f"    gremlins resume {child_id}   (re-run from bail point after fixing the issue)\n"
        f"    gremlins ack {child_id}      (work is in main — proceed to next handoff)\n"
        f"    gremlins skip {child_id}     (give up on this child — re-handoff plans something new)\n"
        f"  Then rescue the boss: gremlins rescue <boss-id>"
    )


def land_child(child_id: str, into_dir: str = "") -> bool:
    logger.info("landing child %s", child_id)
    cmd = _gremlins_cli_cmd("land", child_id)
    if into_dir:
        cmd += ["--into", into_dir]
    return run_proc(cmd, env=_gremlins_cli_env()) == 0


def rescue_child(child_id: str) -> bool:
    logger.info("rescuing child %s (headless)", child_id)
    return (
        run_proc(
            _gremlins_cli_cmd("rescue", "--headless", "--from-boss", child_id),
            env=_gremlins_cli_env(),
        )
        == 0
    )


def _parse_boss_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(add_help=False)
    p.add_argument("--plan", required=True)
    p.add_argument("--chain-kind", required=True, choices=["local", "gh"])
    p.add_argument("--model", default="sonnet")
    p.add_argument("--resume-from", default=None)
    p.add_argument("--test", dest="test_cmd", default=None)
    p.add_argument("--test-max-attempts", dest="test_max_attempts", type=int, default=3)
    p.add_argument("-t", dest="test_fix_model", default="sonnet")
    args, _ = p.parse_known_args(argv)
    return args


def _resolve_plan_source(
    plan: str, state_dir: str, *, gr_id: str | None = None
) -> tuple[str, str, str, str]:
    """Resolve --plan into a snapshot under ``<state_dir>/spec.md``.

    Accepts the same forms as ghgremlin's --plan: a local file path, ``42`` /
    ``#42``, ``owner/name#42``, or a full ``https://github.com/.../issues/N``
    URL. The returned ``spec_path`` is always the snapshot — boss handoffs
    only ever read the snapshot, never the original input.

    Idempotent: if ``spec.md`` already exists with non-zero size, it is
    treated as authoritative and no re-fetch is performed. This handles the
    rescue edge case where a previous run wrote the snapshot but crashed
    before persisting boss_state.json.

    Returns ``(spec_path, issue_url, issue_num, issue_title)``. ``issue_url``,
    ``issue_num``, and ``issue_title`` are empty strings for local-file inputs.
    """
    spec_dest = os.path.join(state_dir, "spec.md")

    if os.path.isfile(spec_dest) and os.path.getsize(spec_dest) > 0:
        # Rescue path: a previous run already wrote the snapshot. Recover any
        # issue_url / issue_num it persisted to state.json so the rescue
        # doesn't silently strip the issue link from boss_state.json. (We
        # avoid re-fetching from GitHub — the snapshot is authoritative.)
        logger.info("reusing existing spec snapshot: %s", spec_dest)
        recovered_url = ""
        recovered_num = ""
        try:
            state_data = load_json(os.path.join(state_dir, "state.json"))
            recovered_url = state_data.get("issue_url") or ""
            recovered_num = state_data.get("issue_num") or ""
        except Exception:
            pass
        return spec_dest, recovered_url, recovered_num, ""

    # Classify the input shape *before* shelling out to `gh repo view`. For a
    # typo like `--plan not-a-ref`, this lets us fail fast with a clear error
    # instead of forcing the caller to be inside a gh-recognized repo first.
    target_repo, issue_ref = parse_issue_ref(plan, "")

    if os.path.isfile(plan):
        try:
            if os.path.getsize(plan) == 0:
                die(f"--plan: file is empty: {plan}")
            shutil.copyfile(plan, spec_dest)
        except OSError as exc:
            die(f"--plan: failed to read/copy local plan file {plan!r}: {exc}")
        logger.info("plan source (file): %s -> %s", plan, spec_dest)
        return spec_dest, "", "", ""

    if target_repo is None and issue_ref is None:
        # parse_issue_ref returned (None, None) for the bare-number form
        # only because we passed an empty repo. Re-check by looking for the
        # bare-number shape directly so the error message is accurate.
        if not re.match(r"^#?[0-9]+$", plan):
            die(f"--plan: not a readable file or recognized issue reference: {plan}")

    if shutil.which("gh") is None:
        die(f"--plan: gh CLI not found; required to resolve issue reference {plan!r}")

    # Resolve the bare-number form against the current repo; cross-repo
    # forms (`owner/name#42` or full URLs) already carry their own repo.
    if target_repo is None or target_repo == "":
        try:
            target_repo = get_repo()
        except RuntimeError as exc:
            die(f"--plan: {exc}")
        # Re-parse against the resolved repo to populate issue_ref for the
        # bare-number form.
        target_repo, issue_ref = parse_issue_ref(plan, target_repo)
        if target_repo is None or issue_ref is None:
            die(f"--plan: not a readable file or recognized issue reference: {plan}")

    if issue_ref is None:
        die(f"--plan: not a readable file or recognized issue reference: {plan}")

    try:
        issue_data = view_issue(issue_ref, target_repo)
    except RuntimeError as exc:
        die(f"--plan: {exc}")

    body = issue_data.get("body") or ""
    if not body:
        die(f"--plan: issue {plan} has an empty body")

    issue_url = issue_data.get("url") or ""
    issue_num = str(issue_data.get("number") or "")
    issue_title = (issue_data.get("title") or "")[:60]

    with open(spec_dest, "w", encoding="utf-8") as f:
        f.write(body + "\n")
    # Persist the issue identifiers to state.json immediately so a crash
    # between this point and init_boss_state() doesn't strip them on rescue.
    patch_state(gr_id, issue_url=issue_url, issue_num=issue_num)
    logger.info(
        "plan source (issue %s#%s): %s -> %s",
        target_repo,
        issue_ref,
        issue_url,
        spec_dest,
    )
    return spec_dest, issue_url, issue_num, issue_title


def _maybe_set_description_from_spec(
    state_dir: str, *, gr_id: str | None = None, issue_title: str = ""
) -> None:
    """If state.json's description wasn't set explicitly, fill it from the
    issue title (when sourced from an issue ref) or the spec snapshot's H1.
    """
    state_file = os.path.join(state_dir, "state.json")
    if not os.path.isfile(state_file):
        return
    try:
        data = load_json(state_file)
    except Exception:
        return
    if data.get("description_explicit"):
        return
    if issue_title:
        patch_state(gr_id, description=issue_title[:60])
        return
    spec_file = os.path.join(state_dir, "spec.md")
    if not os.path.isfile(spec_file):
        return
    try:
        with open(spec_file, encoding="utf-8") as f:
            head_lines = f.read().splitlines()[:50]
    except Exception:
        return
    h1 = ""
    for line in head_lines:
        m = re.match(r"^#\s+(.+)", line)
        if m:
            h1 = m.group(1).strip()[:60]
            break
    if h1:
        patch_state(gr_id, description=h1)


def boss_main(
    argv: list[str], *, gr_id: str | None = None, client: ClaudeClient | None = None
) -> int:
    configure_logging()
    signal.signal(signal.SIGTERM, _sigterm_handler)
    args = _parse_boss_args(argv)
    if os.environ.get("GREMLINS_TEST_NOOP_PIPELINE"):
        return 0

    if not gr_id:
        die("gr_id is required (set by _run-pipeline)")
    state_dir = os.path.join(STATE_ROOT, gr_id)
    if not os.path.isdir(state_dir):
        die(f"state dir not found: {state_dir}")

    try:
        gremlin_state = load_json(os.path.join(state_dir, "state.json"))
    except Exception as exc:
        die(f"could not read state.json: {exc}")
    project_root = gremlin_state.get("project_root", "")
    if not project_root or not os.path.isdir(project_root):
        die(f"project_root not usable: {project_root!r}")
    boss_workdir = gremlin_state.get("workdir", "")

    if client is None:
        client = SubprocessClaudeClient()

    chain_kind = args.chain_kind
    launch_kind = {"local": "localgremlin", "gh": "ghgremlin"}[chain_kind]

    boss_state_file = os.path.join(state_dir, "boss_state.json")
    if not os.path.isfile(boss_state_file):
        # Chain start: snapshot --plan into the state dir. The handoff agent
        # only ever reads the snapshot, so a deleted-or-modified original
        # input cannot perturb later handoffs. For issue refs, the fetch
        # happens here (after launch.sh has detached) so a transient GitHub
        # outage is reported in the boss log instead of failing the launch.
        if args.test_cmd and chain_kind == "gh":
            die(
                "--test is not supported for --chain-kind gh "
                "(gh pipeline test integration is a separate plan)"
            )
        spec_path, issue_url, issue_num, issue_title = _resolve_plan_source(
            args.plan, state_dir, gr_id=gr_id
        )
        if issue_url:
            logger.info(
                "chain start: kind=%s, spec=%s, issue=%s",
                chain_kind,
                spec_path,
                issue_url,
            )
        else:
            logger.info("chain start: kind=%s, spec=%s", chain_kind, spec_path)
        _maybe_set_description_from_spec(
            state_dir, gr_id=gr_id, issue_title=issue_title
        )
        if chain_kind == "gh":
            # gh children open PRs from the repo's default branch and land
            # there, regardless of where the user happens to be. Anchor the
            # chain to origin/<default-branch> so handoff diffs land cleanly.
            target_branch = get_default_branch(project_root)
            chain_base_ref = get_remote_branch_sha(project_root, target_branch)
            logger.info(
                "base ref: %s (origin/%s), target branch: %s",
                chain_base_ref[:12],
                target_branch,
                target_branch,
            )
        else:
            chain_base_ref = get_head_ref(project_root)
            target_branch = get_current_branch(project_root)
            logger.info(
                "base ref: %s, target branch: %s",
                chain_base_ref[:12],
                target_branch or "(detached)",
            )
        boss_state = init_boss_state(
            spec_path=spec_path,
            chain_kind=chain_kind,
            chain_base_ref=chain_base_ref,
            target_branch=target_branch,
            state_dir=state_dir,
            issue_url=issue_url,
            issue_num=issue_num,
            test_cmd=args.test_cmd or "",
            test_max_attempts=args.test_max_attempts,
            test_fix_model=args.test_fix_model,
        )
        # Record initial boss HEAD so local children branch from the right commit.
        if chain_kind == "local" and boss_workdir and os.path.isdir(boss_workdir):
            initial_head = _git_mod.git_head_of_workdir(boss_workdir)
            if not initial_head:
                die(f"failed to resolve HEAD for boss workdir: {boss_workdir!r}")
            patch_state(gr_id, current_head=initial_head)
    else:
        boss_state = load_boss_state(state_dir)
        logger.info(
            "resuming chain: kind=%s, completed children: %d",
            chain_kind,
            len(boss_state["children"]),
        )

    # Main loop: handoff → launch → wait → land/rescue → repeat
    while True:
        check_stop()
        current_child_id = boss_state.get("current_child_id")

        # On fresh rescue with a bailed child, classify before deciding next action.
        # Verdict table: running → adopt child; landed-externally/abandoned → advance
        # to next handoff; no-decision → die to prevent duplicate child spawning.
        if current_child_id is None:
            last_bailed = _last_bailed_child(boss_state)
            if last_bailed is not None and _is_fresh_rescue(state_dir):
                verdict = _classify_from_child_state(last_bailed["id"])
                if verdict == "running":
                    current_child_id = last_bailed["id"]
                    boss_state["current_child_id"] = current_child_id
                    boss_state["children"] = [
                        c
                        for c in boss_state["children"]
                        if c["id"] != last_bailed["id"]
                    ]
                    save_boss_state(state_dir, boss_state)
                elif verdict == "landed":
                    last_bailed["outcome"] = "landed"
                    save_boss_state(state_dir, boss_state)
                    continue
                elif verdict == "landed-externally":
                    last_bailed["outcome"] = "landed-externally"
                    save_boss_state(state_dir, boss_state)
                    continue
                elif verdict == "abandoned":
                    last_bailed["outcome"] = "abandoned"
                    save_boss_state(state_dir, boss_state)
                    continue
                else:
                    die(_format_no_decision_message(last_bailed["id"]))

        if current_child_id is None:
            # Step 1: run handoff to decide what to do next
            exit_state, sig = run_handoff(
                client=client,
                gr_id=gr_id,
                state_dir=state_dir,
                boss_state=boss_state,
                project_root=project_root,
                boss_workdir=boss_workdir,
                model=args.model,
            )
            save_boss_state(state_dir, boss_state)
            check_stop()

            if exit_state == "chain-done":
                logger.info("chain complete")
                followups: list[Any] = boss_state.get("operator_followups") or []
                if followups:
                    logger.info(
                        "operator follow-ups (%d) — owed by the human between phase landings:",
                        len(followups),
                    )
                    for item in followups:
                        logger.info("  - %s", item)
                else:
                    logger.info("operator follow-ups: (none)")
                set_stage(gr_id, "done")
                save_boss_state(state_dir, boss_state)
                return 0

            if exit_state == "bail":
                reason = sig.get("reason") or "(no reason given)"
                logger.info("handoff bailed: %s", reason)
                save_boss_state(state_dir, boss_state)
                die(f"chain halted by handoff: {reason}")

            # next-plan: launch the next child
            child_plan = sig.get("child_plan")
            if not child_plan or not os.path.isfile(child_plan):
                die(
                    f"handoff returned next-plan but child_plan not found: {child_plan!r}"
                )

            check_stop()
            current_child_id = launch_child(gr_id, launch_kind, child_plan)
            boss_state["current_child_id"] = current_child_id
            save_boss_state(state_dir, boss_state)
            # Stop the freshly launched child if a stop was requested during
            # or just after launch (pre-wait window).
            if _stop_requested:
                logger.info(
                    "stop requested — stopping newly launched child %s",
                    current_child_id,
                )
                subprocess.run(
                    _gremlins_cli_cmd("stop", current_child_id),
                    capture_output=True,
                    env=_gremlins_cli_env(),
                )
                sys.exit(130)
            check_stop()

        else:
            # Resume path: already have a child in flight
            logger.info("resuming with in-flight child: %s", current_child_id)
            if child_is_closed(current_child_id):
                # `closed` is a UI hide flag, not a success/failure signal.
                # Inspect the finished marker and exit_code to determine outcome.
                child_wdir = os.path.join(STATE_ROOT, current_child_id)
                finished_path = os.path.join(child_wdir, "finished")
                state_path = os.path.join(child_wdir, "state.json")
                if os.path.isfile(finished_path) and os.path.isfile(state_path):
                    try:
                        child_state = load_json(state_path)
                        child_succeeded = child_state.get("exit_code") == 0
                    except Exception:
                        child_succeeded = False
                else:
                    child_succeeded = False

                if child_succeeded:
                    # Already finished successfully and closed — treat as landed
                    logger.info(
                        "child %s already finished and closed — treating as landed",
                        current_child_id,
                    )
                    boss_state["children"].append(
                        {
                            "id": current_child_id,
                            "outcome": "landed",
                        }
                    )
                    boss_state["current_child_id"] = None
                    save_boss_state(state_dir, boss_state)
                    continue
                else:
                    # Closed but not successfully finished — operator may have
                    # manually hidden a failed child.  Halt for operator action.
                    logger.info(
                        "child %s is closed but did not finish successfully — operator action required",
                        current_child_id,
                    )
                    boss_state["current_child_id"] = None
                    save_boss_state(state_dir, boss_state)
                    die(
                        f"chain halted: child {current_child_id} was manually closed without"
                        f" successfully finishing — inspect and resume or reassign"
                    )

        # Step 2: inner loop — wait → land → (rescue → wait → land)* → bail
        was_rescued = False
        while True:
            check_stop()
            child_wdir = os.path.join(STATE_ROOT, current_child_id)

            if not os.path.isfile(os.path.join(child_wdir, "finished")):
                set_stage(gr_id, "waiting")
                success = wait_for_child(current_child_id, gr_id)
            else:
                try:
                    child_state = load_json(os.path.join(child_wdir, "state.json"))
                    success = child_state.get("exit_code") == 0
                except Exception:
                    success = False

            check_stop()

            if success:
                set_stage(gr_id, "landing")
                into_dir = ""
                if chain_kind == "local":
                    if not boss_workdir or not os.path.isdir(boss_workdir):
                        die(f"boss workdir not usable for local land: {boss_workdir!r}")
                    into_dir = boss_workdir
                if land_child(current_child_id, into_dir=into_dir):
                    if (
                        chain_kind == "local"
                        and boss_workdir
                        and os.path.isdir(boss_workdir)
                    ):
                        new_head = _git_mod.git_head_of_workdir(boss_workdir)
                        if not new_head:
                            die(
                                f"child {current_child_id} landed locally, but could not resolve "
                                f"HEAD for boss workdir {boss_workdir!r}; refusing to continue "
                                f"with a stale current_head."
                            )
                        patch_state(gr_id, current_head=new_head)
                    outcome = "rescued-then-landed" if was_rescued else "landed"
                    logger.info("child %s %s", current_child_id, outcome)
                    boss_state["children"].append(
                        {"id": current_child_id, "outcome": outcome}
                    )
                    boss_state["current_child_id"] = None
                    save_boss_state(state_dir, boss_state)
                    break  # inner loop done; outer loop continues to next handoff
                else:
                    # The pipeline succeeded but land itself failed (e.g. merge
                    # conflict, branch protection rejection, squash conflict).
                    logger.info(
                        "landing failed for %s — operator action required",
                        current_child_id,
                    )
                    boss_state["children"].append(
                        {
                            "id": current_child_id,
                            "outcome": "land-failed",
                        }
                    )
                    boss_state["current_child_id"] = None
                    save_boss_state(state_dir, boss_state)
                    die(
                        f"chain halted: child {current_child_id} pipeline succeeded but"
                        f" land failed (merge conflict or branch protection?) —"
                        f" resolve manually, then resume the boss"
                    )

            # Pipeline failure → rescue
            set_stage(gr_id, "rescuing")
            if not rescue_child(current_child_id):
                bail_reason = get_child_bail_reason(current_child_id)
                bail_detail = _summarize_for_log(
                    get_child_bail_detail(current_child_id)
                )
                # `structural` is distinct from `unsalvageable`: the agent
                # recognized a real bug in the pipeline source or a sibling
                # artifact that the chain can be salvaged from with a human
                # edit, but the agent isn't the right actor.
                if bail_reason == "structural":
                    logger.info(
                        "child %s bailed: STRUCTURAL — pipeline/sibling-artifact bug, human edit required",
                        current_child_id,
                    )
                    if bail_detail:
                        logger.info("  diagnosis: %s", bail_detail)
                elif bail_reason == "unsalvageable":
                    logger.info(
                        "child %s bailed: UNSALVAGEABLE — run cannot be recovered (giving up)",
                        current_child_id,
                    )
                    if bail_detail:
                        logger.info("  detail: %s", bail_detail)
                else:
                    # Other headless-rescue bail reasons also populate bail_detail.
                    logger.info(
                        "rescue refused for %s (%s)",
                        current_child_id,
                        bail_reason or "no bail_reason",
                    )
                    if bail_detail:
                        logger.info("  detail: %s", bail_detail)
                boss_state["children"].append(
                    {
                        "id": current_child_id,
                        "outcome": f"bailed:{bail_reason}" if bail_reason else "bailed",
                    }
                )
                boss_state["current_child_id"] = None
                save_boss_state(state_dir, boss_state)
                die(
                    f"chain halted: child {current_child_id} failed and rescue was refused"
                )

            was_rescued = True
            logger.info("rescue relaunched %s — waiting again", current_child_id)
