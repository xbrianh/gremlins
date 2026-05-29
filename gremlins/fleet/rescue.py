"""Rescue subcommand: diagnose and resume a dead gremlin."""

import datetime
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
from typing import IO, Any, cast

from gremlins import paths as _paths
from gremlins.clients.stream import stream_events
from gremlins.executor.state import StateData
from gremlins.fleet.constants import (
    EXCLUDED_BAIL_CLASSES,
    HEADLESS_DIAGNOSIS_TIMEOUT_SECS,
    RESCUE_CAP,
)
from gremlins.fleet.resolve import (
    resolve_gremlin,
    stage_names_for_gremlin,
)
from gremlins.fleet.state import (
    atomic_patch_state,
    liveness_of_state_file,
    load_state,
)
from gremlins.fleet.stop import do_stop
from gremlins.launcher import GremlinAlreadyRunning
from gremlins.launcher import resume as _resume
from gremlins.utils import proc

_atomic_patch_state = atomic_patch_state


def build_rescue_prompt(
    state: dict[str, Any],
    log_tail: str,
    state_file_path: str,
    log_file_path: str,
    marker_path: str,
):
    """Build the diagnosis-step prompt. The marker contract is the same in
    interactive and headless modes — the agent never knows the difference and
    the wrapper reads the marker to decide whether to invoke the relaunch step.
    """
    pipeline_path = str(state.get("pipeline_path") or "")
    pipeline_name = (
        os.path.basename(pipeline_path).replace(".yaml", "")
        if pipeline_path
        else str(state.get("kind") or "unknown")
    )
    stage = state.get("stage") or "unknown"
    description = state.get("description") or ""
    project_root = state.get("project_root") or ""
    workdir = state.get("workdir") or ""
    parent_id = state.get("parent_id") or ""

    stages = stage_names_for_gremlin(state)

    log_tail_safe = log_tail.replace("```", "` ` `")

    pipeline_paths = [
        (
            "~/.claude/gremlins/",
            "orchestrators, stages, state helpers, launcher — drives all gremlin kinds",
        ),
    ]
    parent_state_dir = (
        os.path.join(str(_paths.state_root()), parent_id) if parent_id else ""
    )

    context_lines = [
        "## Inspect before deciding",
        "",
        "The log above is a starting point, not the whole story. Read whichever of",
        "these are relevant before forming a verdict — narrow context is the most",
        "common cause of a wrong call:",
        "",
        "- **Pipeline source actually executing** (read-only — see `fixed` rules below):",
    ]
    for path, purpose in pipeline_paths:
        context_lines.append(f"    - `{path}` ({purpose})")
    context_lines += [
        "  Bugs in the pipeline often surface as confusing failure logs in the",
        "  worktree. Look here when the failure smells like a code-level issue in",
        "  the orchestrator rather than a problem with the gremlin's own state.",
        "",
        "- **Worktree** (the failed gremlin's checkout, may lag behind the default branch):",
    ]
    if workdir:
        context_lines.append(
            f"    - `{workdir}` — the gremlin runs here at the chain's base ref."
        )
    else:
        context_lines.append("    - (no workdir recorded in state — skip this section)")
    pr_quoted = f'"{project_root}"' if project_root else "<project_root>"
    context_lines += [
        "",
        "- **Product repo at current default branch** (catches fixes that landed",
        "  after the chain started — the worktree's base ref will not see them):",
        f"    - `git -C {pr_quoted} log -20 origin/HEAD`",
        f'    - `git -C {pr_quoted} diff "$(git -C {pr_quoted} merge-base HEAD origin/HEAD)"..origin/HEAD`',
        "  Use this when you suspect the failure was already addressed elsewhere.",
    ]
    if parent_state_dir:
        context_lines += [
            "",
            f"- **Parent boss state dir** (this gremlin is a child of boss `{parent_id}`):",
            f"    - `{parent_state_dir}/boss_state.json` — chain progress, prior children",
            f"    - `{parent_state_dir}/artifacts/rolling-plan.md` — rolling plan document",
            f"    - `{parent_state_dir}/artifacts/child-plan.md` — child plan handed to current gremlin (the",
            "      plan handed to *this* child may itself be the source of the failure)",
            f"    - `{parent_state_dir}/log` — boss lifecycle events",
        ]
    context_lines.append("")
    context_block = "\n".join(context_lines)

    return f"""You are diagnosing a failed background gremlin so it can resume.

## Gremlin context

Pipeline: {pipeline_name}
Description: {description}
Failed at stage: {stage}
Stage order for {pipeline_name}: {" → ".join(stages)}
State dir: {os.path.dirname(state_file_path)}
Worktree: {workdir or "(unknown)"}
Project root: {project_root or "(unknown)"}
Parent (boss) id: {parent_id or "(none)"}

## Failure log (last ~200 lines)

(Log tail also written to: {log_file_path})

```
{log_tail_safe}
```

{context_block}

## What to do

1. Diagnose the failure. Read the relevant items from the **Inspect before deciding**
   list above; do not skip the pipeline-source / current-default-branch / parent-state
   checks when the log alone does not make the root cause obvious.
2. Decide which verdict applies. The bar for each one is strict — picking the wrong
   verdict means the chain either resumes into the same failure or halts when it
   could have continued.
   - **fixed**: you edited `state.json` (at `{state_file_path}`) or files inside
     the gremlin's worktree to resolve the failure; rerunning stage {stage} should
     now succeed. **Do NOT edit pipeline source under `~/.claude/gremlins/`** —
     those paths live outside the gremlin's worktree, so edits there cannot land
     in the PR diff, and may additionally be overwritten by future syncs from an
     upstream source repo. If the fix lives in pipeline source, choose
     `structural`.
   - **transient**: the failure was a flake (network, tool timeout, retriable
     infra) OR a fix has already landed elsewhere (e.g. in `main`, in a
     `~/.claude/gremlins/` file outside the gremlin's worktree) that the chain's
     pre-fix base ref doesn't see. No change needed; rerunning the same stage
     as-is should succeed.
   - **structural**: the failure points at a real bug in the pipeline source
     (`~/.claude/gremlins/`) or in a sibling artifact (e.g. a malformed child plan
     under the parent boss's state dir) that you recognize but cannot fix here.
     Use this when the remedy is "edit the pipeline" or "edit the child plan",
     not "tweak `state.json`". Articulate *what* in the pipeline / plan is wrong
     and *why* a fix elsewhere is required — the operator will read your summary.
   - **unsalvageable**: reserved for genuinely unrecoverable states — corrupted
     state dir, missing worktree, conflicting git state with no clean rewind.
     This should be rare. If the fix isn't a `state.json` or worktree edit you
     can make here, you almost certainly want `structural` instead.
3. Write a marker file to **exactly** this path:

       {marker_path}

   The marker MUST be a single JSON object with these fields:
   - `"status"`: one of `"fixed"`, `"transient"`, `"structural"`, or `"unsalvageable"`.
   - `"summary"` (optional, but strongly recommended for `structural` and
     `unsalvageable`): a one-line string explaining your decision. For
     `structural`, name the file/function and the bug.

   Example:

   ```json
   {{"status": "fixed", "summary": "removed bad --model flag from pipeline_args in state.json"}}
   ```

4. Stop. Do NOT re-run the failed stage or any remaining stages yourself — the
   wrapper reads the marker and (on `fixed`/`transient`) hands off to a
   background resume that relaunches the pipeline starting at {stage}. On
   `structural` or `unsalvageable`, the wrapper writes a bail reason and the
   gremlin stays terminal.

Constraints:
- Do NOT prompt for input. (Headless runs have no TTY at all; interactive runs share the operator's terminal but the agent must still complete autonomously without asking the operator to type.)
- Do NOT call `exit` or otherwise abort — finish normally so the wrapper can read the marker.
- If you do not write the marker file, the wrapper will treat that as a hard error and bail with `diagnosis_no_marker`.
- Permitted edits: `state.json` at `{state_file_path}`{(", files inside the worktree at `" + workdir + "`") if workdir else ""}, the marker file at
  `{marker_path}` (you MUST write this — that's how the wrapper reads your
  verdict), and your scratch working directory.
  Pipeline source under `~/.claude/gremlins/` is read-only for this rescue —
  surface bugs there via `structural`.
"""


