"""Land, rm subcommands and all land helpers."""

import json
import logging
import os
import pathlib
import re
import secrets
import shutil
import sys
import time
from typing import Any, cast

from _gremlins_core.artifacts import ArtifactRegistry, MissingArtifact
from _gremlins_core.clients import RustClient as Client
from _gremlins_core.config import project_root as _project_root_fn
from _gremlins_core.config import scratch_root as _scratch_root_fn
from _gremlins_core.config import state_root as _state_root_fn

import gremlins.utils.git as _git
from gremlins.fleet.resolve import resolve_gremlin
from gremlins.fleet.state import (
    liveness_of_state_file,
    load_state,
)
from gremlins.utils import proc

logger = logging.getLogger(__name__)


def landable_shape(state: dict[str, Any]) -> str:
    """Classify artifact shape for land dispatch."""
    artifacts = list(state.get("artifacts") or [])
    prs = [art for art in artifacts if art.get("type") == "pr"]

    if not prs:
        return "empty"
    if len(prs) == 1:
        return "one_pr"
    return "many_prs"


def expected_branch(state: dict[str, Any], gremlin_id: str):
    """Return the durable branch name for a gremlin, or None if there isn't one."""
    artifacts = list(state.get("artifacts") or [])
    for art in reversed(artifacts):
        if art.get("type") == "pr":
            branch = str(art.get("branch") or "")
            return branch or None
    return None


def _print_cost(state: dict[str, Any]) -> None:
    cost = state.get("total_cost_usd")
    if isinstance(cost, (int, float)) and cost > 0:
        print(f"total cost: ${cost:.4f}")


def _persist_land_cost(sf: str, state: dict[str, Any], additional_cost: float) -> None:
    """Fold a land-time model cost into state.json's total_cost_usd.

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
        parent_sf = os.path.join(_state_root_fn(), pid, "state.json")
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


def _remove_worktree(wdir: str, state: dict[str, Any], cwd: str | None) -> None:
    """Touch closed marker and remove the gremlin's worktree.

    Marks closed first so the gremlin disappears from default views even if
    filesystem removal fails. Best-effort: warnings printed on failure.
    """
    try:
        pathlib.Path(os.path.join(wdir, "closed")).touch()
    except OSError:
        pass

    workdir = state.get("workdir") or ""
    if workdir and os.path.exists(workdir):
        _git.remove_worktree(cwd or _project_root_fn(), workdir)
        if os.path.exists(workdir):
            try:
                shutil.rmtree(workdir)
            except OSError as e:
                print(f"warning: could not remove worktree {workdir}: {e}")
        if not os.path.exists(workdir):
            print(f"removed worktree {workdir}")
            logger.info("cleanup: removing worktree %s", workdir)


def _remove_scratch(gremlin_id: str) -> None:
    """Remove the gremlin's scratch directory if it exists."""
    scratch = pathlib.Path(_scratch_root_fn(gremlin_id))
    if scratch.is_dir():
        try:
            shutil.rmtree(scratch)
            print(f"removed scratch directory {scratch}")
            logger.info("cleanup: removing scratch dir for %s", gremlin_id)
        except OSError as e:
            print(f"warning: could not remove scratch directory {scratch}: {e}")


def _finalize_cleanup(
    gremlin_id: str,
    wdir: str,
    state: dict[str, Any],
    cwd: str | None,
    *,
    remove_state_dir: bool = True,
) -> None:
    """Optionally remove the state directory."""
    if remove_state_dir:
        try:
            shutil.rmtree(wdir)
            print(f"removed state directory {wdir}")
            logger.info("cleanup: removing state dir %s", wdir)
        except OSError as e:
            print(f"warning: could not remove state directory {wdir}: {e}")


