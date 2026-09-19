"""Tests for resolve_interpolation_map — key lookup, content(), and ? defaults."""

from __future__ import annotations

import pathlib

import pytest
from _gremlins_core.artifacts import (
    ArtifactRegistry,
    MissingArtifact,
    Uri,
    resolve_interpolation_map,
)


def _make_registry(tmp_path: pathlib.Path) -> ArtifactRegistry:
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    return ArtifactRegistry(artifact_dir)


def _register_text(reg: ArtifactRegistry, uri: str, text: str) -> str:
    return reg.write_into_registry(Uri.parse(uri), text)


def test_simple_key_resolves_to_file_path(tmp_path):
    reg = _make_registry(tmp_path)
    path = _register_text(reg, "artifact://key", "hello")
    result = resolve_interpolation_map(reg, {"VAR": "artifact://key"})
    assert result == {"VAR": path}


def test_unknown_key_raises(tmp_path):
    reg = _make_registry(tmp_path)
    _register_text(reg, "artifact://pr", "opaque://pr/1")
    with pytest.raises(MissingArtifact):
        resolve_interpolation_map(reg, {"x": "artifact://pr.nonexistent"})


def test_empty_trailing_dot_key_raises(tmp_path):
    """Keys with trailing dots are literal — no such artifact exists."""
    reg = _make_registry(tmp_path)
    _register_text(reg, "artifact://pr", "opaque://pr/1")
    with pytest.raises(MissingArtifact):
        resolve_interpolation_map(reg, {"x": "artifact://pr."})


def test_private_like_key_raises_on_missing(tmp_path):
    """Double-underscore keys are literal — no such artifact exists."""
    reg = _make_registry(tmp_path)
    _register_text(reg, "artifact://pr", "opaque://pr/1")
    with pytest.raises(MissingArtifact):
        resolve_interpolation_map(reg, {"x": "artifact://pr.__class__"})


def test_file_backed_key_resolves_to_path(tmp_path):
    reg = _make_registry(tmp_path)
    path = _register_text(reg, "artifact://plan", "opaque://issue/42")
    result = resolve_interpolation_map(reg, {"ref": "artifact://plan"})
    assert result == {"ref": path}


def test_file_backed_json_value(tmp_path):
    reg = _make_registry(tmp_path)
    path = _register_text(reg, "artifact://data.json", '{"number": 42}')
    result = resolve_interpolation_map(reg, {"ref": "artifact://data.json"})
    assert result == {"ref": path}


def test_content_resolves_artifact_file(tmp_path):
    reg = _make_registry(tmp_path)
    reg.write_into_registry(Uri.parse("artifact://greeting.txt"), "hello world")
    result = resolve_interpolation_map(
        reg, {"msg": 'content("artifact://greeting.txt")'}
    )
    assert result == {"msg": "hello world"}


def test_content_with_json_path(tmp_path):
    reg = _make_registry(tmp_path)
    reg.write_into_registry(
        Uri.parse("artifact://pr.json"), '{"branch": "feat-x", "number": 7}'
    )
    result = resolve_interpolation_map(
        reg, {"branch": 'content("artifact://pr.json", "branch")'}
    )
    assert result == {"branch": "feat-x"}


def test_content_with_json_path_int(tmp_path):
    reg = _make_registry(tmp_path)
    reg.write_into_registry(Uri.parse("artifact://pr.json"), '{"number": 42}')
    result = resolve_interpolation_map(
        reg, {"num": 'content("artifact://pr.json", "number")'}
    )
    assert result == {"num": "42"}


def test_content_unknown_key_raises(tmp_path):
    reg = _make_registry(tmp_path)
    reg.write_into_registry(Uri.parse("artifact://pr.json"), '{"branch": "main"}')
    with pytest.raises((ValueError, KeyError)):
        resolve_interpolation_map(
            reg, {"x": 'content("artifact://pr.json", "nonexistent")'}
        )


def test_content_optional_returns_empty(tmp_path):
    reg = _make_registry(tmp_path)
    result = resolve_interpolation_map(
        reg,
        {"x": 'content("artifact://missing.txt")?'},
    )
    assert result == {"x": ""}
