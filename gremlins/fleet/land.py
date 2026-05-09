"""Land, rm subcommands and all land helpers."""

import json
import os
import pathlib
import re
import secrets
import shutil
import subprocess
import time
from typing import Any, cast

import gremlins.git as _git
from gremlins.fleet import constants as _constants
from gremlins.fleet.resolve import resolve_gremlin
from gremlins.fleet.state import (
    effective_pipeline_kind,
    liveness_of_state_file,
    load_state,
)
from gremlins.utils import proc


def expected_branch(state: dict[str, Any], gr_id: str):
    """Return the durable branch name for a gremlin, or None if there isn't one."""
    if effective_pipeline_kind(state) == "local":
        return f"bg/localgremlin/{gr_id}"
    return None


def _print_cost(state: dict[str, Any]) -> None:
    cost = state.get("total_cost_usd")
    if isinstance(cost, (int, float)) and cost > 0:
        print(f"total cost: ${cost:.4f}")


def _persist_land_cost(sf: str, state: dict[str, Any], additional_cost: float) -> None:
    """Fold a land-time `claude -p` cost into state.json's total_cost_usd.

    Writes through to disk so the value `_print_cost` reports — and any later
    fleet status reader — reflects spend that happened during land. Mutates
    `state` in place so the immediately-following `_print_cost(state)` sees
    the updated total. Best-effort: cost accounting must not crash a
    successful land.
    """
    if additional_cost <= 0:
        return
    try:
        with open(sf, encoding="utf-8") as f:
            data = json.load(f)
        existing = data.get("total_cost_usd")
        existing = float(existing) if isinstance(existing, (int, float)) else 0.0
        new_total = existing + float(additional_cost)
        data["total_cost_usd"] = new_total
        tmp = f"{sf}.{os.getpid()}.{secrets.token_hex(8)}.tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(data, f)
        os.replace(tmp, sf)
        state["total_cost_usd"] = new_total
    except Exception:
        pass


def _resolve_landing_cwd(state: dict[str, Any]) -> str:
    """Return a project_root suitable as cwd for `gh pr merge --delete-branch`.

    For boss-launched children, state.project_root is the boss's worktree, which
    is on a detached HEAD. After --delete-branch, gh tries to switch off the
    deleted branch and fails with "could not determine current branch: failed
    to run git: not on any branch". Walk parent_id up to the topmost ancestor
    (the user's actual repo, on a real branch) to avoid that.
    """
    own_root = state.get("project_root") or ""
    parent_id = state.get("parent_id") or ""
    if not parent_id:
        return own_root

    # Pre-seed cycle protection with the starting state's id so a pathological
    # cycle that loops back through the starting gremlin trips on first revisit.
    seen = {state.get("id") or ""}
    current: dict[str, Any] = state
    while True:
        pid = current.get("parent_id") or ""
        if not pid:
            # Clean termination: reached the topmost ancestor. Note: if its
            # project_root is empty/missing (e.g. corrupted boss state.json),
            # the own_root fallback may still be detached — strictly no worse
            # than the original failure mode.
            return current.get("project_root") or own_root
        if pid in seen:
            # Cycle in parent chain — fall back to own_root rather than
            # returning a possibly-detached intermediate ancestor.
            return own_root
        seen.add(pid)
        parent_sf = os.path.join(_constants.STATE_ROOT, pid, "state.json")
        parent_state = load_state(parent_sf)
        if not parent_state:
            # Unreadable parent state — fall back to own_root rather than
            # returning a possibly-detached intermediate ancestor.
            return own_root
        current = cast(dict[str, Any], parent_state)


def _fast_forward_main(cwd: str | None):
    """Attempt to fast-forward local main to origin/main after a gh PR merge."""
    if not _git.try_fetch_all(cwd=cwd):
        print("warning: git fetch origin failed")
        return
    current = _git.current_branch(cwd=cwd)
    if current == "main":
        try:
            _git.ff_merge("origin/main", cwd=cwd)
            print("Fast-forwarded local main.")
        except _git.GitError as e:
            msg = "warning: local main has diverged from origin/main — fast-forward not possible; update manually"
            if e.stderr:
                msg += f"\n  git: {e.stderr}"
            print(msg)
    else:
        if _git.is_ancestor("main", "origin/main", cwd=cwd):
            try:
                _git.force_update_branch("main", "origin/main", cwd=cwd)
                print("Fast-forwarded local main.")
            except _git.GitError as e:
                print(f"warning: could not fast-forward main: {e.stderr}")
        else:
            print("warning: local main has diverged from origin/main — update manually")


