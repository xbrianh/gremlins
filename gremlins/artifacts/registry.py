"""Artifact registry: maps URI strings to slugged filesystem paths."""

from __future__ import annotations

import json
import logging
import os
import pathlib
import secrets
import shutil
from collections.abc import Iterable
from typing import Any

from _gremlins_core.artifacts import Uri

logger = logging.getLogger(__name__)


class MissingArtifact(KeyError):
    def __init__(self, key: str) -> None:
        super().__init__(f"artifact not bound: {key!r}")
        self.key = key


class DuplicateArtifact(KeyError):
    def __init__(self, key: str, existing: str, incoming: str) -> None:
        super().__init__(
            f"duplicate artifact: {key!r} already bound to {existing!r}, "
            f"cannot rebind to {incoming!r}"
        )
        self.key = key


class ArtifactRegistry:
    def __init__(
        self,
        artifact_dir: pathlib.Path,
    ) -> None:
        self.artifact_dir = artifact_dir
        self.registry_path = artifact_dir.parent / "registry.json"
        self.data: dict[str, Any] = {}
        if self.registry_path.exists():
            data = json.loads(self.registry_path.read_text(encoding="utf-8"))
            self.data = dict(data)

    def _persist(self) -> None:
        path = self.registry_path
        tmp = path.with_name(path.name + f".{os.getpid()}.{secrets.token_hex(4)}.tmp")
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(json.dumps(self.data), encoding="utf-8")
        os.replace(tmp, path)

    def data_uri(self, key: str) -> Any:
        """Return the local filesystem path for *key*.

        Raises MissingArtifact if *key* is not bound.
        """
        if key not in self.data:
            raise MissingArtifact(key)
        return self.data[key]

    def register(self, uri: Uri, *, overwrite: bool = True) -> str:
        """Register a URI artifact, returning the canonical filesystem path.

        Always resolves the URI to its canonical on-disk path (which is what
        exec and agent stages write to), even if the key is already bound in
        the registry.  This keeps the returned path consistent with the file
        the stage is expected to produce.

        If *overwrite* is True (the default), silently overwrites an existing
        entry.  Set *overwrite* to False to raise DuplicateArtifact when
        the key already exists (for stages that must be strict about uniqueness).
        """
        key = str(uri)
        if uri.scheme != "artifact":
            logger.warning(
                "register(%r): unrecognized scheme %r — typo? (expected 'artifact')",
                key,
                uri.scheme,
            )
        if key in self.data and not overwrite:
            raise DuplicateArtifact(key, str(self.data[key]), str(uri))
        # Resolve artifact:// URI to canonical filesystem path
        name = uri.path.lstrip("/")
        if name.startswith("session/"):
            name = name[len("session/") :]
        path = (self.artifact_dir / name).resolve()
        base = self.artifact_dir.resolve()
        try:
            path.relative_to(base)
        except ValueError:
            raise ValueError(f"path escapes artifact directory: {uri}") from None
        path.parent.mkdir(parents=True, exist_ok=True)
        self.data[key] = str(path)
        self._persist()
        return self.data[key]

    def content(self, uri_str: str, json_path: str | None = None) -> str:
        """Read file content, optionally traversing a JSON path."""
        raw = self.data_uri(uri_str)
        if not isinstance(raw, str):
            raise ValueError(
                f"content({uri_str!r}): expected a file path, got {type(raw).__name__}"
            )
        # Resolve URI values to filesystem paths
        if raw.startswith("file://session/"):
            name = raw[len("file://session/") :]
            p = self.artifact_dir / name
            if not p.exists():
                raise MissingArtifact(uri_str)
            text = p.read_text(encoding="utf-8")
        elif raw.startswith("file://"):
            p = pathlib.Path(raw[len("file://") :])
            if not p.exists():
                raise MissingArtifact(uri_str)
            text = p.read_text(encoding="utf-8")
        elif raw.startswith("/"):
            p = pathlib.Path(raw)
            if not p.exists():
                raise MissingArtifact(uri_str)
            text = p.read_text(encoding="utf-8")
        else:
            # Logical values (e.g. git:// URIs, raw strings) — return as-is
            logger.warning(
                "content(%r): returning raw registry value as-is (not a file path)",
                uri_str,
            )
            return raw
        if json_path is None:
            return text
        data = json.loads(text)
        for segment in json_path.split("."):
            if isinstance(data, dict):
                data = data[segment]
            else:
                raise ValueError(
                    f"content({uri_str}, {json_path!r}): cannot traverse into {type(data)}"
                )
        return str(data) if not isinstance(data, str) else data

    def exists(self, uri: str | Uri) -> bool:
        """Check whether a registered artifact exists on disk.

        Returns True if the key is registered and the backing file
        exists with non-zero size.  For non-file artifacts (e.g.
        ``git://range/...``, ``opaque://...``) returns True once
        registered.
        """
        match uri:
            case str():
                key = uri
            case Uri():
                key = str(uri)
            case _:
                raise ValueError(f"expected str or Uri, got {type(uri).__name__}")
        if key not in self.data:
            return False
        value = self.data[key]
        if not isinstance(value, str):
            return True
        # Resolve file:// URIs to filesystem paths
        if value.startswith("file://session/"):
            name = value[len("file://session/") :]
            p = self.artifact_dir / name
        elif value.startswith("file://"):
            p = pathlib.Path(value[len("file://") :])
        else:
            p = pathlib.Path(value)
        if p.is_absolute():
            return p.exists() and p.stat().st_size > 0
        # Non-file values (e.g. git://range, opaque://, raw strings) are
        # registered but have no on-disk file to check — they always exist.
        return True

    def is_registered(self, key: str) -> bool:
        """Return True if *key* is bound in the registry."""
        return key in self.data

    def keys(self) -> Iterable[str]:
        return self.data.keys()

    def merge_from(
        self,
        other: ArtifactRegistry,
        *,
        key_map: dict[str, str] | None = None,
        copy_files: bool = False,
        keys: set[str] | None = None,
    ) -> None:
        """Copy file artifacts from *other* into self.

        *key_map* maps child keys to parent keys.  When None, child keys are
        used as-is (identity mapping).  Keys already present in self are skipped.
        """
        for key in keys if keys is not None else other.keys():
            uri_str = other.data.get(key)
            if not isinstance(uri_str, str):
                continue
            parent_key = key_map[key] if key_map else key
            if parent_key in self.data:
                continue
            # copy_files=True: copy actual file artifacts into self.artifact_dir.
            # register() stores absolute filesystem paths; older code stores
            # file:// URIs.  Both represent files that must survive child-dir
            # cleanup.
            is_file_artifact = uri_str.startswith("file://") or os.path.isabs(uri_str)
            if copy_files and is_file_artifact:
                # Resolve the URI to a filesystem path
                if uri_str.startswith("file://session/"):
                    name = uri_str[len("file://session/") :]
                    src_path = other.artifact_dir / name
                elif uri_str.startswith("file://"):
                    src_path = pathlib.Path(uri_str[len("file://") :])
                else:
                    src_path = pathlib.Path(uri_str)
                if not src_path.exists():
                    logger.warning("child artifact missing: %s", src_path)
                    continue
                # Use parent_key to derive a unique filename (key_map means
                # multi-child disambiguation, so parent_key includes child name).
                # Naming contract: parent_keys that differ only in separator
                # position (e.g. "review_code/one" vs "review_code_two") may
                # collide after transformation.  Pipeline authors should ensure
                # sibling stage names are sufficiently distinct.
                if key_map:
                    unique_name = parent_key.replace("/", "_") + src_path.suffix
                else:
                    unique_name = src_path.name
                dest_path = self.artifact_dir / unique_name
                dest_path.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(src_path, dest_path)
                self.data[parent_key] = str(dest_path)
                self._persist()
            else:
                self.data[parent_key] = uri_str
                self._persist()

    @classmethod
    def from_registry_file(
        cls,
        path: pathlib.Path,
        *,
        artifact_dir: pathlib.Path,
    ) -> ArtifactRegistry:
        """Load a registry from a registry.json at *path*."""
        registry = cls(artifact_dir=artifact_dir)
        if path != registry.registry_path:
            if path.exists():
                registry.data = dict(json.loads(path.read_text(encoding="utf-8")))
                registry._persist()
        return registry