def _write_bail(sf: str, wdir: str, bail_reason: str, bail_detail: str = "") -> None:
    """Mark a gremlin as bailed by headless rescue.

    Writes bail_reason/bail_detail/status/exit_code/ended_at into state.json
    and touches the `finished` marker so liveness classifies the gremlin as
    terminal (`dead:bailed:<reason>`). Best-effort throughout — failing to
    write either piece leaves the gremlin in its prior state, which is no
    worse than before headless rescue ran.
    """
    now_iso = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
    atomic_patch_state(
        sf,
        {
            "bail_reason": bail_reason,
            "bail_detail": bail_detail,
            "status": "bailed",
            "exit_code": 2,
            "ended_at": now_iso,
        },
    )
    try:
        pathlib.Path(os.path.join(wdir, "finished")).touch()
    except OSError:
        pass


def write_rescue_report(wdir: str, report: dict[str, Any]) -> None:
    """Write a Markdown rescue report to <wdir>/rescue-<UTC-ts>-<pid>.md.

    Best-effort: never raises (same "never break a session" principle as the
    rest of the file). Lives in the state dir root alongside `log` /
    `state.json` / `finished` so it sits with the operational record rather
    than mixed with plan/review outputs in `artifacts/`.

    Expected keys in `report`:
      - state: dict (gremlin state at the start of the attempt)
      - attempt_number: int (rescue_count + 1; pre-increment, since
        launch.sh --resume bumps the counter on relaunch — not do_rescue)
      - headless: bool
      - verdict: str (agent verdict, diagnosis-step failure mode, or wrapper refusal)
      - summary: str (agent summary or wrapper-generated diagnostic)
      - relaunch_attempted: bool (True iff a relaunch subprocess was actually
        invoked; preflight failures like "launcher not executable" leave this
        False even when relaunch_outcome is 'failed')
      - relaunch_outcome: str ('success' / 'failed' / 'skipped')
      - relaunch_reason: str (optional explanation for failed/skipped paths)
    """
    try:
        # Match the marker_path uniqueness pattern (~290 lines below): include
        # microseconds and PID so two rescue attempts within the same UTC
        # second — wrapper-refusal paths return in well under a second, so a
        # scripted retry loop or operator-driven back-to-back invocation can
        # land in the same %S — don't silently overwrite each other.
        ts = datetime.datetime.now(datetime.UTC).strftime("%Y%m%dT%H%M%S_%fZ")
        path = os.path.join(wdir, f"rescue-{ts}-{os.getpid()}.md")
        state: dict[str, Any] = report.get("state") or {}
        gremlin_id = state.get("id") or os.path.basename(wdir)
        kind = state.get("kind") or ""
        description = state.get("description") or ""
        stage = state.get("stage") or ""
        client = state.get("client") or ""
        parent_id = state.get("parent_id") or ""
        attempt = int(report.get("attempt_number") or 0)
        # rescue_count is normalized to a non-negative int upstream (see
        # do_rescue), so attempt >= 1 and prior >= 0 by construction.
        prior = attempt - 1
        headless = bool(report.get("headless"))
        verdict = report.get("verdict") or ""
        summary = report.get("summary") or ""
        relaunch_outcome = report.get("relaunch_outcome") or ""
        relaunch_reason = report.get("relaunch_reason") or ""
        ts_human = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")

        # Prefer the explicit boolean recorded by do_rescue (set only when
        # subprocess.run is actually invoked). Fall back to inferring from
        # outcome if absent — older callers / unexpected report shapes.
        if "relaunch_attempted" in report:
            attempted = "yes" if bool(report.get("relaunch_attempted")) else "no"
        else:
            attempted = "yes" if relaunch_outcome in ("success", "failed") else "no"

        lines = [
            "# Rescue Attempt",
            "",
            f"- Timestamp: {ts_human}",
            f"- Attempt: #{attempt} (pre-increment of rescue_count={prior}; the launcher bumps the counter on relaunch)",
            f"- Mode: {'headless' if headless else 'interactive'}",
            "",
            "## Gremlin Context",
            "",
            f"- ID: {gremlin_id}",
            f"- Kind: {kind or '(unknown)'}",
            f"- Description: {description or '(none)'}",
            f"- Failed stage: {stage or '(unknown)'}",
        ]
        if client:
            lines.append(f"- Client: {client}")
        if parent_id:
            lines.append(f"- Parent (boss) id: {parent_id}")
        lines += [
            "",
            "## Diagnosis",
            "",
            f"- Verdict: {verdict or '(none)'}",
            f"- Summary: {summary or '(none)'}",
            "",
            "## Relaunch",
            "",
            f"- Attempted: {attempted}",
            f"- Outcome: {relaunch_outcome or '(unknown)'}",
        ]
        if relaunch_reason:
            lines.append(f"- Reason: {relaunch_reason}")
        lines.append("")
        with open(path, "w", encoding="utf-8") as fh:
            fh.write("\n".join(lines))
    except Exception:
        pass