def _cleanup_gremlin(
    gr_id: str,
    sf: str,
    wdir: str,
    state: dict[str, Any],
    cwd: str | None,
    *,
    delete_branch: bool = True,
    check_cwd: bool = False,
    remove_state_dir: bool = True,
) -> bool:
    """Touch closed marker, remove worktree, optionally delete branch, optionally remove state dir.

    Returns False only when check_cwd=True and we're inside the worktree; all
    other steps are best-effort (warnings printed on failure).
    """
    workdir = state.get("workdir") or ""

    if check_cwd and workdir and os.path.exists(workdir):
        cwd_real = os.path.realpath(os.getcwd())
        worktree_real = os.path.realpath(workdir)
        if cwd_real == worktree_real or cwd_real.startswith(worktree_real + os.sep):
            print(
                "you are inside this gremlin's worktree — cd elsewhere before running this command"
            )
            return False

    # Mark closed before cleanup so a partial failure doesn't allow a re-run.
    try:
        pathlib.Path(os.path.join(wdir, "closed")).touch()
    except OSError:
        pass

    if workdir and os.path.exists(workdir):
        _git.remove_worktree(cwd or os.getcwd(), workdir)
        if os.path.exists(workdir):
            try:
                shutil.rmtree(workdir)
            except OSError as e:
                print(f"warning: could not remove worktree {workdir}: {e}")
        if not os.path.exists(workdir):
            print(f"removed worktree {workdir}")

    if delete_branch:
        branch = state.get("branch") or expected_branch(state, gr_id)
        if branch:
            try:
                _git.delete_branch(branch, force=True, cwd=cwd)
                print(f"deleted branch {branch}")
            except _git.GitError as e:
                if "not found" not in e.stderr:
                    print(f"warning: could not delete branch {branch}: {e.stderr}")

    if remove_state_dir:
        try:
            shutil.rmtree(wdir)
            print(f"removed state directory {wdir}")
        except OSError as e:
            print(f"warning: could not remove state directory {wdir}: {e}")

    return True


def do_rm(target: str) -> bool:
    match = resolve_gremlin(target)
    if match is None:
        return False

    gr_id, sf, wdir = match
    state = load_state(sf)
    if not state:
        print(f"error: could not read state for {gr_id}")
        return False

    live = liveness_of_state_file(sf, state)

    if not live:
        print(f"error: could not determine liveness for {gr_id}")
        return False

    if live == "running" or live.startswith("stalled:"):
        print(f"gremlin {gr_id} is still live ({live}) — use 'stop' first, then rm")
        return False

    project_root = str(state.get("project_root") or "")
    cwd_for_git = project_root if project_root and os.path.isdir(project_root) else None

    if not _cleanup_gremlin(
        gr_id,
        sf,
        wdir,
        cast(dict[str, Any], state),
        cwd_for_git,
        delete_branch=True,
        check_cwd=True,
    ):
        return False

    print(f"rm: gremlin {gr_id} cleaned up")
    return True


