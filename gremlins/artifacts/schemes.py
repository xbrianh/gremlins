"""Concrete SchemeResolver implementations for file://, git://, and gh:// URIs."""

from __future__ import annotations

import json
import pathlib
import re
import subprocess
from typing import Any

from gremlins.artifacts.uri import Uri


class FileSessionResolver:
    """Resolves file://session/<name> against a fixed session directory."""

    def __init__(self, session_dir: pathlib.Path) -> None:
        self._session_dir = session_dir

    def _path(self, uri: Uri) -> pathlib.Path:
        if not uri.path.startswith("session/"):
            raise ValueError(f"file:// URI must start with 'session/': {uri}")
        name = uri.path[len("session/"):]
        p = (self._session_dir / name).resolve()
        base = self._session_dir.resolve()
        try:
            p.relative_to(base)
        except ValueError:
            raise ValueError(f"path escapes session directory: {uri}") from None
        return p

    def read(self, uri: Uri) -> bytes:
        return self._path(uri).read_bytes()

    def verify_produced(self, uri: Uri) -> None:
        p = self._path(uri)
        if not p.exists() or p.stat().st_size == 0:
            raise FileNotFoundError(f"artifact file missing or empty: {p}")


class GitResolver:
    """Resolves git://range/<base>..<head>, git://ref/<name>, git://commit/<sha>."""

    def __init__(self, cwd: pathlib.Path | None = None) -> None:
        self._cwd = cwd

    def _run(self, cmd: list[str]) -> str:
        result = subprocess.run(
            cmd, cwd=self._cwd, capture_output=True, text=True, check=True
        )
        return result.stdout.strip()

    def read(self, uri: Uri) -> Any:
        path = uri.path
        if path.startswith("range/"):
            range_str = path.removeprefix("range/")
            out = self._run(["git", "log", "--format=%H %s", range_str])
            commits: list[dict[str, str]] = []
            for line in out.splitlines():
                sha, _, subject = line.partition(" ")
                commits.append({"sha": sha, "subject": subject})
            return commits
        if path.startswith("ref/"):
            name = path.removeprefix("ref/")
            return self._run(["git", "rev-parse", name])
        if path.startswith("commit/"):
            sha = path.removeprefix("commit/")
            out = self._run(["git", "log", "-1", "--format=%H%n%an%n%ae%n%s", sha])
            lines = out.splitlines()
            return {
                "sha": lines[0],
                "author": lines[1],
                "email": lines[2],
                "subject": lines[3],
            }
        raise ValueError(f"unrecognised git URI path: {uri}")

    def verify_produced(self, uri: Uri) -> None:
        # Raises on subprocess error if ref/sha/range doesn't exist
        self.read(uri)


def snapshot_head_before(cwd: pathlib.Path | None = None) -> str:
    """Return current HEAD sha for use with bind_range_after()."""
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=cwd,
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout.strip()


def bind_range_after(
    registry: Any,  # Registry — avoid circular import in type hint
    key: str,
    base_sha: str,
    cwd: pathlib.Path | None = None,
) -> None:
    """Bind key to git://range/<base_sha>..<new_head> after a stage has run."""
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=cwd,
        capture_output=True,
        text=True,
        check=True,
    )
    head_sha = result.stdout.strip()
    registry.bind(key, Uri.parse(f"git://range/{base_sha}..{head_sha}"))


_PR_URL_RE = re.compile(r"https://github\.com/[^/]+/[^/]+/pull/(\d+)")


class GhResolver:
    """Resolves gh://pr/<n> and gh://issue/<n> via `gh` CLI."""

    def __init__(self, cwd: pathlib.Path | None = None) -> None:
        self._cwd = cwd

    def _gh(self, args: list[str]) -> str:
        result = subprocess.run(
            ["gh", *args],
            cwd=self._cwd,
            capture_output=True,
            text=True,
            check=True,
        )
        return result.stdout.strip()

    def read(self, uri: Uri) -> Any:
        path = uri.path
        if path.startswith("pr/"):
            n = path.removeprefix("pr/")
            raw = self._gh(["pr", "view", n, "--json", "url,number,headRefName"])
            data = json.loads(raw)
            return {
                "url": data["url"],
                "number": data["number"],
                "branch": data["headRefName"],
            }
        if path.startswith("issue/"):
            n = path.removeprefix("issue/")
            raw = self._gh(["issue", "view", n, "--json", "url,number"])
            data = json.loads(raw)
            return {"url": data["url"], "number": data["number"]}
        raise ValueError(f"unrecognised gh URI path: {uri}")

    def verify_produced(self, uri: Uri) -> None:
        self.read(uri)

    def capture(self, stdout: str, _stderr: str) -> Uri:
        """Parse a gh://pr/<n> URI from `gh pr create` stdout."""
        m = _PR_URL_RE.search(stdout)
        if not m:
            raise ValueError(f"no PR URL found in gh pr create output: {stdout!r}")
        n = m.group(1)
        return Uri.parse(f"gh://pr/{n}")
