"""Tests for gremlins.artifacts.registry."""

from __future__ import annotations

import pathlib

import pytest

from gremlins.artifacts.registry import MissingArtifact, Registry
from gremlins.artifacts.uri import Uri


def make_registry(tmp_path: pathlib.Path) -> Registry:
    return Registry(session_dir=tmp_path)


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


def test_read_returns_file_bytes(tmp_path: pathlib.Path) -> None:
    (tmp_path / "plan.md").write_bytes(b"hello")
    r = make_registry(tmp_path)
    r.bind("plan", Uri(scheme="file", path="session/plan.md"))
    assert r.read("plan") == b"hello"