def _compose_commit_message(plan_path: str):
    """Return (subject, body) distilled from plan.md's ## Context and ## Tasks."""
    try:
        with open(plan_path, encoding="utf-8") as fh:
            content = fh.read()
    except OSError:
        return "Land gremlin branch", ""

    m = re.search(
        r"^##\s+Context\s*\n(.*?)(?=^##\s|\Z)", content, re.MULTILINE | re.DOTALL
    )
    if not m:
        return "Land gremlin branch", ""

    para = next(
        (p.strip() for p in re.split(r"\n\n+", m.group(1).strip()) if p.strip()),
        "",
    )
    if not para:
        return "Land gremlin branch", ""

    subject = " ".join(para.split())
    subject = re.sub(
        r"^(?:implement\s+|add\s+support\s+for\s+|this\s+change\s+|this\s+pr\s+)",
        "",
        subject,
        flags=re.IGNORECASE,
    )
    if subject:
        subject = subject[0].upper() + subject[1:]

    if len(subject) > 72:
        cut = subject[:72]
        boundary = cut.rfind(" ")
        subject = cut[:boundary] if boundary > 0 else cut

    tm = re.search(
        r"^##\s+Tasks\s*\n(.*?)(?=^##\s|\Z)", content, re.MULTILINE | re.DOTALL
    )
    body = ""
    if tm:
        done = re.findall(
            r"^\s*-\s+\[x\]\s+(.+)", tm.group(1), re.MULTILINE | re.IGNORECASE
        )
        if done:
            body = "\n".join(f"- {t.strip()}" for t in done[:8])

    return subject, body


def _gather_commit_inputs(
    wdir: str, state: dict[str, Any], branch: str, merge_base: str, cwd: str | None
) -> dict[str, Any]:
    """Collect all available context for commit message synthesis."""
    inputs = {"description": state.get("description", "")}

    _CONTENT_CAP = 4000  # chars; enough context without blowing up the prompt

    plan_path = os.path.join(wdir, "artifacts", "plan.md")
    try:
        with open(plan_path, encoding="utf-8") as fh:
            inputs["plan"] = fh.read(_CONTENT_CAP)
    except OSError:
        inputs["plan"] = ""

    spec_path = os.path.join(wdir, "artifacts", "spec.md")
    try:
        with open(spec_path, encoding="utf-8") as fh:
            inputs["spec"] = fh.read(_CONTENT_CAP)
    except OSError:
        inputs["spec"] = ""

    inputs["git_log"] = "\n".join(
        _git.log_oneline(f"{merge_base}..{branch}", cwd=cwd).splitlines()[:100]
    )
    inputs["git_stat"] = "\n".join(
        _git.diff_stat(f"{merge_base}..{branch}", cwd=cwd).splitlines()[:100]
    )

    return inputs


def _parse_commit_output(text: str) -> tuple[str, str]:
    """Split model output into (subject, body) on the first blank line."""
    lines = text.strip().splitlines()
    subject = ""
    body_lines: list[str] = []
    past_blank = False
    for line in lines:
        if not subject:
            subject = line.strip()
        elif not past_blank and line.strip() == "":
            past_blank = True
        elif past_blank or line.strip():
            past_blank = True
            body_lines.append(line)

    if len(subject) > 72:
        cut = subject[:72]
        boundary = cut.rfind(" ")
        subject = cut[:boundary] if boundary > 0 else cut

    body = "\n".join(body_lines).strip()
    return subject, body


def _run_claude_p_text(prompt: str, timeout: int = 60) -> tuple[str, float]:
    """Run `claude -p` and return (stdout text, total_cost_usd).

    Uses `--output-format json` so the single result object carries both the
    assistant's reply text and `total_cost_usd`. The cost is surfaced so the
    caller can fold land-time `claude -p` spend into the gremlin's reported
    total — without it, `gremlins land`'s commit-message synthesis would not
    show up in the "total cost" line printed after a squash-land.

    Suppresses the session-summary hook via `GREMLIN_SKIP_SUMMARY=1`; otherwise
    the hook's "surface this verbatim" directive prepends the gremlin status
    block to the model's reply and corrupts structured output. Any `claude -p`
    caller in this repo that parses the reply as text should go through here.
    """
    env = os.environ.copy()
    env["GREMLIN_SKIP_SUMMARY"] = "1"
    result = subprocess.run(
        ["claude", "-p", "--output-format", "json"],
        input=prompt,
        capture_output=True,
        text=True,
        timeout=timeout,
        env=env,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"claude -p exited {result.returncode}: {result.stderr.strip()}"
        )
    try:
        data = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"claude -p returned non-JSON output: {exc}")
    text = data.get("result") if isinstance(data.get("result"), str) else ""
    raw_cost = data.get("total_cost_usd", data.get("cost_usd"))
    cost = float(raw_cost) if isinstance(raw_cost, (int, float)) else 0.0
    return text, cost


