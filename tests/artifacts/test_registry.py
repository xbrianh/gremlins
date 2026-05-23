"""Tests for gremlins.artifacts.registry."""
from __future__ import annotations

import pathlib
from unittest.mock import MagicMock

import pytest

import gremlins.artifacts.registry as registry_mod
import gremlins.artifacts.uri as uri_mod
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


def test_read_delegates_to_mock_resolver(
    tmp_path: pathlib.Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    mock_resolver = MagicMock()
    mock_resolver.read.return_value = b"hello"

    monkeypatch.setitem(registry_mod._extra_resolvers, "mock", mock_resolver)
    monkeypatch.setattr(uri_mod, "extra_scheme_names", {"mock"})

    r = make_registry(tmp_path)
    uri = Uri(scheme="mock", path="some/path")
    r.bind("thing", uri)
    result = r.read("thing")
    assert result == b"hello"
    mock_resolver.read.assert_called_once_with(uri)
