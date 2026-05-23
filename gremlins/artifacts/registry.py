"""Artifact registry: maps string keys to URIs and resolves them via scheme resolvers."""

from __future__ import annotations

import pathlib
import subprocess
from collections.abc import Iterable
from typing import Any

from gremlins.artifacts._protocol import SchemeResolver
from gremlins.artifacts.schemes import FileSessionResolver, GitHubResolver, GitResolver
from gremlins.artifacts.uri import Uri


class MissingArtifact(KeyError):
    def __init__(self, key: str) -> None:
        super().__init__(f"artifact not bound: {key!r}")
        self.key = key


class ArtifactRegistry:
    def __init__(
        self,
        session_dir: pathlib.Path,
        cwd: pathlib.Path | None = None,
    ) -> None:
        self._cwd = cwd
        self._bindings: dict[str, Uri] = {}
        self._resolvers: dict[str, SchemeResolver] = {
            "file": FileSessionResolver(session_dir),
            "git": GitResolver(cwd),
            "gh": GitHubResolver(cwd),
        }

    def bind(self, key: str, uri: Uri) -> None:
        self._bindings[key] = uri

    def resolve(self, key: str) -> Uri:
        try:
            return self._bindings[key]
        except KeyError:
            raise MissingArtifact(key) from None

    def read(self, key: str) -> Any:
        uri = self.resolve(key)
        return self._resolvers[uri.scheme].read(uri)

    def produced(self, key: str) -> bool:
        return key in self._bindings

    def keys(self) -> Iterable[str]:
        return self._bindings.keys()

    def resolver(self, scheme: str) -> SchemeResolver:
        return self._resolvers[scheme]

    def bind_git_commit_range(self, key: str, base_sha: str) -> None:
        head_sha = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=self._cwd,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        self.bind(key, Uri.parse(f"git://range/{base_sha}..{head_sha}"))