def _synthesize_commit_message_ai(inputs: dict[str, Any]) -> tuple[str, str, float]:
    """Call `claude -p` to produce a commit message from gathered inputs."""
    parts: list[str] = []

    if inputs.get("description"):
        parts.append(f"Gremlin description: {inputs['description']}")

    if inputs.get("git_log"):
        parts.append(f"Branch commits (git log --oneline):\n{inputs['git_log']}")

    if inputs.get("git_stat"):
        parts.append(f"Changed files (git diff --stat):\n{inputs['git_stat']}")

    if inputs.get("spec"):
        parts.append(f"Spec:\n{inputs['spec']}")

    if inputs.get("plan"):
        parts.append(f"Implementation plan:\n{inputs['plan']}")

    context_block = "\n\n".join(parts)

    prompt = f"""Write a git commit message for the following change.

{context_block}

Requirements:
- First line: subject in imperative mood, ≤72 characters, describing WHAT was done (not why)
- Blank line
- 2–3 sentence summary of what the change does

Output only the commit message text, nothing else."""

    stdout, cost = _run_claude_p_text(prompt)
    subject, body = _parse_commit_output(stdout)
    if not subject:
        raise RuntimeError("claude -p returned empty subject")
    return subject, body, cost


def _build_commit_message(
    wdir: str, state: dict[str, Any], branch: str, merge_base: str, cwd: str | None
) -> tuple[str, str, float]:
    """Return (subject, body, cost_usd) using AI synthesis with fallback to regex extraction."""
    inputs = _gather_commit_inputs(wdir, state, branch, merge_base, cwd)

    print("Composing commit message...", flush=True)
    try:
        subject, body, cost = _synthesize_commit_message_ai(inputs)
        print(f"Commit message: {subject}", flush=True)
        return subject, body, cost
    except Exception as exc:
        print(
            f"warning: AI commit message synthesis failed ({exc}); falling back to plan.md extraction",
            flush=True,
        )
        plan_path = os.path.join(wdir, "artifacts", "plan.md")
        if not os.path.isfile(plan_path):
            print(
                f"error: plan.md not found at {plan_path} — cannot build commit message"
            )
            raise
        subject, body = _compose_commit_message(plan_path)
        return subject, body, 0.0


def _inside_worktree(workdir: str) -> bool:
    if not workdir or not os.path.exists(workdir):
        return False
    cwd_real = os.path.realpath(os.getcwd())
    worktree_real = os.path.realpath(workdir)
    return cwd_real == worktree_real or cwd_real.startswith(worktree_real + os.sep)


def _preflight_land(state: dict[str, Any], cwd: str | None) -> tuple[str, bool]:
    """Shared land preflight. Returns (current_branch, ok)."""
    workdir = state.get("workdir") or ""
    if _inside_worktree(workdir):
        print("you are inside this gremlin's worktree — cd elsewhere before landing")
        return "", False

    current = _git.current_branch(cwd=cwd)
    if not current:
        # Detached HEAD: head_sha succeeds but no branch name exists.
        if _git.head_sha(cwd=cwd):
            current = "HEAD"
        else:
            print("error: could not determine current branch")
            return "", False

    tracked_changes = [
        ln
        for ln in _git.status_porcelain(cwd=cwd).splitlines()
        if not ln.startswith(("??", "!!"))
    ]
    if tracked_changes:
        print(
            "error: working tree is not clean — commit or stash changes before landing"
        )
        return current, False

    return current, True


