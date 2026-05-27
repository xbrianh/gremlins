"""Artifact registry: maps string keys to JSON values, auto-resolving URI strings on read."""

from __future__ import annotations

import json
import os
import pathlib
import secrets
from collections.abc import Iterable, Mapping
from typing import Any

from gremlins.artifacts._protocol import SchemeResolver
from gremlins.artifacts.schemes import (
    FileSessionResolver,
    GitHubResolver,
    GitResolver,
)
from gremlins.artifacts.uri import Uri
from gremlins.utils import git as git_utils


class MissingArtifact(KeyError):
    def __init__(self, key: str) -> None:
        super().__init__(f"artifact not bound: {key!r}")
        self.key = key


class DuplicateArtifact(ValueError):
    def __init__(self, key: str, existing: Any, attempted: Any) -> None:
        super().__init__(
            f"artifact {key!r} already bound to {existing!r}; cannot rebind to {attempted!r}"
        )
        self.key = key


class ArtifactRegistry:
    def __init__(
        self,
        session_dir: pathlib.Path,
        cwd: pathlib.Path | None = None,
        resolvers: Mapping[str, SchemeResolver] | None = None,
    ) -> None:
        self._cwd = cwd
        self.registry_path = session_dir.parent / "registry.json"
        self._data: dict[str, Any] = {}
        self._resolvers: dict[str, SchemeResolver] = {
            "file": FileSessionResolver(session_dir),
            "git": GitResolver(cwd),
            "gh": GitHubResolver(cwd),
            **(resolvers or {}),
        }
        if self.registry_path.exists():
            data = json.loads(self.registry_path.read_text(encoding="utf-8"))
            self._data = dict(data)

    def _persist(self) -> None:
        path = self.registry_path
        tmp = path.with_name(path.name + f".{os.getpid()}.{secrets.token_hex(4)}.tmp")
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(self._data), encoding="utf-8")
        os.replace(tmp, path)

    def write(self, key: str, value: Any) -> None:
        """Store a JSON value. Fails at write time if value is not JSON-serializable."""
        json.dumps(value)  # validate serializability
        self._data[key] = value
        self._persist()

    def bind(self, key: str, uri: Uri, *, override: bool = False) -> None:
        value = str(uri)
        if key in self._data:
            if self._data[key] == value:
                return
            if not override:
                raise DuplicateArtifact(key, self._data[key], value)
        self._data[key] = value
        self._persist()

    def mount(self, key: str, uri: Uri) -> None:
        """Register a URI binding in-memory only; not persisted to disk."""
        self._data[key] = str(uri)

    def resolve(self, key: str) -> Uri:
        if key not in self._data:
            raise MissingArtifact(key)
        value = self._data[key]
        if not isinstance(value, str):
            raise ValueError(f"artifact {key!r} is not a URI (stored value: {value!r})")
        return Uri.parse(value)

    def _resolve_value(self, value: Any) -> Any:
        if not isinstance(value, str):
            return value
        try:
            uri = Uri.parse(value)
        except ValueError:
            return value
        if uri.scheme not in self._resolvers:
            return value
        resolved = self._resolvers[uri.scheme].read(uri)
        return self._resolve_value(resolved)

    def read(self, key: str) -> Any:
        if key not in self._data:
            raise MissingArtifact(key)
        return self._resolve_value(self._data[key])

    def produced(self, key: str) -> bool:
        return key in self._data

    def keys(self) -> Iterable[str]:
        return self._data.keys()

    def resolver(self, scheme: str) -> SchemeResolver:
        return self._resolvers[scheme]

    def unbind(self, key: str) -> None:
        if key not in self._data:
            return
        del self._data[key]
        self._persist()

    def bind_git_commit_range(self, key: str, base_sha: str) -> None:
        sha = git_utils.head_sha(cwd=self._cwd)
        if not sha:
            raise RuntimeError("could not resolve HEAD")
        self.bind(key, Uri.parse(f"git://range/{base_sha}..{sha}"))