def _read_rescue_marker(marker_path: str):
    """Read and validate the diagnosis marker file. Returns (status, msg).

    status ∈ {"fixed", "transient", "structural", "unsalvageable"} → agent verdict
    status ∈ {"no_marker", "bad_marker"} → protocol violation by the agent

    msg carries the agent's summary on the verdict statuses and a
    wrapper-generated diagnostic on the failure ones.
    """
    if not os.path.isfile(marker_path):
        return "no_marker", f"agent did not write marker file at {marker_path}"

    try:
        with open(marker_path, encoding="utf-8") as fh:
            marker = json.load(fh)
    except Exception as exc:
        return "bad_marker", f"marker file unreadable: {exc}"

    if not isinstance(marker, dict):
        return "bad_marker", "marker file is not a JSON object"

    marker_obj: dict[str, Any] = cast(dict[str, Any], marker)
    status = marker_obj.get("status")
    raw_summary = marker_obj.get("summary", "")
    # Normalize summary: must be a string. We persist it into state.json as
    # bail_detail and print it in logs (plain text, single line for boss
    # log readability), so reject objects/arrays and collapse whitespace
    # rather than letting a chatty agent inject newlines into our logs.
    if raw_summary is None:
        summary = ""
    elif isinstance(raw_summary, str):
        summary = " ".join(raw_summary.split()).strip()
        # Cap to a sane length so a runaway summary can't blow up logs.
        if len(summary) > 500:
            summary = summary[:497] + "..."
    else:
        return (
            "bad_marker",
            f"marker summary must be a string, got {type(raw_summary).__name__}",
        )

    if status not in ("fixed", "transient", "structural", "unsalvageable"):
        return "bad_marker", f"marker has invalid status: {status!r}"

    # Provide a wrapper-generated fallback only for the bail-shaped verdicts —
    # operators see this in the printed bail line. `fixed`/`transient` keep an
    # empty summary so the success path can suppress the parenthetical when
    # the agent didn't bother to write one (avoids double-printing).
    if status == "structural":
        return status, summary or "agent declared failure structural"
    if status == "unsalvageable":
        return status, summary or "agent declared failure unsalvageable"
    return status, summary


