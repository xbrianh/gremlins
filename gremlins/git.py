"""Git helpers used across pipeline stages.

Conventions:
- Predicates return bool and never raise.
- Value-returning helpers raise GitError on failure. _git-based helpers
  (record_pre_impl_state, create_handoff_branch, etc.) still raise RuntimeError
  pending migration to _run_git.
- Best-effort helpers swallow errors and use a try_ prefix.
- Every helper accepts cwd: str | os.PathLike | None = None.
- _run_git defaults to check=True and raises GitError on non-zero exit.
"""

from __future__ import annotations

import dataclasses
import os
import shutil
import subprocess
import sys
import tempfile
from typing import Any, cast


class GitError(Exception):
    def __init__(self, returncode: int, stderr: str) -> None:
        super().__init__(f"git exited {returncode}: {stderr}")
        self.returncode = returncode
        self.stderr = stderr


def _run_git(
    args: list[str],
    *,
    cwd: str | os.PathLike[str] | None = None,
    check: bool = True,
    capture: bool = True,
) -> subprocess.CompletedProcess[str]:
    if capture:
        r: subprocess.CompletedProcess[str] = subprocess.run(
            ["git"] + args, cwd=cwd, capture_output=True, text=True
        )
        if check and r.returncode != 0:
            raise GitError(r.returncode, r.stderr.strip())
        return r
    else:
        rr = subprocess.run(
            ["git"] + args,
            cwd=cwd,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        if check and rr.returncode != 0:
            raise GitError(rr.returncode, rr.stderr.strip())
        return subprocess.CompletedProcess(rr.args, rr.returncode, "", rr.stderr)


def in_git_repo(cwd: str | os.PathLike[str] | None = None) -> bool:
    try:
        r = _run_git(["rev-parse", "--git-dir"], cwd=cwd, check=False, capture=False)
        return r.returncode == 0
    except OSError:
        return False


def git_head(cwd: str | os.PathLike[str] | None = None) -> str:
    r = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        check=False,
        cwd=cwd,
    )
    return r.stdout.strip() if r.returncode == 0 else ""


def git_head_of_workdir(workdir: str) -> str:
    r = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        check=False,
        cwd=workdir,
    )
    return r.stdout.strip() if r.returncode == 0 else ""


# ---------------------------------------------------------------------------
# ghgremlin impl-handoff branch lifecycle (Phase 3)
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class PreImplState:
    """Git state captured before the implement stage runs."""

    head: str
    branch: str  # empty string when HEAD is detached (the launch.sh worktree default)


@dataclasses.dataclass
class EmptyImpl:
    """HEAD unchanged and worktree clean — no implementation work produced."""


@dataclasses.dataclass
class DirtyOnly:
    """HEAD unchanged but worktree has uncommitted changes."""


@dataclasses.dataclass
class HeadAdvanced:
    """HEAD advanced fast-forward from pre-impl state."""

    commit_count: int


@dataclasses.dataclass
class DivergentHead:
    """HEAD changed but is not a fast-forward of the pre-impl HEAD."""

    pre_head: str
    post_head: str


ImplOutcome = EmptyImpl | DirtyOnly | HeadAdvanced | DivergentHead


# being phased out in favour of _run_git
def _git(
    args: list[str], *, cwd: str | None = None, **kwargs: Any
) -> subprocess.CompletedProcess[Any]:
    return cast(
        subprocess.CompletedProcess[Any],
        subprocess.run(["git"] + list(args), cwd=cwd, **kwargs),
    )


def record_pre_impl_state(cwd: str | None = None) -> PreImplState:
    """Capture HEAD commit and symbolic branch ref before running the implement stage."""
    head_r = _git(
        ["rev-parse", "HEAD"], cwd=cwd, capture_output=True, text=True, check=False
    )
    head = head_r.stdout.strip() if head_r.returncode == 0 else ""
    if not head:
        raise RuntimeError("could not resolve HEAD before implement stage")
    branch_r = _git(
        ["symbolic-ref", "--short", "HEAD"],
        cwd=cwd,
        capture_output=True,
        text=True,
        check=False,
    )
    branch = branch_r.stdout.strip() if branch_r.returncode == 0 else ""
    return PreImplState(head=head, branch=branch)