def _squash_land(
    gr_id: str,
    sf: str,
    wdir: str,
    state: dict[str, Any],
    cwd: str | None,
    source_ref: str,
    source_label: str,
    current: str,
    delete_branch: bool,
) -> bool:
    """Squash all commits above the merge-base of `source_ref` and HEAD, then commit."""
    try:
        base = _git.merge_base("HEAD", source_ref, cwd=cwd)
    except _git.GitError:
        print(f"error: could not compute merge-base between HEAD and {source_label}")
        return False

    try:
        commit_count = _git.rev_list_count(f"{base}..{source_ref}", cwd=cwd)
    except _git.GitError:
        print(f"error: could not count commits between merge-base and {source_label}")
        return False
    if commit_count < 1:
        print(f"{current} is already up to date with {source_label}.")
        _cleanup_gremlin(
            gr_id,
            sf,
            wdir,
            state,
            cwd,
            delete_branch=delete_branch,
            remove_state_dir=False,
        )
        return True

    pre_merge_untracked = _git.ls_others(cwd=cwd)

    print(f"Squash-merging {source_label} onto {current}...")
    try:
        _git.squash_merge(source_ref, cwd=cwd)
    except _git.GitError as e:
        reset_ok = True
        try:
            _git.reset_hard("HEAD", cwd=cwd)
        except _git.GitError:
            reset_ok = False
        if not pre_merge_untracked:
            _git.clean_fd(cwd=cwd)
        suffix = "working tree restored" if reset_ok else "manual cleanup may be needed"
        detail = f"\n  git: {e.stderr}" if e.stderr else ""
        print(f"error: git merge --squash failed — {suffix}{detail}")
        return False

    subject, body, land_cost = _build_commit_message(wdir, state, source_ref, base, cwd)
    commit_msg = f"{subject}\n\n{body}" if body else subject

    try:
        _git.commit(commit_msg, cwd=cwd)
    except _git.GitError as e:
        detail = f"\n  git: {e.stderr}" if e.stderr else ""
        print(f"error: git commit failed{detail}")
        return False

    print(f"Landed {source_label} onto {current}.")
    _persist_land_cost(sf, state, land_cost)
    _print_cost(state)
    _cleanup_gremlin(
        gr_id, sf, wdir, state, cwd, delete_branch=delete_branch, remove_state_dir=False
    )
    return True


def _ff_land(
    gr_id: str,
    sf: str,
    wdir: str,
    state: dict[str, Any],
    cwd: str | None,
    source_ref: str,
    source_label: str,
    current: str,
    delete_branch: bool,
) -> bool:
    """Fast-forward the caller's branch to `source_ref`. Hard fail if ff is not possible."""
    if not _git.is_ancestor("HEAD", source_ref, cwd=cwd):
        print(
            f"error: cannot fast-forward — {current} has diverged from {source_label}. "
            f"Re-run with --squash to condense the chain into one commit, or rebase manually."
        )
        return False

    try:
        commit_count = _git.rev_list_count(f"HEAD..{source_ref}", cwd=cwd)
    except _git.GitError:
        print(f"error: could not count commits between HEAD and {source_label}")
        return False
    if commit_count < 1:
        print(f"{current} is already up to date with {source_label}.")
        _cleanup_gremlin(
            gr_id,
            sf,
            wdir,
            state,
            cwd,
            delete_branch=delete_branch,
            remove_state_dir=False,
        )
        return True

    print(f"Fast-forwarding {current} to {source_label}...")
    try:
        _git.ff_merge(source_ref, cwd=cwd)
    except _git.GitError as e:
        detail = f"\n  git: {e.stderr}" if e.stderr else ""
        print(f"error: git merge --ff-only failed{detail}")
        return False

    print(f"Landed {source_label} onto {current}.")
    _print_cost(state)
    _cleanup_gremlin(
        gr_id, sf, wdir, state, cwd, delete_branch=delete_branch, remove_state_dir=False
    )
    return True