def cleanup_gremlin(
    gremlin_id: str,
    wdir: str,
    state: dict[str, Any],
    cwd: str | None,
    *,
    check_cwd: bool = False,
    remove_state_dir: bool = True,
) -> bool:
    """Touch closed marker, remove worktree, scratch dir, optionally remove state dir.

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

    _remove_worktree(wdir, state, cwd)
    _remove_scratch(gremlin_id)
    _finalize_cleanup(
        gremlin_id,
        wdir,
        state,
        cwd,
        remove_state_dir=remove_state_dir,
    )
    return True


def _rm_parallel_children(gremlin_id: str, cwd_for_git: str | None) -> None:
    prefix = f"{gremlin_id}--"
    _sr = _state_root_fn()
    if not os.path.isdir(_sr):
        return
    for name in sorted(os.listdir(_sr)):
        if not name.startswith(prefix):
            continue
        wdir = os.path.join(_sr, name)
        sf = os.path.join(wdir, "state.json")
        if not os.path.isfile(sf):
            continue
        child_state = load_state(sf)
        if not child_state:
            continue
        live = liveness_of_state_file(sf, child_state)
        if live == "running" or (live and live.startswith("stalled:")):
            print(f"rm: skipping live child {name} ({live}) — stop it first")
            continue
        cleanup_gremlin(
            name,
            wdir,
            cast(dict[str, Any], child_state),
            cwd_for_git,
        )
        print(f"rm: parallel child {name} cleaned up")


def do_rm(target: str) -> bool:
    match = resolve_gremlin(target)
    if match is None:
        return False

    gremlin_id, sf, wdir = match
    state = load_state(sf)
    if not state:
        print(f"error: could not read state for {gremlin_id}")
        return False

    live = liveness_of_state_file(sf, state)

    if not live:
        print(f"error: could not determine liveness for {gremlin_id}")
        return False

    if live == "running" or live.startswith("stalled:"):
        print(
            f"gremlin {gremlin_id} is still live ({live}) — use 'stop' first, then rm"
        )
        return False

    project_root = str(state.get("project_root") or "")
    cwd_for_git = project_root if project_root and os.path.isdir(project_root) else None

    if not cleanup_gremlin(
        gremlin_id,
        wdir,
        cast(dict[str, Any], state),
        cwd_for_git,
        check_cwd=True,
    ):
        return False

    _rm_parallel_children(gremlin_id, cwd_for_git)
    print(f"rm: gremlin {gremlin_id} cleaned up")
    return True


def _registry_for_gremlin(gremlin_id: str) -> ArtifactRegistry:
    artifact_dir = pathlib.Path(_scratch_root_fn(gremlin_id)) / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    return ArtifactRegistry(artifact_dir=artifact_dir)


def compose_commit_message(plan_path: str):
    """Return (subject, body) distilled from plan.md."""
    try:
        with open(plan_path, encoding="utf-8") as fh:
            content = fh.read()
    except OSError:
        return _fallback_commit_subject(), ""
    return compose_commit_message_from_content(content)


def compose_commit_message_from_content(content: str):
    """Return (subject, body) distilled from plan.md content."""
    para = _extract_first_paragraph(content, "Context") or _extract_first_paragraph(
        content, "Goal"
    )

    if not para:
        title_m = re.search(r"^#\s+(.+)", content, re.MULTILINE)
        if title_m:
            para = title_m.group(1).strip()

    if not para:
        return _fallback_commit_subject(), ""

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

    body = _extract_task_body(content)
    return subject, body


def _fallback_commit_subject() -> str:
    return "Land gremlin branch"


def _extract_first_paragraph(content: str, heading: str) -> str | None:
    """Extract the first non-empty paragraph from ## <heading>."""
    m = re.search(
        rf"^##\s+{re.escape(heading)}\s*\n(.*?)(?=^##\s|\Z)",
        content,
        re.MULTILINE | re.DOTALL,
    )
    if not m:
        return None
    para = next(
        (p.strip() for p in re.split(r"\n\n+", m.group(1).strip()) if p.strip()),
        "",
    )
    return para or None


def _extract_task_body(content: str) -> str:
    """Extract body lines from ## Tasks checkboxes."""
    tm = re.search(
        r"^##\s+Tasks\s*\n(.*?)(?=^##\s|\Z)", content, re.MULTILINE | re.DOTALL
    )
    if not tm:
        return ""
    done = re.findall(
        r"^\s*-\s+\[x\]\s+(.+)", tm.group(1), re.MULTILINE | re.IGNORECASE
    )
    if done:
        return "\n".join(f"- {t.strip()}" for t in done[:8])
    return ""


def _gather_commit_inputs(
    registry: ArtifactRegistry,
    state: dict[str, Any],
    branch: str,
    merge_base: str,
    cwd: str | None,
) -> dict[str, Any]:
    """Collect all available context for commit message synthesis."""
    inputs = {"description": state.get("description", "")}

    _CONTENT_CAP = 4000  # chars; enough context without blowing up the prompt

    inputs["plan"] = registry.content("artifact://plan.md")[:_CONTENT_CAP]
    try:
        inputs["spec"] = registry.content("artifact://spec.md")[:_CONTENT_CAP]
    except MissingArtifact:
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
    if lines and lines[0].strip().startswith("```"):
        lines = lines[1:]
    if lines and lines[-1].strip().startswith("```"):
        lines = lines[:-1]
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


