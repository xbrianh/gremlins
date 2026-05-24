"""Artifact registry: maps string keys to URIs and resolves them via scheme resolvers."""

from __future__ import annotations

import pathlib
from collections.abc import Iterable
from typing import Any

from gremlins.artifacts._protocol import SchemeResolver
from gremlins.artifacts.schemes import FileSessionResolver, GitHubResolver, GitResolver
from gremlins.artifacts.uri import Uri
from gremlins.utils import git as git_utils


class MissingArtifact(KeyError):
    def __init__(self, key: str) -> None:
        super().__init__(f"artifact not bound: {key!r}")
        self.key = key


class DuplicateArtifact(Exception):
    def __init__(self, key: str, existing: Uri, attempted: Uri) -> None:
        super().__init__(
            f"artifact {key!r} already bound to {existing!r}; cannot rebind to {attempted!r}"
        )
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
        if key in self._bindings:
            raise DuplicateArtifact(key, self._bindings[key], uri)
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
        sha = git_utils.head_sha(cwd=self._cwd)
        if not sha:
            raise RuntimeError("could not resolve HEAD")
        self.bind(key, Uri.parse(f"git://range/{base_sha}..{sha}"))