def _land_local(
    gr_id: str, sf: str, wdir: str, state: dict[str, Any], mode: str, into_dir: str = ""
) -> bool:
    """Land a local gremlin branch (mode: 'squash' or 'ff'). If into_dir is given, land there instead of project_root."""
    setup_kind = state.get("setup_kind", "")
    if setup_kind != "worktree-branch":
        print(
            f"gremlin {gr_id} has setup_kind={setup_kind!r} — only worktree-branch gremlins support local landing"
        )
        return False

    branch = state.get("branch", "")
    if not branch:
        print(f"error: no branch field in state for {gr_id}")
        return False

    project_root = state.get("project_root") or ""
    if into_dir:
        if not os.path.isdir(into_dir):
            print(f"error: --into directory does not exist: {into_dir!r}")
            return False
        cwd = into_dir
    else:
        cwd = project_root if project_root and os.path.isdir(project_root) else None

    if not _git.branch_exists(branch, cwd=cwd):
        print(
            f"error: gremlin branch {branch!r} does not exist — may already have been cleaned up"
        )
        return False

    current, ok = _preflight_land(state, cwd)
    if not ok:
        return False
    if current == branch:
        print(
            f"error: currently on gremlin branch {branch!r} — switch to your target branch first"
        )
        return False

    if mode == "squash":
        return _squash_land(
            gr_id, sf, wdir, state, cwd, branch, branch, current, delete_branch=True
        )
    return _ff_land(
        gr_id, sf, wdir, state, cwd, branch, branch, current, delete_branch=True
    )


def _land_boss(
    gr_id: str, sf: str, wdir: str, state: dict[str, Any], mode: str
) -> bool:
    """Land a boss gremlin's chain of squash commits onto the current branch."""
    workdir = state.get("workdir") or ""
    if not workdir or not os.path.isdir(workdir):
        print(
            f"error: boss worktree missing ({workdir!r}) — cannot resolve chain HEAD. "
            f"Its commits are likely unreachable; use 'gremlins rm {gr_id}' to clean up."
        )
        return False

    boss_head = _git.head_sha(cwd=workdir)
    if not boss_head:
        print(f"error: could not resolve HEAD in boss worktree {workdir}")
        return False

    project_root = state.get("project_root") or ""
    cwd = project_root if project_root and os.path.isdir(project_root) else None

    current, ok = _preflight_land(state, cwd)
    if not ok:
        return False

    label = f"boss {gr_id} ({boss_head[:12]})"
    if mode == "squash":
        return _squash_land(
            gr_id, sf, wdir, state, cwd, boss_head, label, current, delete_branch=False
        )
    return _ff_land(
        gr_id, sf, wdir, state, cwd, boss_head, label, current, delete_branch=False
    )