def classify_impl_outcome(pre: PreImplState, cwd: str | None = None) -> ImplOutcome:
    """Classify post-implement git state into one of the four outcome types."""
    head_r = _git(
        ["rev-parse", "HEAD"], cwd=cwd, capture_output=True, text=True, check=False
    )
    post_head = head_r.stdout.strip() if head_r.returncode == 0 else ""

    if post_head and post_head != pre.head:
        ancestor_r = _git(
            ["merge-base", "--is-ancestor", pre.head, post_head],
            cwd=cwd,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if ancestor_r.returncode == 0:
            count_r = _git(
                ["rev-list", "--count", f"{pre.head}..HEAD"],
                cwd=cwd,
                capture_output=True,
                text=True,
                check=False,
            )
            count = int(count_r.stdout.strip() or "0") if count_r.returncode == 0 else 0
            return HeadAdvanced(commit_count=count)
        return DivergentHead(pre_head=pre.head, post_head=post_head)

    status_r = _git(
        ["status", "--porcelain"], cwd=cwd, capture_output=True, text=True, check=False
    )
    if status_r.stdout.strip():
        return DirtyOnly()
    return EmptyImpl()


def create_handoff_branch(pre: PreImplState, cwd: str | None = None) -> str:
    """Create a ghgremlin-impl-handoff-<pid> branch at current HEAD.

    Returns the branch name. Raises if the branch already exists or git switch fails.
    The PID suffix scopes the name to this process so concurrent gremlins in the
    same repo don't collide.
    """
    handoff = f"ghgremlin-impl-handoff-{os.getpid()}"
    check_r = _git(
        ["show-ref", "--verify", "--quiet", f"refs/heads/{handoff}"],
        cwd=cwd,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    if check_r.returncode == 0:
        raise RuntimeError(
            f"hand-off branch {handoff} already exists; refusing to clobber"
        )
    switch_r = _git(
        ["switch", "-c", handoff], cwd=cwd, capture_output=True, text=True, check=False
    )
    if switch_r.returncode != 0:
        raise RuntimeError(
            f"could not create hand-off branch {handoff}: {switch_r.stderr.strip()}"
        )
    return handoff


def reset_pre_branch(pre: PreImplState, cwd: str | None = None) -> None:
    """Reset the pre-impl branch ref back to pre.head.

    No-op when HEAD was detached at impl start (pre.branch is empty), which is
    the normal case under launch.sh's detached worktree. Under direct invocation
    from a named branch this resets that branch to PRE_HEAD so implementation
    commits don't leak onto the chain's start ref (intentional destructive reset).
    """
    if not pre.branch:
        return
    r = _git(
        ["branch", "-f", pre.branch, pre.head],
        cwd=cwd,
        capture_output=True,
        text=True,
        check=False,
    )
    if r.returncode != 0:
        raise RuntimeError(
            f"could not reset {pre.branch} to {pre.head}: {r.stderr.strip()}"
        )


def sweep_stale_handoff_branches(handoff_branch: str, cwd: str | None = None) -> None:
    """Delete ghgremlin-impl-handoff-* branches from prior failed runs that are
    already merged into HEAD. Leaves divergent ones in place with a warning.

    Called after create_handoff_branch so HEAD is on the new handoff branch and
    git branch -d (which refuses to delete the current branch) can safely delete
    stale branches that are ancestors of the current HEAD.
    """
    list_r = _git(
        [
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/ghgremlin-impl-handoff-*",
        ],
        cwd=cwd,
        capture_output=True,
        text=True,
        check=False,
    )
    if list_r.returncode != 0:
        return
    for stale in list_r.stdout.splitlines():
        stale = stale.strip()
        if not stale or stale == handoff_branch:
            continue
        ancestor_r = _git(
            ["merge-base", "--is-ancestor", stale, "HEAD"],
            cwd=cwd,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if ancestor_r.returncode == 0:
            _git(
                ["branch", "-d", stale],
                cwd=cwd,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
        else:
            sys.stderr.write(
                f"warning: leaving divergent hand-off branch {stale} in place "
                "(unique commits would be lost)\n"
            )
            sys.stderr.flush()


def is_git_repo(path: str) -> bool:
    """Return True if `path` is inside a git repository."""
    try:
        r = _run_git(["-C", path, "rev-parse", "--git-dir"], check=False, capture=False)
        return r.returncode == 0
    except OSError:
        return False


def resolve_default_branch(project_root: str) -> str:
    """Resolve origin's default branch via gh CLI. Raises RuntimeError on failure."""
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
            timeout=30,
        )
    except FileNotFoundError:
        raise RuntimeError("gh CLI not found on PATH")
    except subprocess.TimeoutExpired:
        raise RuntimeError("gh repo view timed out after 30s")
    if r.returncode != 0 or not r.stdout.strip():
        raise RuntimeError(f"gh repo view failed: {r.stderr.strip() or 'empty output'}")
    return r.stdout.strip()


def setup_worktree_branch(
    project_root: str,
    gr_id: str,
    base_ref: str = "HEAD",
    branch_prefix: str = "bg/localgremlin",
) -> tuple[str, str]:
    """Add a named-branch worktree at base_ref. Returns (workdir_path, branch_name).

    Raises RuntimeError on failure.
    """
    workdir = tempfile.mkdtemp(prefix="aibg-localgremlin.")
    os.rmdir(workdir)
    branch = f"{branch_prefix}/{gr_id}"
    r = _git(
        ["worktree", "add", "-b", branch, workdir, base_ref],
        cwd=project_root,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise RuntimeError(f"git worktree add -b {branch!r} failed: {r.stderr.strip()}")
    return workdir, branch


def setup_detached_worktree(project_root: str, base_ref: str) -> str:
    """Add a detached worktree at base_ref. Returns the worktree path.

    Raises RuntimeError on failure.
    """
    workdir = tempfile.mkdtemp(prefix="aibg-gremlin.")
    os.rmdir(workdir)
    r = _git(
        ["worktree", "add", "--detach", workdir, base_ref],
        cwd=project_root,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise RuntimeError(
            f"git worktree add --detach {base_ref!r} failed: {r.stderr.strip()}"
        )
    return workdir


def setup_copy(project_root: str) -> str:
    """Non-git fallback: copy project root into a fresh temp dir. Returns workdir path."""
    workdir = tempfile.mkdtemp(prefix="aibg-gremlin.")
    shutil.copytree(project_root, workdir, dirs_exist_ok=True)
    return workdir


def remove_worktree(project_root: str, workdir: str) -> None:
    """Remove a git worktree and prune stale entries. Best-effort; never raises."""
    try:
        _git(
            ["worktree", "remove", "--force", workdir],
            cwd=project_root,
            capture_output=True,
            check=False,
        )
        _git(["worktree", "prune"], cwd=project_root, capture_output=True, check=False)
    except Exception:
        pass
