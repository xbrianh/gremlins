"""Artifact registry: maps string keys to URIs and resolves them via scheme resolvers."""

from __future__ import annotations

import json
import os
import pathlib
import secrets
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


class DuplicateArtifact(ValueError):
    def __init__(self, key: str, existing: Uri, attempted: Uri) -> None:
        super().__init__(
            f"artifact {key!r} already bound to {existing}; cannot rebind to {attempted}"
        )
        self.key = key


class ArtifactRegistry:
    def __init__(
        self,
        session_dir: pathlib.Path,
        cwd: pathlib.Path | None = None,
        *,
        persist_path: pathlib.Path | None = None,
    ) -> None:
        self._cwd = cwd
        self.registry_path = (
            persist_path
            if persist_path is not None
            else session_dir.parent / "registry.json"
        )
        self._bindings: dict[str, Uri] = {}
        self._resolvers: dict[str, SchemeResolver] = {
            "file": FileSessionResolver(session_dir),
            "git": GitResolver(cwd),
            "gh": GitHubResolver(cwd),
        }
        if self.registry_path.exists():
            data = json.loads(self.registry_path.read_text(encoding="utf-8"))
            for k, v in data.items():
                self._bindings[k] = Uri.parse(v)

    def bind(self, key: str, uri: Uri, *, override: bool = False) -> None:
        if key in self._bindings and not override:
            raise DuplicateArtifact(key, self._bindings[key], uri)
        self._bindings[key] = uri
        path = self.registry_path
        data = {k: str(v) for k, v in self._bindings.items()}
        tmp = path.with_name(path.name + f".{os.getpid()}.{secrets.token_hex(4)}.tmp")
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(data), encoding="utf-8")
        os.replace(tmp, path)

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
