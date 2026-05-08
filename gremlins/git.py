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
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    if capture:
        r: subprocess.CompletedProcess[str] = subprocess.run(
            ["git"] + args, cwd=cwd, capture_output=True, text=True, timeout=timeout
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
            timeout=timeout,
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


def head_sha(cwd: str | os.PathLike[str] | None = None) -> str:
    r = _run_git(["rev-parse", "HEAD"], cwd=cwd, check=False)
    return r.stdout.strip() if r.returncode == 0 else ""


def status_porcelain(cwd: str | os.PathLike[str] | None = None) -> str:
    """Return raw output of `git status --porcelain`, or '' on error."""
    try:
        r = _run_git(["status", "--porcelain"], cwd=cwd, check=False)
        return r.stdout
    except OSError:
        return ""


def has_dirty_worktree(cwd: str | os.PathLike[str] | None = None) -> bool:
    return bool(status_porcelain(cwd).strip())


def has_commits(cwd: str | os.PathLike[str] | None = None) -> bool:
    try:
        r = _run_git(["rev-list", "--count", "HEAD"], cwd=cwd, check=False)
        return r.returncode == 0 and int(r.stdout.strip() or "0") > 0
    except OSError:
        return False


def rev_exists(rev: str, cwd: str | os.PathLike[str] | None = None) -> bool:
    try:
        r = _run_git(
            ["rev-parse", "--verify", rev], cwd=cwd, check=False, capture=False
        )
        return r.returncode == 0
    except OSError:
        return False


def has_diff(ref_a: str, ref_b: str, cwd: str | os.PathLike[str] | None = None) -> bool:
    try:
        r = _run_git(
            ["diff", "--quiet", ref_a, ref_b], cwd=cwd, check=False, capture=False
        )
        return r.returncode != 0
    except OSError:
        return False


def current_branch(cwd: str | os.PathLike[str] | None = None) -> str:
    """Return current branch name, or '' for detached HEAD or on error."""
    try:
        r = _run_git(["rev-parse", "--abbrev-ref", "HEAD"], cwd=cwd, check=False)
    except OSError:
        return ""
    if r.returncode != 0:
        return ""
    branch = r.stdout.strip()
    return "" if branch == "HEAD" else branch


def resolve_base_ref(
    name: str, *, cwd: str | os.PathLike[str] | None = None
) -> tuple[str, str]:
    """Resolve a symbolic ref name to (sym_name, sha)."""
    if name == "current":
        sha = head_sha(cwd=cwd)
        if not sha:
            raise GitError(128, "could not resolve HEAD: no commits")
        branch = current_branch(cwd=cwd)
        return (branch if branch else sha), sha

    for refpath in (
        f"refs/heads/{name}",
        f"refs/remotes/{name}",
        f"refs/tags/{name}",
        name,  # raw SHA or other direct ref
    ):
        r = _run_git(["rev-parse", "--verify", refpath], cwd=cwd, check=False)
        if r.returncode == 0:
            return name, r.stdout.strip()

    raise GitError(
        128, f"base_ref {name!r} does not resolve to a branch, tag, or commit"
    )


def fetch_origin(
    branch: str,
    *,
    cwd: str | os.PathLike[str] | None = None,
    timeout: float | None = None,
) -> None:
    """Fetch refs/heads/<branch> from origin into refs/remotes/origin/<branch>. Raises GitError."""
    refspec = f"refs/heads/{branch}:refs/remotes/origin/{branch}"
    _run_git(["fetch", "origin", refspec], cwd=cwd, timeout=timeout)


def remote_ref_sha(ref: str, cwd: str | os.PathLike[str] | None = None) -> str:
    """Return the SHA of <ref> (e.g. refs/remotes/origin/main). Raises GitError."""
    r = _run_git(["rev-parse", ref], cwd=cwd)
    return r.stdout.strip()


def diff_output(
    args: list[str] | None = None,
    *,
    cwd: str | os.PathLike[str] | None = None,
) -> str:
    """Return stdout of `git diff [args]`. Raises GitError on failure."""
    r = _run_git(["diff"] + (args or []), cwd=cwd)
    return r.stdout


def log_patch(rev_range: str, *, cwd: str | os.PathLike[str] | None = None) -> str:
    """Return stdout of `git log --patch <rev_range>`. Raises GitError on failure."""
    r = _run_git(["log", "--patch", rev_range], cwd=cwd)
    return r.stdout


def is_ancestor(
    ref_a: str, ref_b: str, *, cwd: str | os.PathLike[str] | None = None
) -> bool:
    try:
        r = _run_git(
            ["merge-base", "--is-ancestor", ref_a, ref_b],
            cwd=cwd,
            check=False,
            capture=False,
        )
        return r.returncode == 0
    except OSError:
        return False


def merge_base(
    ref_a: str, ref_b: str, *, cwd: str | os.PathLike[str] | None = None
) -> str:
    r = _run_git(["merge-base", ref_a, ref_b], cwd=cwd)
    return r.stdout.strip()


def rev_list_count(rev_range: str, *, cwd: str | os.PathLike[str] | None = None) -> int:
    r = _run_git(["rev-list", "--count", rev_range], cwd=cwd)
    return int(r.stdout.strip())


def squash_merge(ref: str, *, cwd: str | os.PathLike[str] | None = None) -> None:
    _run_git(["merge", "--squash", ref], cwd=cwd)


def reset_hard(ref: str = "HEAD", *, cwd: str | os.PathLike[str] | None = None) -> None:
    _run_git(["reset", "--hard", ref], cwd=cwd)


def clean_fd(*, cwd: str | os.PathLike[str] | None = None) -> None:
    try:
        _run_git(["clean", "-fd"], cwd=cwd, check=False)
    except Exception:
        pass


def commit(message: str, *, cwd: str | os.PathLike[str] | None = None) -> None:
    _run_git(["commit", "-m", message], cwd=cwd)


def ff_merge(ref: str, *, cwd: str | os.PathLike[str] | None = None) -> None:
    _run_git(["merge", "--ff-only", ref], cwd=cwd)


def branch_exists(branch: str, *, cwd: str | os.PathLike[str] | None = None) -> bool:
    try:
        r = _run_git(
            ["show-ref", "--verify", "--quiet", f"refs/heads/{branch}"],
            cwd=cwd,
            check=False,
            capture=False,
        )
        return r.returncode == 0
    except OSError:
        return False


def delete_branch(
    branch: str, *, force: bool = False, cwd: str | os.PathLike[str] | None = None
) -> None:
    _run_git(["branch", "-D" if force else "-d", branch], cwd=cwd)


def try_fetch_all(
    remote: str = "origin",
    *,
    cwd: str | os.PathLike[str] | None = None,
    timeout: float | None = None,
) -> bool:
    try:
        r = _run_git(["fetch", remote], cwd=cwd, check=False, timeout=timeout)
        return r.returncode == 0
    except Exception:
        return False


def force_update_branch(
    branch: str, target: str, *, cwd: str | os.PathLike[str] | None = None
) -> None:
    _run_git(["branch", "-f", branch, target], cwd=cwd)


def log_oneline(rev_range: str, *, cwd: str | os.PathLike[str] | None = None) -> str:
    try:
        r = _run_git(["log", "--oneline", rev_range], cwd=cwd, check=False)
        return r.stdout.strip()
    except Exception:
        return ""


def diff_stat(rev_range: str, *, cwd: str | os.PathLike[str] | None = None) -> str:
    try:
        r = _run_git(["diff", "--stat", rev_range], cwd=cwd, check=False)
        return r.stdout.strip()
    except Exception:
        return ""


def ls_others(*, cwd: str | os.PathLike[str] | None = None) -> str:
    try:
        r = _run_git(
            ["ls-files", "--others", "--exclude-standard"], cwd=cwd, check=False
        )
        return r.stdout.strip()
    except Exception:
        return ""


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


def setup_worktree_branch(
    project_root: str,
    gr_id: str,
    base_ref: str = "HEAD",
    branch_prefix: str = "bg/local",
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


def toplevel(cwd: str | os.PathLike[str] | None = None) -> str:
    """Return the absolute path of the git toplevel. Raises GitError on failure."""
    r = _run_git(["rev-parse", "--show-toplevel"], cwd=cwd)
    return r.stdout.strip()


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