def _run_headless_diagnosis(workdir: str, prompt: str, marker_path: str):
    """Run the diagnosis step non-interactively. Returns (status, error_msg).

    status ∈ {"fixed", "transient", "structural", "unsalvageable"} →
        handled by caller
    status ∈ {"timeout", "claude_exit", "no_marker", "bad_marker"} →
        diagnosis-step failure modes that should write a bail_reason.

    error_msg is empty for the success-shaped statuses ("fixed",
    "transient") and populated for the bail-shaped ones (including
    "structural" and "unsalvageable", which carry the agent's summary
    if provided).
    """
    env = os.environ.copy()
    # Same rationale as _run_claude_p_text below — keep the session-summary
    # hook from prepending its block to the agent's output, which would
    # otherwise corrupt anything the agent prints to its final reply (we
    # don't actually read the reply here, but the hook also slows things
    # down materially when scanning state).
    env["GREMLIN_SKIP_SUMMARY"] = "1"
    cmd = [
        "claude",
        "-p",
        "--permission-mode",
        "bypassPermissions",
        "--output-format",
        "text",
    ]
    try:
        # Discard stdout — the agent's reply can be large and we don't read
        # it (results come from the marker file, not the process output).
        # Keep stderr for the failure-path snippet.
        result = subprocess.run(
            cmd,
            cwd=workdir,
            input=prompt,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            timeout=HEADLESS_DIAGNOSIS_TIMEOUT_SECS,
            env=env,
        )
    except subprocess.TimeoutExpired:
        return "timeout", f"claude -p exceeded {HEADLESS_DIAGNOSIS_TIMEOUT_SECS}s"
    except FileNotFoundError:
        return "claude_exit", "'claude' CLI not found in PATH"

    if result.returncode != 0:
        stderr_snip = (result.stderr or "").strip().splitlines()[-1:] or [""]
        return "claude_exit", f"claude -p exited {result.returncode}: {stderr_snip[0]}"

    return _read_rescue_marker(marker_path)


