"""Git helpers used across pipeline stages.

Conventions:
- Predicates return bool and never raise.
- Value-returning helpers raise GitError on failure.
- Best-effort helpers swallow errors and use a try_ prefix.
- Every helper accepts cwd: str | os.PathLike | None = None.
- _run_git defaults to check=True and raises GitError on non-zero exit.
"""

from __future__ import annotations

import dataclasses
import os
import pathlib
import shutil
import tempfile

from gremlins.utils import proc


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
    timeout: float | None = None,
):
    r = proc.run(["git"] + args, cwd=cwd, timeout=timeout)
    if check and r.returncode != 0:
        raise GitError(r.returncode, r.stderr.strip())
    return r


def in_git_repo(cwd: str | os.PathLike[str] | None = None) -> bool:
    try:
        return proc.run_ok(["git", "rev-parse", "--git-dir"], cwd=cwd)
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
        return proc.run_ok(["git", "rev-parse", "--verify", rev], cwd=cwd)
    except OSError:
        return False


def has_diff(ref_a: str, ref_b: str, cwd: str | os.PathLike[str] | None = None) -> bool:
    try:
        return not proc.run_ok(["git", "diff", "--quiet", ref_a, ref_b], cwd=cwd)
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
        return proc.run_ok(
            ["git", "merge-base", "--is-ancestor", ref_a, ref_b], cwd=cwd
        )
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
        return proc.run_ok(
            ["git", "show-ref", "--verify", "--quiet", f"refs/heads/{branch}"], cwd=cwd
        )
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


@dataclasses.dataclass
class PreImplState:
    """Git state captured before the implement stage runs."""

    head: str


@dataclasses.dataclass
class EmptyImpl:
    """HEAD unchanged and worktree clean — no implementation work produced."""


@dataclasses.dataclass
class HeadAdvanced:
    """HEAD advanced fast-forward from pre-impl state."""

    commit_count: int


@dataclasses.dataclass
class DivergentHead:
    """HEAD changed but is not a fast-forward of the pre-impl HEAD."""

    pre_head: str
    post_head: str


ImplOutcome = EmptyImpl | HeadAdvanced | DivergentHead


def record_pre_impl_state(cwd: str | None = None) -> PreImplState:
    """Capture HEAD commit before running the implement stage."""
    head_r = proc.run(["git", "rev-parse", "HEAD"], cwd=cwd)
    head = head_r.stdout.strip() if head_r.returncode == 0 else ""
    if not head:
        raise RuntimeError("could not resolve HEAD before implement stage")
    return PreImplState(head=head)


def classify_impl_outcome(pre: PreImplState, cwd: str | None = None) -> ImplOutcome:
    """Classify post-implement git state into one of three outcome types."""
    head_r = proc.run(["git", "rev-parse", "HEAD"], cwd=cwd)
    post_head = head_r.stdout.strip() if head_r.returncode == 0 else ""

    if post_head and post_head != pre.head:
        if proc.run_ok(
            ["git", "merge-base", "--is-ancestor", pre.head, post_head], cwd=cwd
        ):
            count_r = proc.run(
                ["git", "rev-list", "--count", f"{pre.head}..HEAD"], cwd=cwd
            )
            count = int(count_r.stdout.strip() or "0") if count_r.returncode == 0 else 0
            return HeadAdvanced(commit_count=count)
        return DivergentHead(pre_head=pre.head, post_head=post_head)

    if has_dirty_worktree(cwd=cwd):
        raise RuntimeError(
            "implement left uncommitted changes but made no commits — "
            "stage all changes and commit before proceeding"
        )
    return EmptyImpl()


def checkout_detach(ref: str, *, cwd: str | os.PathLike[str] | None = None) -> None:
    """Detach HEAD to <ref>. Raises GitError on failure."""
    _run_git(["checkout", "--detach", ref], cwd=cwd)


def git_detach_to_branch(
    branch: str, *, cwd: str | os.PathLike[str] | None = None
) -> None:
    fetch_origin(branch, cwd=cwd)
    checkout_detach(f"origin/{branch}", cwd=cwd)


def setup_detached_worktree(project_root: str, base_ref: str) -> str:
    """Add a detached worktree at base_ref. Returns the worktree path.

    Raises RuntimeError on failure.
    """
    workdir = tempfile.mkdtemp(prefix="aibg-gremlin.")
    os.rmdir(workdir)
    r = proc.run(
        ["git", "worktree", "add", "--detach", workdir, base_ref], cwd=project_root
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
        proc.run_quiet(
            ["git", "worktree", "remove", "--force", workdir], cwd=project_root
        )
        proc.run_quiet(["git", "worktree", "prune"], cwd=project_root)
    except Exception:
        pass


def stage_gremlins_overlay(project_root: str, state_dir: os.PathLike[str]) -> None:
    src = pathlib.Path(project_root) / ".gremlins"
    if src.is_dir():
        shutil.copytree(src, pathlib.Path(state_dir) / ".gremlins", dirs_exist_ok=True)


def setup_named_worktree(
    project_root: str, gr_id: str, base_ref_sha: str
) -> tuple[str, str]:
    workdir = tempfile.mkdtemp(prefix="aibg-localgremlin.")
    os.rmdir(workdir)
    branch = f"bg/local/{gr_id}"
    r = proc.run(
        ["git", "worktree", "add", "-b", branch, workdir, base_ref_sha or "HEAD"],
        cwd=project_root,
    )
    if r.returncode != 0:
        raise RuntimeError(f"git worktree add -b {branch!r} failed: {r.stderr.strip()}")
    return workdir, branch


def setup_workdir(
    setup_kind: str,
    project_root: str,
    base_ref_sha: str,
    gr_id: str,
    state_dir: os.PathLike[str],
) -> tuple[str, str, str, str]:
    """Return (workdir, branch, worktree_base, setup_kind)."""
    if not in_git_repo(cwd=project_root):
        return setup_copy(project_root), "", "", "copy"

    if setup_kind == "worktree-branch":
        workdir, branch = setup_named_worktree(project_root, gr_id, base_ref_sha)
        stage_gremlins_overlay(project_root, state_dir)
        return workdir, branch, "", "worktree-branch"

    workdir = setup_detached_worktree(project_root, base_ref_sha or "HEAD")
    stage_gremlins_overlay(project_root, state_dir)
    return workdir, "", base_ref_sha, "worktree"
