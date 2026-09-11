"""Tests for gremlins.artifacts.registry."""

from __future__ import annotations

import json
import pathlib

import pytest
from _gremlins_core.artifacts import (
    ArtifactRegistry,
    DuplicateArtifact,
    MissingArtifact,
    Uri,
)


def make_registry(tmp_path: pathlib.Path) -> ArtifactRegistry:
    return ArtifactRegistry(artifact_dir=tmp_path / "artifacts")


def test_data_uri_unbound_raises_missing_artifact(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    with pytest.raises(MissingArtifact) as exc_info:
        r.data_uri("nope")
    assert "nope" in str(exc_info.value)


def test_missing_artifact_is_key_error(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    with pytest.raises(KeyError):
        r.data_uri("missing")


def test_registered_true_after_write(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    uri = Uri.parse("artifact://x.md")
    assert not r.is_registered(str(uri))
    r.write_into_registry(uri, "content")
    assert r.is_registered(str(uri))


def test_path_for_uri_does_not_register(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    uri = Uri.parse("artifact://later.md")
    path = r.path_for_uri(uri)
    assert path.endswith("later.md")
    assert not r.is_registered(str(uri))


def test_keys_returns_registered_keys(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    r.write_into_registry(Uri.parse("artifact://a.md"), "")
    r.write_into_registry(Uri.parse("artifact://b.md"), "")
    assert set(r.keys()) == {"artifact://a.md", "artifact://b.md"}


def test_commit_idempotent_same_path(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    uri = Uri.parse("artifact://x.md")
    path = r.path_for_uri(uri)
    r.commit(str(uri), path)
    r.commit(str(uri), path)
    assert r.data_uri(str(uri)) == path


def test_commit_conflicting_path_raises(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    uri = Uri.parse("artifact://x.md")
    r.commit(str(uri), "/tmp/one")
    with pytest.raises(DuplicateArtifact) as exc_info:
        r.commit(str(uri), "/tmp/two")
    assert str(uri) in str(exc_info.value)


def test_write_into_registry_writes_file(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    uri = Uri.parse("artifact://x.md")
    path = r.write_into_registry(uri, "hello")
    assert pathlib.Path(path).read_text() == "hello"


def test_copy_into_registry_copies_file(tmp_path: pathlib.Path) -> None:
    src = tmp_path / "src.txt"
    src.write_text("copied")
    r = make_registry(tmp_path)
    uri = Uri.parse("artifact://dst.txt")
    path = r.copy_into_registry(uri, src)
    assert pathlib.Path(path).read_text() == "copied"
    assert r.is_registered(str(uri))


def test_registry_path_derives_from_artifact_dir(tmp_path: pathlib.Path) -> None:
    r = ArtifactRegistry(artifact_dir=tmp_path / "artifacts")
    assert r.registry_path == tmp_path / "registry.json"


def test_write_into_registry_persists_to_file(tmp_path: pathlib.Path) -> None:
    r = ArtifactRegistry(artifact_dir=tmp_path / "artifacts")
    r.write_into_registry(Uri.parse("artifact://plan.md"), "")
    data = json.loads(r.registry_path.read_text())
    assert "artifact://plan.md" in data
    assert data["artifact://plan.md"].endswith("plan.md")


def test_init_loads_from_persist_file(tmp_path: pathlib.Path) -> None:
    (tmp_path / "registry.json").write_text(
        json.dumps({"artifact://plan.md": "/tmp/some/plan.md"})
    )
    r = ArtifactRegistry(artifact_dir=tmp_path / "artifacts")
    assert r.data_uri("artifact://plan.md") == "/tmp/some/plan.md"


def test_persist_survives_roundtrip(tmp_path: pathlib.Path) -> None:
    artifact_dir = tmp_path / "artifacts"
    r1 = ArtifactRegistry(artifact_dir=artifact_dir)
    r1.write_into_registry(Uri.parse("artifact://pr.md"), "")
    r2 = ArtifactRegistry(artifact_dir=artifact_dir)
    assert r2.is_registered("artifact://pr.md")
    stored = r2.data_uri("artifact://pr.md")
    assert isinstance(stored, str)
    assert "pr.md" in stored
