"""Tests for resolve_in_map ?default syntax (E2)."""

from __future__ import annotations

from types import SimpleNamespace
from unittest.mock import MagicMock

import pytest

from gremlins.artifacts.registry import MissingArtifact
from gremlins.artifacts.resolve import resolve_in_map


def _registry(bindings: dict) -> MagicMock:
    reg = MagicMock()

    def _read(key):
        if key not in bindings:
            raise MissingArtifact(key)
        return bindings[key]

    reg.read.side_effect = _read
    return reg


def test_bound_key_default_ignored():
    reg = _registry({"k": "live-value"})
    assert resolve_in_map(reg, {"v": "k?fallback"}) == {"v": "live-value"}


def test_unbound_key_empty_default():
    reg = _registry({})
    assert resolve_in_map(reg, {"v": "missing?"}) == {"v": ""}


def test_unbound_key_literal_default():
    reg = _registry({})
    assert resolve_in_map(reg, {"v": "missing?main"}) == {"v": "main"}


def test_attr_typo_raises_even_with_default():
    obj = SimpleNamespace(branch="feat")
    reg = _registry({"pr": obj})
    with pytest.raises(ValueError, match="has no attribute"):
        resolve_in_map(reg, {"v": "pr.brnch?fallback"})


def test_no_default_missing_artifact_raises():
    reg = _registry({})
    with pytest.raises(MissingArtifact):
        resolve_in_map(reg, {"v": "missing"})


def test_bound_attr_access_works():
    obj = SimpleNamespace(name="main")
    reg = _registry({"ref": obj})
    assert resolve_in_map(reg, {"v": "ref.name"}) == {"v": "main"}


def test_bound_attr_with_default_returns_attr():
    obj = SimpleNamespace(path="main")
    reg = _registry({"base_ref": obj})
    assert resolve_in_map(reg, {"v": "base_ref.path?other"}) == {"v": "main"}