def _run_client_text(
    client: Client, prompt: str, label: str = "commit-msg"
) -> tuple[str, float]:
    """Run a Client prompt and return (text_result, cost_usd).

    Exists so land-time commit-message synthesis goes through the same
    backend abstraction as pipeline stages.
    """
    import asyncio

    completed = asyncio.run(
        client.run(
            prompt,
            label=label,
            capture_events=False,
            idle_timeout=60.0,
            max_retries=0,
        )
    )
    if completed.exit_code != 0:
        raise RuntimeError(f"client ({client}) exited {completed.exit_code}")
    text = completed.text_result or ""
    cost = completed.cost_usd or 0.0
    return text, cost


def _synthesize_commit_message_ai(
    inputs: dict[str, Any], client: Client
) -> tuple[str, str, float]:
    """Call the configured model to produce a commit message from gathered inputs."""
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

    stdout, cost = _run_client_text(client, prompt)
    subject, body = _parse_commit_output(stdout)
    if not subject:
        raise RuntimeError("model returned empty subject")
    return subject, body, cost


def _build_commit_message(
    registry: ArtifactRegistry,
    state: dict[str, Any],
    branch: str,
    merge_base: str,
    cwd: str | None,
    client: Client,
) -> tuple[str, str, float]:
    """Return (subject, body, cost_usd) using AI synthesis with fallback to regex extraction."""
    inputs = _gather_commit_inputs(registry, state, branch, merge_base, cwd)

    logger.debug("_build_commit_message: branch=%s merge_base=%s", branch, merge_base)
    logger.debug(
        "_build_commit_message: plan len=%d spec len=%d",
        len(inputs.get("plan") or ""),
        len(inputs.get("spec") or ""),
    )
    logger.debug(
        "_build_commit_message: git_log lines=%d",
        len(inputs.get("git_log") or ""),
    )

    print("Composing commit message...", flush=True)
    try:
        subject, body, cost = _synthesize_commit_message_ai(inputs, client)
        print(f"Commit message: {subject}", flush=True)
        return subject, body, cost
    except Exception as exc:
        print(
            f"warning: AI commit message synthesis failed ({exc}); falling back to plan.md extraction",
            flush=True,
        )
        plan = registry.content("artifact://plan.md")
        if not plan:
            print("error: plan.md not found — cannot build commit message")
            raise
        subject, body = compose_commit_message_from_content(plan)
        return subject, body, 0.0


def _inside_worktree(workdir: str) -> bool:
    if not workdir or not os.path.exists(workdir):
        return False
    cwd_real = os.path.realpath(os.getcwd())
    worktree_real = os.path.realpath(workdir)
    return cwd_real == worktree_real or cwd_real.startswith(worktree_real + os.sep)


def _preflight_land(state: dict[str, Any], cwd: str | None) -> tuple[str, int, bool]:
    """Shared land preflight. Returns (current_branch, tracked_changes, ok)."""
    workdir = state.get("workdir") or ""
    if _inside_worktree(workdir):
        print("you are inside this gremlin's worktree — cd elsewhere before landing")
        return "", 0, False

    current = _git.current_branch(cwd=cwd)
    if not current:
        # Detached HEAD: head_sha succeeds but no branch name exists.
        if _git.head_sha(cwd=cwd):
            current = "HEAD"
        else:
            print("error: could not determine current branch")
            return "", 0, False

    tracked_changes = [
        ln
        for ln in _git.status_porcelain(cwd=cwd).splitlines()
        if not ln.startswith(("??", "!!"))
    ]
    if tracked_changes:
        print(
            "error: working tree is not clean — commit or stash changes before landing"
        )
        return current, len(tracked_changes), False

    return current, 0, True