def recreate_worktree(state: dict[str, Any]) -> tuple[bool, str]:
    """Attempt to recreate a missing worktree from the gremlin's branch or base ref.

    Tries the named branch first (preserves in-progress commits for localgremlin),
    then falls back to a detached checkout of worktree_base or HEAD.
    Returns (success: bool, detail: str).
    """
    workdir = state.get("workdir") or ""
    gremlin_id_val = str(state.get("id") or "")
    branch = StateData.load(gremlin_id_val).last_artifact_branch()
    worktree_base = state.get("worktree_base") or ""
    project_root = state.get("project_root") or ""

    if not workdir or not project_root:
        return False, "workdir or project_root not recorded in state"
    if not os.path.isdir(project_root):
        return False, f"project_root {project_root!r} does not exist"

    # Prune stale worktree entries so that git worktree add doesn't fail
    # with "is already registered" when the directory was deleted by the host.
    proc.run_quiet(["git", "worktree", "prune"], cwd=project_root)

    if branch:
        r = proc.run(["git", "worktree", "add", workdir, branch], cwd=project_root)
        if r.returncode == 0:
            return True, f"recreated from branch {branch!r}"
        branch_err = r.stderr.strip()
    else:
        branch_err = ""

    ref = worktree_base or "HEAD"
    r = proc.run(["git", "worktree", "add", "--detach", workdir, ref], cwd=project_root)
    if r.returncode == 0:
        suffix = f" (branch {branch!r} was gone)" if branch_err else ""
        return True, f"recreated detached at {ref!r}{suffix}"
    fallback_err = r.stderr.strip()

    if branch_err:
        return (
            False,
            f"branch {branch!r}: {branch_err}; fallback {ref!r}: {fallback_err}",
        )
    return False, f"worktree add --detach {ref!r}: {fallback_err}"


