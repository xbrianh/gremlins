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
import secrets
import shutil

from gremlins import paths
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
class Commit:
    sha: str
    subject: str


def commits_since(
    ref: str, *, cwd: str | os.PathLike[str] | None = None
) -> list[Commit]:
    """Return commits reachable from HEAD but not from ref, oldest first."""
    r = _run_git(["log", f"{ref}..HEAD", "--format=%H %s", "--reverse"], cwd=cwd)
    result: list[Commit] = []
    for line in r.stdout.splitlines():
        sha, _, subject = line.partition(" ")
        if sha:
            result.append(Commit(sha=sha, subject=subject))
    return result


def checkout_detach(ref: str, *, cwd: str | os.PathLike[str] | None = None) -> None:
    """Detach HEAD to <ref>. Raises GitError on failure."""
    _run_git(["checkout", "--detach", ref], cwd=cwd)


def setup_detached_worktree(
    project_root: str,
    base_ref: str,
    *,
    worktree_parent: pathlib.Path | None = None,
) -> str:
    """Add a detached worktree at base_ref. Returns the worktree path."""
    parent = worktree_parent if worktree_parent is not None else paths.work_root()
    parent.mkdir(parents=True, exist_ok=True)
    workdir = str(parent / f"aibg-gremlin.{secrets.token_hex(6)}")
    _run_git(["worktree", "add", "--detach", workdir, base_ref], cwd=project_root)
    return workdir


def setup_detached_from_remote_ref(
    project_root: str,
    ref: str,
    *,
    worktree_parent: pathlib.Path | None = None,
) -> str:
    """Fetch <ref> from origin and add a detached worktree at FETCH_HEAD. Returns the worktree path."""
    _run_git(["fetch", "origin", "--", ref], cwd=project_root)
    return setup_detached_worktree(
        project_root, "FETCH_HEAD", worktree_parent=worktree_parent
    )


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
    src = pathlib.Path(project_root) / paths.OVERLAY_DIRNAME
    dst = pathlib.Path(state_dir) / paths.OVERLAY_DIRNAME
    if src.is_dir() and src.resolve() != dst.resolve():
        shutil.copytree(src, dst, dirs_exist_ok=True)


def setup_named_worktree(
    project_root: str,
    gremlin_id: str,
    base_ref_sha: str,
    *,
    worktree_parent: pathlib.Path | None = None,
) -> tuple[str, str]:
    parent = worktree_parent if worktree_parent is not None else paths.work_root()
    parent.mkdir(parents=True, exist_ok=True)
    workdir = str(parent / gremlin_id)
    branch = f"bg/local/{gremlin_id}"
    _run_git(
        ["worktree", "add", "-b", branch, workdir, base_ref_sha or "HEAD"],
        cwd=project_root,
    )
    return workdir, branch


async def in_git_repo_async(cwd: str | os.PathLike[str] | None = None) -> bool:
    try:
        return await proc.run_ok_async(["git", "rev-parse", "--git-dir"], cwd=cwd)
    except OSError:
        return False


async def head_sha_async(cwd: str | os.PathLike[str] | None = None) -> str:
    r = await proc.run_async(["git", "rev-parse", "HEAD"], cwd=cwd)
    return r.stdout.strip() if r.returncode == 0 else ""


async def status_porcelain_async(cwd: str | os.PathLike[str] | None = None) -> str:
    try:
        r = await proc.run_async(["git", "status", "--porcelain"], cwd=cwd)
        return r.stdout
    except OSError:
        return ""


async def setup_detached_worktree_async(
    project_root: str,
    base_ref: str,
    *,
    worktree_parent: pathlib.Path | None = None,
) -> str:
    """Add a detached worktree at base_ref. Returns the worktree path."""
    parent = worktree_parent if worktree_parent is not None else paths.work_root()
    parent.mkdir(parents=True, exist_ok=True)
    workdir = str(parent / f"aibg-gremlin.{secrets.token_hex(6)}")
    r = await proc.run_async(
        ["git", "worktree", "add", "--detach", workdir, base_ref], cwd=project_root
    )
    if r.returncode != 0:
        raise GitError(r.returncode, r.stderr.strip())
    return workdir


async def remove_worktree_async(project_root: str, workdir: str) -> None:
    """Remove a git worktree. Best-effort; never raises."""
    try:
        await proc.run_quiet_async(
            ["git", "worktree", "remove", "--force", workdir], cwd=project_root
        )
    except Exception:
        pass


async def remove_worktrees_async(project_root: str, paths: list[str]) -> None:
    """Remove worktrees in bulk and prune stale entries. No-op outside a repo."""
    if not await in_git_repo_async(cwd=project_root):
        return
    for wt in paths:
        await remove_worktree_async(project_root, wt)
    await prune_worktrees_async(project_root)


async def prune_worktrees_async(project_root: str) -> None:
    """Prune stale worktree entries. No-op outside a repo."""
    if not await in_git_repo_async(cwd=project_root):
        return
    try:
        await proc.run_quiet_async(["git", "worktree", "prune"], cwd=project_root)
    except Exception:
        pass


def setup_workdir(
    setup_kind: str,
    project_root: str,
    base_ref_sha: str,
    gremlin_id: str,
    state_dir: os.PathLike[str],
    *,
    worktree_parent: pathlib.Path | None = None,
) -> tuple[str, str, str, str]:
    if not in_git_repo(cwd=project_root):
        raise GitError(128, f"{project_root!r} is not a git repository")

    if setup_kind == "worktree-branch":
        workdir, branch = setup_named_worktree(
            project_root, gremlin_id, base_ref_sha, worktree_parent=worktree_parent
        )
        stage_gremlins_overlay(project_root, state_dir)
        return workdir, branch, "", "worktree-branch"

    if setup_kind == "worktree-detached-from-ref":
        workdir = setup_detached_from_remote_ref(
            project_root, base_ref_sha, worktree_parent=worktree_parent
        )
        stage_gremlins_overlay(project_root, state_dir)
        return workdir, "", base_ref_sha, "worktree-detached-from-ref"

    if setup_kind not in ("worktree", "worktree-detached", "local"):
        raise ValueError(f"unknown setup_kind: {setup_kind!r}")

    workdir = setup_detached_worktree(
        project_root, base_ref_sha or "HEAD", worktree_parent=worktree_parent
    )
    stage_gremlins_overlay(project_root, state_dir)
    return workdir, "", base_ref_sha, "worktree"
