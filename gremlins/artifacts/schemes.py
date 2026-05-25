"""Concrete SchemeResolver implementations for file://, git://, and gh:// URIs."""

from __future__ import annotations

import dataclasses
import pathlib
import re
from typing import Any

from gremlins.artifacts.uri import Uri
from gremlins.utils import git as git_utils
from gremlins.utils import github as gh_utils
from gremlins.utils import proc


@dataclasses.dataclass(frozen=True)
class PrInfo:
    url: str
    number: int
    branch: str


@dataclasses.dataclass(frozen=True)
class IssueInfo:
    url: str
    number: int


class FileSessionResolver:
    """Resolves file://session/<name> against a fixed session directory."""

    def __init__(self, session_dir: pathlib.Path) -> None:
        self._session_dir = session_dir

    def _path(self, uri: Uri) -> pathlib.Path:
        if uri.path.startswith("/"):
            return pathlib.Path(uri.path).resolve()
        if not uri.path.startswith("session/"):
            raise ValueError(f"file:// URI must start with 'session/': {uri}")
        name = uri.path[len("session/") :]
        p = (self._session_dir / name).resolve()
        base = self._session_dir.resolve()
        try:
            p.relative_to(base)
        except ValueError:
            raise ValueError(f"path escapes session directory: {uri}") from None
        return p

    def read(self, uri: Uri) -> bytes:
        try:
            return self._path(uri).read_bytes()
        except FileNotFoundError:
            return b""

    def verify_produced(self, uri: Uri) -> None:
        p = self._path(uri)
        if not p.exists() or p.stat().st_size == 0:
            raise FileNotFoundError(f"artifact file missing or empty: {p}")


class GitResolver:
    """Resolves git://range/<base>..<head>, git://ref/<name>, git://commit/<sha>."""

    def __init__(self, cwd: pathlib.Path | None = None) -> None:
        self._cwd = cwd

    def read(self, uri: Uri) -> Any:
        path = uri.path
        if path.startswith("range/"):
            range_str = path.removeprefix("range/")
            out = proc.run_or_raise(
                ["git", "log", "--format=%H %s", range_str], cwd=self._cwd
            )
            commits: list[dict[str, str]] = []
            for line in out.splitlines():
                sha, _, subject = line.partition(" ")
                commits.append({"sha": sha, "subject": subject})
            return commits
        if path.startswith("ref/"):
            name = path.removeprefix("ref/")
            return proc.run_or_raise(["git", "rev-parse", name], cwd=self._cwd)
        if path.startswith("commit/"):
            sha = path.removeprefix("commit/")
            return proc.run_or_raise(["git", "rev-parse", "--verify", sha], cwd=self._cwd)
        raise ValueError(f"unrecognised git URI path: {uri}")

    def verify_produced(self, uri: Uri) -> None:
        # Raises on subprocess error if ref/sha/range doesn't exist
        self.read(uri)


def snapshot_head_before(cwd: pathlib.Path | None = None) -> str:
    """Return current HEAD sha for use with ArtifactRegistry.bind_git_commit_range()."""
    sha = git_utils.head_sha(cwd=cwd)
    if not sha:
        raise RuntimeError("could not resolve HEAD")
    return sha


_PR_URL_RE = re.compile(r"https://github\.com/[^/]+/[^/]+/pull/(\d+)")


class GitHubResolver:
    """Resolves gh://pr/<n> and gh://issue/<n> via `gh` CLI."""

    def __init__(self, cwd: pathlib.Path | None = None) -> None:
        self._cwd = cwd

    def read(self, uri: Uri) -> Any:
        path = uri.path
        if path.startswith("pr/"):
            n = path.removeprefix("pr/")
            data = gh_utils.view_pr(
                n, project_root=str(self._cwd) if self._cwd else None
            )
            return PrInfo(
                url=data["url"],
                number=data["number"],
                branch=data["headRefName"],
            )
        if path.startswith("issue/"):
            n = path.removeprefix("issue/")
            repo = gh_utils.current_repo()
            data = gh_utils.view_issue(n, repo)
            return data.get("body") or ""
        raise ValueError(f"unrecognised gh URI path: {uri}")

    def verify_produced(self, uri: Uri) -> None:
        self.read(uri)

    def capture(self, stdout: str, _: str) -> Uri:
        """Parse a gh://pr/<n> URI from `gh pr create` stdout."""
        m = _PR_URL_RE.search(stdout)
        if not m:
            raise ValueError(f"no PR URL found in gh pr create output: {stdout!r}")
        n = m.group(1)
        return Uri.parse(f"gh://pr/{n}")
