"""Tests for gremlins.artifacts.registry."""

from __future__ import annotations

import json
import pathlib

import pytest

from gremlins.artifacts.registry import (
    ArtifactRegistry,
    DuplicateArtifact,
    MissingArtifact,
)
from gremlins.artifacts.uri import Uri


def make_registry(tmp_path: pathlib.Path) -> ArtifactRegistry:
    return ArtifactRegistry(session_dir=tmp_path / "artifacts")


def test_bind_resolve_roundtrip(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    uri = Uri(scheme="file", path="session/plan.md")
    r.bind("plan", uri)
    assert r.resolve("plan") == uri


def test_resolve_unbound_raises_missing_artifact(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    with pytest.raises(MissingArtifact) as exc_info:
        r.resolve("nope")
    assert exc_info.value.key == "nope"


def test_missing_artifact_is_key_error(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    with pytest.raises(KeyError):
        r.resolve("missing")


def test_produced_true_after_bind(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    assert not r.produced("x")
    r.bind("x", Uri(scheme="file", path="session/x.md"))
    assert r.produced("x")


def test_keys_returns_bound_keys(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    r.bind("a", Uri(scheme="file", path="session/a.md"))
    r.bind("b", Uri(scheme="file", path="session/b.md"))
    assert set(r.keys()) == {"a", "b"}


def test_bind_duplicate_raises(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    first = Uri(scheme="file", path="session/a.md")
    second = Uri(scheme="file", path="session/b.md")
    r.bind("x", first)
    with pytest.raises(DuplicateArtifact) as exc_info:
        r.bind("x", second)
    assert exc_info.value.key == "x"
    assert "x" in str(exc_info.value)
    assert str(first) in str(exc_info.value)
    assert str(second) in str(exc_info.value)


def test_read_returns_file_content(tmp_path: pathlib.Path) -> None:
    session_dir = tmp_path / "artifacts"
    session_dir.mkdir()
    (session_dir / "plan.md").write_text("hello", encoding="utf-8")
    r = ArtifactRegistry(session_dir=session_dir)
    r.bind("plan", Uri(scheme="file", path="session/plan.md"))
    assert r.read("plan") == "hello"


def test_registry_path_derives_from_session_dir(tmp_path: pathlib.Path) -> None:
    r = ArtifactRegistry(session_dir=tmp_path / "artifacts")
    assert r.registry_path == tmp_path / "registry.json"


def test_bind_persists_to_file(tmp_path: pathlib.Path) -> None:
    r = ArtifactRegistry(session_dir=tmp_path / "artifacts")
    r.bind("plan", Uri.parse("file://session/plan.md"))
    data = json.loads(r.registry_path.read_text())
    assert data["plan"] == "file://session/plan.md"


def test_init_loads_from_persist_file(tmp_path: pathlib.Path) -> None:
    (tmp_path / "registry.json").write_text(
        json.dumps({"plan": "file://session/plan.md"})
    )
    r = ArtifactRegistry(session_dir=tmp_path / "artifacts")
    assert r.resolve("plan") == Uri.parse("file://session/plan.md")


def test_persist_survives_roundtrip(tmp_path: pathlib.Path) -> None:
    session_dir = tmp_path / "artifacts"
    r1 = ArtifactRegistry(session_dir=session_dir)
    r1.bind("pr", Uri.parse("gh://pr/42"))
    r2 = ArtifactRegistry(session_dir=session_dir)
    assert r2.resolve("pr") == Uri.parse("gh://pr/42")


def test_unbind_removes_binding(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    r.bind("x", Uri.parse("file://session/x.md"))
    assert r.produced("x")
    r.unbind("x")
    assert not r.produced("x")


def test_unbind_persists_removal(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    r.bind("x", Uri.parse("file://session/x.md"))
    r.unbind("x")
    data = json.loads(r.registry_path.read_text())
    assert "x" not in data


def test_unbind_missing_key_is_noop(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    r.unbind("does-not-exist")  # must not raise


def test_bind_still_raises_duplicate_after_unbind_rebind(
    tmp_path: pathlib.Path,
) -> None:
    r = make_registry(tmp_path)
    first = Uri.parse("file://session/a.md")
    second = Uri.parse("file://session/b.md")
    r.bind("x", first)
    r.unbind("x")
    r.bind("x", first)  # clean re-bind after unbind
    with pytest.raises(DuplicateArtifact):
        r.bind("x", second)  # bind() is still strict


def test_write_stores_plain_value(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    r.write("status", "needs_fix")
    assert r.read("status") == "needs_fix"


def test_write_persists_to_file(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    r.write("meta", {"count": 3, "flag": True})
    data = json.loads(r.registry_path.read_text())
    assert data["meta"] == {"count": 3, "flag": True}


def test_write_fails_on_non_serializable(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    with pytest.raises(TypeError):
        r.write("bad", object())


def test_read_returns_dict_as_is(tmp_path: pathlib.Path) -> None:
    r = make_registry(tmp_path)
    r.write("meta", {"key": "value", "num": 42})
    assert r.read("meta") == {"key": "value", "num": 42}