def do_rescue(target: str, headless: bool = False, from_boss: bool = False) -> bool:
    match = resolve_gremlin(target)
    if match is None:
        return False

    gremlin_id, sf, wdir = match
    state = load_state(sf)
    if not state:
        print(f"error: could not read state for {gremlin_id}")
        return False

    live = liveness_of_state_file(sf, state)

    if live == "running":
        print(f"gremlin {gremlin_id} is still running — use 'stop' first, then rescue")
        return False
    if live == "finished":
        print(f"gremlin {gremlin_id} finished successfully — nothing to rescue")
        return False
    if live.startswith("stalled:"):
        print(
            f"gremlin {gremlin_id} is stalled but its process is still alive — stopping it first..."
        )
        if not do_stop(target):
            print("error: could not stop the stalled gremlin — aborting rescue")
            return False
        # Reload state — do_stop wrote ended_at / status / exit_code via the
        # finished-marker fallback, and we want the fresh values for the
        # bail-class and rescue_count checks below.
        state = load_state(sf) or state
        live = liveness_of_state_file(sf, state)

    workdir = state.get("workdir")
    if not workdir:
        print(f"error: no workdir recorded in state for {gremlin_id} — cannot rescue")
        return False

    if live == "dead:host-terminated":
        project_root_check: str = str(state.get("project_root") or "")
        print(
            f"gremlin {gremlin_id}: worktree is gone (host likely terminated externally)"
        )
        if not project_root_check or not os.path.isdir(project_root_check):
            detail = (
                f"project_root {project_root_check!r} is also gone — "
                "worktree cannot be recreated; use 'gremlins rm' to clean up"
            )
            print(f"  {detail}")
            if headless:
                _write_bail(sf, wdir, "host_terminated_unrecoverable", detail)
            return False
        print(f"  attempting to recreate worktree at {workdir}...")
        recreated, detail = recreate_worktree(state)
        if not recreated:
            msg = f"worktree recreation failed: {detail}"
            print(f"  {msg}")
            print(f"  use 'gremlins rm {gremlin_id}' to clean up")
            if headless:
                _write_bail(sf, wdir, "host_terminated_unrecoverable", msg)
            return False
        print(f"  worktree recreated: {detail}; resuming rescue...")
        live = liveness_of_state_file(sf, state)

    rescue_count_raw: Any = state.get("rescue_count") or 0
    try:
        rescue_count = int(rescue_count_raw)
    except (ValueError, TypeError):
        rescue_count = 0

    _gremlin_id_for_bail = str(state.get("id") or "")
    _bail_file_data = (
        StateData.load(_gremlin_id_for_bail).read_bail_info()
        if _gremlin_id_for_bail
        else None
    )
    bail_class = (
        (_bail_file_data.get("class") or "")
        if _bail_file_data
        else (state.get("bail_class") or "")
    )

    # Build a mutable report context so each terminal path can populate the
    # diagnosis/relaunch fields and the finally block emits a single Markdown
    # report into <wdir>/rescue-<UTC-ts>.md. Defaults assume "skipped" so any
    # unexpected exit produces an interpretable record.
    report = {
        "state": state,
        "attempt_number": rescue_count + 1,
        "headless": headless,
        "verdict": "",
        "summary": "",
        # relaunch_attempted is flipped to True only when subprocess.run is
        # actually invoked below — preflight refusals (launcher missing /
        # not executable, exec failure) keep it False even though they set
        # relaunch_outcome='failed'. This lets the report distinguish "we
        # tried and the relaunch failed" from "we never made it to a relaunch".
        "relaunch_attempted": False,
        "relaunch_outcome": "skipped",
        "relaunch_reason": "",
    }
    aborted = [False]

    try:
        # Headless: hard-refuse on excluded class or exhausted attempts. Both
        # paths write a fresh bail_reason (overwriting any prior one) so the
        # most recent decision is what /gremlins listings show.
        # --from-boss bypasses the excluded-class short-circuit so the boss's
        # own classification logic (not the rescue agent) decides next action.
        if headless:
            if bail_class in EXCLUDED_BAIL_CLASSES and not from_boss:
                reason = f"excluded_class:{bail_class}"
                detail: str = str(
                    (_bail_file_data.get("detail") if _bail_file_data else None)
                    or state.get("bail_detail")
                    or f"upstream stage bailed with bail_class={bail_class}"
                )
                _write_bail(sf, wdir, reason, detail)
                print(f"headless rescue refused: {reason}")
                report["verdict"] = reason
                report["summary"] = detail
                return False
            if rescue_count >= RESCUE_CAP:
                reason = "attempts_exhausted"
                detail = f"rescue_count={rescue_count} reached cap of {RESCUE_CAP}"
                _write_bail(sf, wdir, reason, detail)
                print(f"headless rescue refused: {reason}")
                report["verdict"] = reason
                report["summary"] = detail
                return False
        else:
            # Interactive: warn but let the human override. The cap is
            # primarily a guardrail for autonomous callers; a person watching
            # the diagnosis step can decide for themselves whether attempt #4 is worth it.
            if rescue_count >= RESCUE_CAP:
                print(
                    f"warning: gremlin has been rescued {rescue_count} times "
                    f"(cap is {RESCUE_CAP}); proceeding because this is interactive — "
                    f"Ctrl-C to abort."
                )

        stage = state.get("stage") or "unknown"

        log_path = os.path.join(wdir, "log")
        log_tail = ""
        if os.path.isfile(log_path):
            try:
                with open(log_path, encoding="utf-8", errors="replace") as fh:
                    lines = fh.readlines()
                    log_tail = "".join(lines[-200:])
            except OSError:
                log_tail = "(could not read log)"

        # Marker file is the contract between the agent and this wrapper for
        # both interactive and headless modes. Pre-create the artifacts dir so
        # the agent doesn't need to mkdir it themselves — one less thing to
        # get wrong. If we can't make/use it, bail with a wrapper-specific
        # reason rather than letting the diagnosis step run and inevitably trip
        # diagnosis_no_marker (which would incorrectly attribute the failure to
        # the agent).
        artifacts_dir = os.path.join(wdir, "artifacts")
        artifacts_dir_error = None
        try:
            os.makedirs(artifacts_dir, exist_ok=True)
        except OSError as exc:
            artifacts_dir_error = exc
        if not os.path.isdir(artifacts_dir) or not os.access(
            artifacts_dir, os.W_OK | os.X_OK
        ):
            reason = "wrapper_artifacts_dir_unavailable"
            if artifacts_dir_error is not None:
                detail = (
                    f"could not prepare artifacts dir {artifacts_dir!r}: "
                    f"{artifacts_dir_error}"
                )
            elif not os.path.isdir(artifacts_dir):
                detail = f"artifacts dir {artifacts_dir!r} does not exist after setup"
            else:
                detail = f"artifacts dir {artifacts_dir!r} is not writable"
            if headless:
                _write_bail(sf, wdir, reason, detail)
                print(f"headless rescue refused: {reason}: {detail}")
            else:
                print(f"rescue refused: {reason}: {detail}")
            report["verdict"] = reason
            report["summary"] = detail
            return False
        # Include microseconds and PID so two rescue attempts in the same UTC
        # second (e.g. operator hits Ctrl-C and immediately reruns) don't collide
        # on the same marker_path — without uniqueness, the new run could read a
        # stale marker from the prior attempt and treat it as the new agent's
        # output.
        ts = datetime.datetime.now(datetime.UTC).strftime("%Y%m%dT%H%M%S_%fZ")
        marker_path = os.path.join(artifacts_dir, f"rescue-{ts}-{os.getpid()}.done")
        # Best-effort unlink: even though the timestamp is unique per run, removing
        # any pre-existing marker at this path is cheap insurance against weird
        # filesystem clock edge cases.
        try:
            os.unlink(marker_path)
        except OSError:
            pass

        # Create a scratch dir so the diagnosis step runs isolated from the worktree.
        # The log tail is written there as a readable file; state.json is passed
        # by absolute path as the only file the agent may edit.
        # mkdtemp is inside the try so any subsequent exception triggers cleanup.
        scratch_dir = None
        try:
            scratch_dir = tempfile.mkdtemp(prefix="gremlin-rescue-")
            scratch_log = os.path.join(scratch_dir, "gremlin.log")
            try:
                with open(scratch_log, "w", encoding="utf-8") as fh:
                    fh.write(log_tail)
            except OSError:
                scratch_log = log_path  # fallback: reference the original log path

            prompt = build_rescue_prompt(state, log_tail, sf, scratch_log, marker_path)

            print(f"Rescuing gremlin {gremlin_id} (stage: {stage}, liveness: {live})")
            print(f"Gremlin workdir: {workdir}")
            print(f"Agent scratch dir: {scratch_dir}")

            if headless:
                print(
                    f"Diagnosis step (headless): running diagnosis agent "
                    f"(timeout: {HEADLESS_DIAGNOSIS_TIMEOUT_SECS}s, marker: {marker_path})..."
                )
                status, err_msg = _run_headless_diagnosis(
                    scratch_dir, prompt, marker_path
                )

                if status == "timeout":
                    _write_bail(sf, wdir, "diagnosis_timeout", err_msg)
                    print(f"Diagnosis step timed out: {err_msg}")
                    report["verdict"] = "diagnosis_timeout"
                    report["summary"] = err_msg
                    return False
                if status == "claude_exit":
                    _write_bail(sf, wdir, "diagnosis_claude_error", err_msg)
                    print(f"Diagnosis step claude error: {err_msg}")
                    report["verdict"] = "diagnosis_claude_error"
                    report["summary"] = err_msg
                    return False
                if status == "no_marker":
                    _write_bail(sf, wdir, "diagnosis_no_marker", err_msg)
                    print(f"Diagnosis step produced no marker file: {err_msg}")
                    report["verdict"] = "diagnosis_no_marker"
                    report["summary"] = err_msg
                    return False
                if status == "bad_marker":
                    _write_bail(sf, wdir, "diagnosis_bad_marker", err_msg)
                    print(f"Diagnosis step marker file invalid: {err_msg}")
                    report["verdict"] = "diagnosis_bad_marker"
                    report["summary"] = err_msg
                    return False
                if status == "structural":
                    _write_bail(sf, wdir, "structural", err_msg)
                    print(
                        f"Diagnosis step: agent flagged a structural problem in the pipeline "
                        f"or sibling artifacts that requires a human edit ({err_msg})"
                    )
                    report["verdict"] = "structural"
                    report["summary"] = err_msg
                    return False
                if status == "unsalvageable":
                    _write_bail(sf, wdir, "unsalvageable", err_msg)
                    if err_msg:
                        print(
                            f"Diagnosis step: agent declared the failure unsalvageable ({err_msg})"
                        )
                    else:
                        print(
                            "Diagnosis step: agent declared the failure unsalvageable."
                        )
                    report["verdict"] = "unsalvageable"
                    report["summary"] = err_msg
                    return False
                # status == "fixed" or "transient" → proceed to the relaunch step.
                # Both count as a rescue attempt; the launcher increments
                # rescue_count when it actually relaunches. err_msg carries the
                # agent's summary on the success path so a post-mortem reader
                # can see WHAT was diagnosed.
                if err_msg:
                    print(
                        f"Diagnosis step complete (status: {status}, diagnosis: {err_msg}); handing off to relaunch step..."
                    )
                else:
                    print(
                        f"Diagnosis step complete (status: {status}); handing off to relaunch step..."
                    )
                report["verdict"] = status
                report["summary"] = err_msg
            else:
                print("Diagnosis: running diagnosis agent inline — Ctrl-C to abort.")
                print(f"Marker: {marker_path}")
                print()
                cmd = [
                    "claude",
                    "-p",
                    "--permission-mode",
                    "bypassPermissions",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                ]
                try:
                    p = subprocess.Popen(
                        cmd,
                        cwd=scratch_dir,
                        stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE,
                    )
                    stdin = cast(IO[bytes], p.stdin)
                    stdin.write(prompt.encode())
                    stdin.close()
                except FileNotFoundError:
                    print("error: 'claude' CLI not found in PATH")
                    _write_bail(
                        sf,
                        wdir,
                        "diagnosis_claude_error",
                        "'claude' CLI not found in PATH",
                    )
                    report["verdict"] = "diagnosis_claude_error"
                    report["summary"] = "'claude' CLI not found in PATH"
                    return False
                try:
                    if p.stdout is not None:
                        stream_events(p.stdout, prefix="[rescue] ")
                        p.stdout.close()
                    rc = p.wait()
                except KeyboardInterrupt:
                    try:
                        p.terminate()
                        p.wait(timeout=2)
                    except Exception:
                        pass
                    if p.poll() is None:
                        try:
                            p.kill()
                        except Exception:
                            pass
                    print(
                        "\nRescue aborted by user. Gremlin state preserved — rerun /gremlins rescue, rm, or close."
                    )
                    aborted[0] = True
                    return False

                print()

                if rc != 0:
                    detail = f"claude -p exited {rc}"
                    _write_bail(sf, wdir, "diagnosis_claude_error", detail)
                    print(f"Diagnosis: rescue agent exited with code {rc}.")
                    print(
                        f"Inspect the log at {log_path} and worktree at {workdir} for details."
                    )
                    report["verdict"] = "diagnosis_claude_error"
                    report["summary"] = detail
                    return False

                # Zero exit: the marker is the source of truth. Without it the
                # agent is presumed to have abdicated the protocol — bail rather
                # than silently launching the relaunch step into the same broken state.
                status, msg = _read_rescue_marker(marker_path)
                if status == "structural":
                    _write_bail(sf, wdir, "structural", msg)
                    print(
                        f"Diagnosis: agent flagged a structural problem in the pipeline "
                        f"or sibling artifacts that requires a human edit ({msg})"
                    )
                    print(
                        "Gremlin marked dead:bailed:structural — rerun /gremlins rescue after fixing the named pipeline file or sibling plan."
                    )
                    report["verdict"] = "structural"
                    report["summary"] = msg
                    return False
                if status == "unsalvageable":
                    _write_bail(sf, wdir, "unsalvageable", msg)
                    if msg:
                        print(
                            f"Diagnosis: agent declared the failure unsalvageable ({msg})"
                        )
                    else:
                        print("Diagnosis: agent declared the failure unsalvageable.")
                    # The bail is recorded but interactive callers can still rerun
                    # /gremlins rescue (do_rescue's preflight only refuses
                    # running/finished/stalled), so make that explicit — the
                    # `dead:bailed:unsalvageable` liveness can otherwise look
                    # terminal at a glance.
                    print(
                        "Gremlin marked dead:bailed:unsalvageable — rerun /gremlins rescue if you want to try again."
                    )
                    report["verdict"] = "unsalvageable"
                    report["summary"] = msg
                    return False
                if status == "no_marker":
                    _write_bail(sf, wdir, "diagnosis_no_marker", msg)
                    print(f"Diagnosis produced no marker file: {msg}")
                    report["verdict"] = "diagnosis_no_marker"
                    report["summary"] = msg
                    return False
                if status == "bad_marker":
                    _write_bail(sf, wdir, "diagnosis_bad_marker", msg)
                    print(f"Diagnosis marker file invalid: {msg}")
                    report["verdict"] = "diagnosis_bad_marker"
                    report["summary"] = msg
                    return False
                # status ∈ {"fixed", "transient"} → proceed to the relaunch step.
                if msg:
                    print(f"Diagnosis summary: {msg}")
                print(
                    f"Diagnosis complete (status: {status}); handing off to relaunch step..."
                )
                report["verdict"] = status
                report["summary"] = msg
        finally:
            if scratch_dir is not None:
                shutil.rmtree(scratch_dir, ignore_errors=True)

        # Relaunch step: call the Python launcher's resume() to re-spawn the
        # pipeline under the same GREMLIN_ID with --resume-from <stage>.
        print()
        print(f"Relaunch step: resuming gremlin {gremlin_id} in the background...")
        report["relaunch_attempted"] = True
        StateData.load(gremlin_id).patch(rescue_count=rescue_count + 1)
        try:
            _resume(gremlin_id)
        except GremlinAlreadyRunning as exc:
            detail = str(exc)
            # The gremlin is already running — skip relaunch regardless of how it got there.
            print(f"note: relaunch skipped — gremlin is already running: {detail}")
            report["relaunch_outcome"] = "already_running"
            report["relaunch_reason"] = detail
            return True
        except Exception as exc:
            detail = str(exc)
            if headless:
                _write_bail(sf, wdir, "relaunch_failed", detail)
            print(f"error: background resume failed: {detail}")
            report["relaunch_outcome"] = "failed"
            report["relaunch_reason"] = detail
            return False

        report["relaunch_outcome"] = "success"
        report["relaunch_reason"] = "launcher resume() succeeded"
        return True
    finally:
        if not aborted[0]:
            write_rescue_report(wdir, report)