def _land_gh(
    gr_id: str, sf: str, wdir: str, state: dict[str, Any], force: bool = False
) -> bool:
    """Merge a gh gremlin's PR and clean up."""
    pr_url = state.get("pr_url", "")
    if not pr_url:
        print(f"error: no pr_url in state for {gr_id}")
        print(
            "This gremlin may have been launched before pr_url tracking was added to ghgremlin.sh."
        )
        return False

    project_root = _resolve_landing_cwd(state)
    cwd = project_root if project_root and os.path.isdir(project_root) else None

    print(f"Checking PR: {pr_url}")
    r = proc.run(
        [
            "gh",
            "pr",
            "view",
            pr_url,
            "--json",
            "state,mergeable,reviewDecision,statusCheckRollup",
        ],
        cwd=cwd,
    )
    if r.returncode != 0:
        print(f"error: could not fetch PR info: {r.stderr.strip()}")
        return False

    try:
        pr_info = json.loads(r.stdout)
    except json.JSONDecodeError:
        print("error: could not parse PR info response")
        return False

    pr_state = pr_info.get("state", "")
    mergeable = pr_info.get("mergeable", "")
    review_decision = pr_info.get("reviewDecision") or ""
    checks: list[Any] = pr_info.get("statusCheckRollup") or []

    if pr_state == "MERGED":
        print("PR already merged.")
        _fast_forward_main(cwd)
        _cleanup_gremlin(
            gr_id, sf, wdir, state, cwd, delete_branch=False, remove_state_dir=False
        )
        return True

    if pr_state == "CLOSED":
        if force:
            print(
                "PR is closed (not merged) — force flag set, cleaning up without merge."
            )
            _cleanup_gremlin(
                gr_id, sf, wdir, state, cwd, delete_branch=False, remove_state_dir=False
            )
            return True
        print(f"PR is closed (not merged): {pr_url}")
        print("Use --force to skip merge and clean up only.")
        return False

    # PR is OPEN — check for blockers before merging
    if review_decision == "CHANGES_REQUESTED":
        print(
            "error: PR has changes requested — address review comments before landing"
        )
        print(f"  {pr_url}")
        return False

    failed = [
        c
        for c in checks
        if c.get("conclusion") in ("FAILURE", "ERROR", "TIMED_OUT", "CANCELLED")
    ]
    if failed:
        names = ", ".join(c.get("name", "?") for c in failed[:3])
        print(f"error: PR has failed CI checks: {names}")
        print(f"  {pr_url}")
        return False

    if mergeable == "UNKNOWN":
        print("GitHub is computing mergeability — waiting 5s and retrying...")
        time.sleep(5)
        r = proc.run(["gh", "pr", "view", pr_url, "--json", "mergeable"], cwd=cwd)
        if r.returncode == 0:
            try:
                mergeable = json.loads(r.stdout).get("mergeable", "UNKNOWN")
            except json.JSONDecodeError:
                pass

    if mergeable == "CONFLICTING":
        print("error: PR has merge conflicts — resolve them before landing")
        print(f"  {pr_url}")
        return False

    print(f"Merging: {pr_url}")
    r = proc.run(["gh", "pr", "merge", pr_url, "--squash", "--delete-branch"], cwd=cwd)
    if r.returncode != 0:
        if "already merged" in r.stdout.lower() or "already merged" in r.stderr.lower():
            print("PR was already merged.")
        else:
            # gh may exit non-zero on post-merge cleanup (e.g. --delete-branch
            # tries to switch off the deleted branch and fails on a detached
            # HEAD cwd) even though the PR did merge. Re-verify before bailing.
            err = r.stderr.strip() or r.stdout.strip()
            v = proc.run(["gh", "pr", "view", pr_url, "--json", "state"], cwd=cwd)
            verified_merged = False
            verify_err = ""
            if v.returncode == 0:
                try:
                    verified_merged = json.loads(v.stdout).get("state") == "MERGED"
                except json.JSONDecodeError as e:
                    verify_err = f"could not parse gh pr view response: {e}"
            else:
                verify_err = v.stderr.strip() or v.stdout.strip()
            if verified_merged:
                print(
                    f"warning: gh pr merge exited non-zero ({err}) but PR is MERGED on GitHub — proceeding with cleanup."
                )
            else:
                if verify_err:
                    # Verification was inconclusive (gh pr view failed or returned
                    # unparseable output) — operator should check PR state manually
                    # before reaching for `rescue` or re-running `land`.
                    print(
                        f"error: gh pr merge failed: {err}; verification inconclusive: {verify_err}"
                    )
                else:
                    print(f"error: gh pr merge failed: {err}")
                return False
    else:
        print("PR merged.")

    _fast_forward_main(cwd)
    _cleanup_gremlin(
        gr_id, sf, wdir, state, cwd, delete_branch=False, remove_state_dir=False
    )
    return True


def do_land(
    target: str, force: bool = False, mode: str | None = None, into_dir: str = ""
) -> bool:
    match = resolve_gremlin(target)
    if match is None:
        return False

    gr_id, sf, wdir = match
    state = load_state(sf)
    if not state:
        print(f"error: could not read state for {gr_id}")
        return False

    live = liveness_of_state_file(sf, state)
    if live == "running" or live.startswith("stalled:"):
        print(f"gremlin {gr_id} is still live ({live}) — use 'stop' first, then land")
        return False

    pk = effective_pipeline_kind(state)
    if pk == "local":
        if live != "dead:finished":
            print(f"gremlin {gr_id} is not finished (liveness: {live})")
            return False
        return _land_local(gr_id, sf, wdir, state, mode or "squash", into_dir=into_dir)
    elif pk == "boss":
        if live != "dead:finished":
            print(f"gremlin {gr_id} is not finished (liveness: {live})")
            return False
        return _land_boss(gr_id, sf, wdir, state, mode or "ff")
    elif pk == "gh":
        if mode is not None:
            print(
                "error: --squash/--ff are not applicable to gh gremlins (merged via PR)"
            )
            return False
        return _land_gh(gr_id, sf, wdir, state, force=force)
    return False