def _squash_land(
    gremlin_id: str,
    sf: str,
    wdir: str,
    state: dict[str, Any],
    cwd: str | None,
    source_ref: str,
    source_label: str,
    current: str,
    client: Client,
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

    logger.debug("_squash_land: merge-base=%s commit_count=%d", base, commit_count)
    if commit_count < 1:
        print(f"{current} is already up to date with {source_label}.")
        cleanup_gremlin(
            gremlin_id,
            wdir,
            state,
            cwd,
            remove_state_dir=False,
        )
        return True

    pre_merge_untracked = _git.ls_others(cwd=cwd)

    logger.debug(
        "_squash_land: pre_merge_untracked=%s",
        pre_merge_untracked[:100] if pre_merge_untracked else "(none)",
    )

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

    subject, body, land_cost = _build_commit_message(
        _registry_for_gremlin(gremlin_id), state, source_ref, base, cwd, client
    )
    logger.debug("_squash_land: commit_message subject=%r", subject)
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
    cleanup_gremlin(
        gremlin_id,
        wdir,
        state,
        cwd,
        remove_state_dir=False,
    )
    return True


def _ff_land(
    gremlin_id: str,
    wdir: str,
    state: dict[str, Any],
    cwd: str | None,
    source_ref: str,
    source_label: str,
    current: str,
) -> bool:
    """Fast-forward the caller's branch to `source_ref`. Hard fail if ff is not possible."""
    is_ancestor = _git.is_ancestor("HEAD", source_ref, cwd=cwd)
    logger.debug("_ff_land: is_ancestor(HEAD, %s)=%s", source_ref, is_ancestor)
    if not is_ancestor:
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

    logger.debug("_ff_land: commit_count=%d", commit_count)
    if commit_count < 1:
        print(f"{current} is already up to date with {source_label}.")
        cleanup_gremlin(
            gremlin_id,
            wdir,
            state,
            cwd,
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
    cleanup_gremlin(
        gremlin_id,
        wdir,
        state,
        cwd,
        remove_state_dir=False,
    )
    return True


def _land_boss(
    gremlin_id: str,
    sf: str,
    wdir: str,
    state: dict[str, Any],
    mode: str,
    client: Client,
) -> bool:
    """Land a boss gremlin's chain of squash commits onto the current branch."""
    workdir = state.get("workdir") or ""
    if not workdir or not os.path.isdir(workdir):
        print(
            f"error: boss worktree missing ({workdir!r}) — cannot resolve chain HEAD. "
            f"Its commits are likely unreachable; use 'gremlins rm {gremlin_id}' to clean up."
        )
        return False

    boss_head = _git.head_sha(cwd=workdir)
    if not boss_head:
        print(f"error: could not resolve HEAD in boss worktree {workdir}")
        return False

    logger.info("_land_boss: boss_head=%s mode=%s", boss_head[:12], mode)

    project_root = state.get("project_root") or ""
    cwd = project_root if project_root and os.path.isdir(project_root) else None

    current, tracked_changes, ok = _preflight_land(state, cwd)
    if not ok:
        return False

    logger.info("_land_boss: current=%s tracked_changes=%d", current, tracked_changes)

    label = f"boss {gremlin_id} ({boss_head[:12]})"
    if mode == "squash":
        logger.info("_land_boss: squash-merging %s onto %s", label, current)
        return _squash_land(
            gremlin_id,
            sf,
            wdir,
            state,
            cwd,
            boss_head,
            label,
            current,
            client,
        )
    try:
        commit_count = _git.rev_list_count(f"{current}..{boss_head}", cwd=cwd)
    except _git.GitError:
        commit_count = 0
    logger.info(
        "_land_boss: fast-forwarding %s to %s (%d commits)",
        current,
        label,
        commit_count,
    )
    return _ff_land(gremlin_id, wdir, state, cwd, boss_head, label, current)


def _land_gh(
    gremlin_id: str, wdir: str, state: dict[str, Any], force: bool = False
) -> bool:
    """Merge a gh gremlin's PR and clean up."""
    project_root = _resolve_landing_cwd(state)
    cwd = project_root if project_root and os.path.isdir(project_root) else None

    artifact_dir = pathlib.Path(_scratch_root_fn(gremlin_id)) / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    registry = ArtifactRegistry(
        artifact_dir=artifact_dir,
    )
    pr_url = None
    for key in ("artifact://pr-url", "artifact://pr"):
        try:
            pr_url = registry.content(key).strip()
            break
        except (KeyError, MissingArtifact):
            continue
    if not pr_url:
        print(f"error: no PR URL recorded for {gremlin_id}")
        return False
    pr_url = pr_url.strip()

    logger.info("_land_gh: cwd=%s pr_url=%s", cwd or ".", pr_url)

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

    logger.info(
        "_land_gh: PR state=%s mergeable=%s review=%s",
        pr_state,
        mergeable,
        review_decision,
    )
    logger.debug(
        "_land_gh: CI checks=%s",
        [(c.get("name"), c.get("conclusion")) for c in checks],
    )

    if pr_state == "MERGED":
        print("PR already merged.")
        _fast_forward_main(cwd)
        _remove_worktree(wdir, state, cwd)
        _remove_scratch(gremlin_id)
        _finalize_cleanup(gremlin_id, wdir, state, cwd, remove_state_dir=False)
        return True

    if pr_state == "CLOSED":
        if force:
            print(
                "PR is closed (not merged) — force flag set, cleaning up without merge."
            )
            _remove_worktree(wdir, state, cwd)
            _remove_scratch(gremlin_id)
            _finalize_cleanup(
                gremlin_id,
                wdir,
                state,
                cwd,
                remove_state_dir=False,
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
    logger.info("_land_gh: merging %s", pr_url)
    _remove_worktree(wdir, state, cwd)
    r = proc.run(["gh", "pr", "merge", pr_url, "--squash", "--delete-branch"], cwd=cwd)
    if r.returncode != 0:
        if "already merged" in r.stdout.lower() or "already merged" in r.stderr.lower():
            print("PR was already merged.")
        else:
            # gh may exit non-zero on post-merge cleanup (e.g. --delete-branch
            # tries to switch off the deleted branch and fails on a detached
            # HEAD cwd) even though the PR did merge. Re-verify before bailing.
            err = r.stderr.strip() or r.stdout.strip()
            logger.debug(
                "_land_gh: gh pr merge exited %d; re-verifying...", r.returncode
            )
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
                    # before re-running `land`.
                    print(
                        f"error: gh pr merge failed: {err}; verification inconclusive: {verify_err}"
                    )
                else:
                    print(f"error: gh pr merge failed: {err}")
                return False
    else:
        print("PR merged.")

    logger.info("_land_gh: merged — fast-forwarding main in %s", cwd)

    _fast_forward_main(cwd)
    _remove_scratch(gremlin_id)
    _finalize_cleanup(gremlin_id, wdir, state, cwd, remove_state_dir=False)
    return True


def _load_pipeline_land_stage(state: dict[str, Any]):
    """Load and return the pipeline's land: exec stage, or None if not present."""
    from _gremlins_core.discovery import resolve_pipeline_path
    from _gremlins_core.schemas import Pipeline

    pipeline_path = str(state.get("pipeline_path") or "")
    _pr = str(state.get("project_root") or "")
    if not pipeline_path:
        return None
    try:
        p = resolve_pipeline_path(pipeline_path)
        pipeline = Pipeline.from_yaml(p)
        return pipeline.land
    except Exception as exc:
        sys.stderr.write(f"warning: failed to load land stage: {exc}\n")
        return None


def _exec_land_stage(land_stage: Any, gremlin: Any) -> bool:
    """Run an exec land stage against the given gremlin. Returns True on success."""
    import asyncio

    from _gremlins_core.stages import Bail

    try:
        asyncio.run(land_stage.run(gremlin))
        return True
    except Bail as b:
        print(f"error: land: {b.reason}")
        return False
    except Exception as e:
        print(f"error: land stage failed: {e}")
        return False


def _land_with_stage(
    gremlin_id: str,
    wdir: str,
    state: dict[str, Any],
    land_stage: Any,
) -> bool:
    """Run the pipeline's land: stage as the merge step, with shared teardown."""
    from gremlins.executor.gremlin import Gremlin

    project_root = _resolve_landing_cwd(state)
    cwd = project_root if project_root and os.path.isdir(project_root) else None

    workdir = state.get("workdir") or ""
    if _inside_worktree(workdir):
        print("you are inside this gremlin's worktree — cd elsewhere before landing")
        return False

    gremlin = Gremlin.open(gremlin_id)
    gremlin.state = gremlin.build_state_with_cwd(cwd or "")
    _remove_worktree(wdir, state, cwd)

    logger.info("_land_with_stage: cwd=%s land_stage=%s", cwd or ".", land_stage.name)

    if not _exec_land_stage(land_stage, gremlin):
        return False

    print("Landed.")
    logger.info("_land_with_stage: done — setup_kind=%s", state.get("setup_kind", ""))
    _print_cost(state)

    setup_kind = state.get("setup_kind", "")
    if setup_kind in ("worktree-detached", "worktree-detached-from-ref"):
        _fast_forward_main(cwd)

    _remove_scratch(gremlin_id)
    _finalize_cleanup(gremlin_id, wdir, state, cwd, remove_state_dir=False)
    return True


def do_land(
    target: str, force: bool = False, mode: str | None = None, into_dir: str = ""
) -> bool:
    match = resolve_gremlin(target)
    if match is None:
        return False

    gremlin_id, sf, wdir = match
    state = load_state(sf)
    if not state:
        print(f"error: could not read state for {gremlin_id}")
        return False

    logger.info("land: resolved %s -> %s", target, gremlin_id)

    live = liveness_of_state_file(sf, state)
    if live == "running" or live.startswith("stalled:"):
        print(
            f"gremlin {gremlin_id} is still live ({live}) — use 'stop' first, then land"
        )
        return False

    logger.info("land: liveness=%s", live)

    shape = landable_shape(state)

    logger.debug(
        "land: state loaded — client=%s setup_kind=%s project_root=%s pipeline=%s",
        state.get("client") or "",
        state.get("setup_kind") or "",
        state.get("project_root") or "",
        state.get("pipeline_path") or "",
    )

    logger.info("land: shape=%s", shape)

    # Source env from the persisted pipeline's bootstrap block.
    raw: object | None = state.get("project_root")
    project_root = str(raw) if raw else ""
    pipeline_path = str(state.get("pipeline_path") or "")
    if pipeline_path and project_root:
        from _gremlins_core.discovery import resolve_pipeline_path
        from _gremlins_core.schemas import Bootstrap

        from gremlins.utils.yaml_io import YamlLoadError, load_yaml_file

        try:
            if project_root:
                os.environ["GREMLINS_PROJECT_ROOT"] = project_root
                try:
                    p = resolve_pipeline_path(pipeline_path)
                finally:
                    del os.environ["GREMLINS_PROJECT_ROOT"]
            else:
                p = resolve_pipeline_path(pipeline_path)
            raw_yaml = load_yaml_file(p)
            bootstrap = Bootstrap.from_yaml(raw_yaml.get("bootstrap"))
            env_script = bootstrap.env
        except (FileNotFoundError, YamlLoadError, ValueError, OSError) as exc:
            print(f"warning: could not source bootstrap env: {exc}", flush=True)
            env_script = ""
    else:
        env_script = ""

    if env_script:
        from gremlins.env_file import source_env_string

        try:
            env_vars = source_env_string(
                env_script,
                base_env=dict(os.environ),
                cwd=pathlib.Path(project_root),
            )
            os.environ.update(env_vars)
        except Exception as exc:
            print(f"warning: could not source bootstrap env: {exc}", flush=True)
        else:
            logger.debug("land: bootstrap env sourced (%d chars)", len(env_script))

    # Resolve the model client this gremlin used so commit-message synthesis
    # goes through the same backend as the pipeline stages.
    client_str: str = str(state.get("client") or "")
    if not client_str:
        print(
            "error: state.json is missing client field — cannot determine which model to use for commit-message synthesis"
        )
        return False
    try:
        client: Client = Client.parse(client_str)
    except Exception as exc:
        print(f"error: cannot parse client from state.json: {exc}")
        return False

    logger.info("land: client=%s", client_str)

    if shape in ("empty", "one_branch"):
        artifact_dir = pathlib.Path(_scratch_root_fn(gremlin_id)) / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)
        registry = ArtifactRegistry(artifact_dir=artifact_dir)
        has_pr = registry.is_registered("artifact://pr")
        logger.debug("land: registry check — exists(artifact://pr)=%s", has_pr)
        if has_pr:
            shape = "one_pr"

    if shape == "many_prs":
        print("error: stacked PR series — merge in order on GitHub")
        logger.info("land: many_prs — error (stacked PR series)")
        return False

    if shape == "one_pr":
        if mode is not None:
            print(
                "error: --squash/--ff are not applicable to gh gremlins (merged via PR)"
            )
            return False
        land_stage = _load_pipeline_land_stage(cast(dict[str, Any], state))
        if land_stage is not None:
            logger.info("land: dispatching to _land_with_stage (one_pr + land stage)")
            return _land_with_stage(
                gremlin_id, wdir, cast(dict[str, Any], state), land_stage
            )
        logger.info("land: dispatching to _land_gh (one_pr)")
        return _land_gh(gremlin_id, wdir, state, force=force)

    # shape == "empty": only boss gremlins (worktree-detached) have commits to land
    if state.get("setup_kind") != "worktree-detached":
        print(f"error: gremlin {gremlin_id} has no PR artifacts to land")
        return False
    if live != "finished":
        print(f"gremlin {gremlin_id} is not finished (liveness: {live})")
        return False
    logger.info("land: dispatching to _land_boss mode=%s (empty)", mode or "ff")
    return _land_boss(gremlin_id, sf, wdir, state, mode or "ff", client)
