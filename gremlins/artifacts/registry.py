"""Artifact registry: maps string keys to URIs and resolves them via scheme resolvers."""

from __future__ import annotations

import pathlib
from collections.abc import Iterable
from typing import Any

from gremlins.artifacts._protocol import SchemeResolver
from gremlins.artifacts.uri import Uri, extra_scheme_names

_extra_resolvers: dict[str, SchemeResolver] = {}


class MissingArtifact(KeyError):
    def __init__(self, key: str) -> None:
        super().__init__(f"artifact not bound: {key!r}")
        self.key = key


def register_scheme(scheme: str, resolver: SchemeResolver) -> None:
    """Register a custom scheme resolver. The scheme becomes valid in Uri.parse()."""
    extra_scheme_names.add(scheme)
    _extra_resolvers[scheme] = resolver


class Registry:
    def __init__(
        self,
        session_dir: pathlib.Path,
        cwd: pathlib.Path | None = None,
    ) -> None:
        from gremlins.artifacts.schemes import (  # noqa: PLC0415
            FileSessionResolver,
            GhResolver,
            GitResolver,
        )

        self._bindings: dict[str, Uri] = {}
        self._resolvers: dict[str, SchemeResolver] = {
            "file": FileSessionResolver(session_dir),
            "git": GitResolver(cwd),
            "gh": GhResolver(cwd),
            **_extra_resolvers,
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
