"""Tests for resolve_interpolation_map ?default syntax (E2)."""

from __future__ import annotations

import json
import pathlib

import pytest
from _gremlins_core.artifacts import (
    ArtifactRegistry,
    MissingArtifact,
    resolve_interpolation_map,
)


def _registry(bindings: dict, tmp_path: pathlib.Path) -> ArtifactRegistry:
    (tmp_path / "registry.json").write_text(json.dumps(bindings))
    return ArtifactRegistry(artifact_dir=tmp_path / "artifacts")


def test_bound_key_default_ignored(tmp_path: pathlib.Path):
    reg = _registry({"k": "live-value"}, tmp_path)
    assert resolve_interpolation_map(reg, {"v": "k?fallback"}) == {"v": "live-value"}


def test_unbound_key_empty_default(tmp_path: pathlib.Path):
    reg = _registry({}, tmp_path)
    assert resolve_interpolation_map(reg, {"v": "missing?"}) == {"v": ""}


def test_unbound_key_literal_default(tmp_path: pathlib.Path):
    reg = _registry({}, tmp_path)
    assert resolve_interpolation_map(reg, {"v": "missing?main"}) == {"v": "main"}


def test_unbound_key_fallback_with_literal_dot_in_name(tmp_path: pathlib.Path):
    """Dot-prefixed path syntax is no longer special — dots are literal key chars."""
    reg = _registry({"pr.brnch": "feat"}, tmp_path)
    assert resolve_interpolation_map(reg, {"v": "pr.brnch?fallback"}) == {"v": "feat"}


def test_no_default_missing_artifact_raises(tmp_path: pathlib.Path):
    reg = _registry({}, tmp_path)
    with pytest.raises(MissingArtifact):
        resolve_interpolation_map(reg, {"v": "missing"})


def test_missing_artifact_raises_on_literal_dot_key(tmp_path: pathlib.Path):
    """Dots in keys are literal — 'ref.name' is a single key, not a traversal."""
    reg = _registry({"ref": "some_value"}, tmp_path)
    with pytest.raises(MissingArtifact):
        resolve_interpolation_map(reg, {"v": "ref.name"})
