"""Tests for launcher registry seeding from input sources."""

import pathlib

import pytest

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.launcher import seed_registry_from_sources
from gremlins.pipeline.inputs import InputSource


def _make_registry(tmp_path: pathlib.Path) -> tuple[ArtifactRegistry, pathlib.Path]:
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir()
    return ArtifactRegistry(artifact_dir=artifact_dir), artifact_dir


def _sources(*items: tuple[str, list[str], bool]) -> dict[str, InputSource]:
    return {
        name: InputSource(name=name, types=types, optional=optional)
        for name, types, optional in items
    }


class TestSeedRegistryFromSources:
    def test_string_source_writes_file_and_binds(self, tmp_path: pathlib.Path) -> None:
        registry, artifact_dir = _make_registry(tmp_path)
        sources = _sources(("instructions", ["string"], False))

        seed_registry_from_sources(
            registry, {"instructions": "do the thing"}, sources, artifact_dir
        )

        assert registry.data["instructions"] == "file://session/instructions.txt"
        assert (artifact_dir / "instructions.txt").read_text() == "do the thing"

    def test_filepath_source_copies_to_session(self, tmp_path: pathlib.Path) -> None:
        registry, artifact_dir = _make_registry(tmp_path)
        plan_file = tmp_path / "plan.md"
        plan_file.write_text("# Plan", encoding="utf-8")
        sources = _sources(("plan", ["filepath"], False))

        seed_registry_from_sources(
            registry, {"plan": str(plan_file)}, sources, artifact_dir
        )

        assert registry.data["plan"] == "file://session/plan.md"
        assert (artifact_dir / "plan.md").read_text() == "# Plan"

    def test_union_type_falls_back_to_string(self, tmp_path: pathlib.Path) -> None:
        registry, artifact_dir = _make_registry(tmp_path)
        sources = _sources(("plan", ["filepath", "string"], True))

        seed_registry_from_sources(registry, {"plan": "#123"}, sources, artifact_dir)

        assert registry.data["plan"] == "file://session/plan.txt"
        assert (artifact_dir / "plan.txt").read_text() == "#123"

    def test_optional_source_absent_skipped(self, tmp_path: pathlib.Path) -> None:
        registry, artifact_dir = _make_registry(tmp_path)
        sources = _sources(("instructions", ["string"], True))

        seed_registry_from_sources(registry, {}, sources, artifact_dir)

        assert "instructions" not in registry.data

    def test_required_source_absent_raises(self, tmp_path: pathlib.Path) -> None:
        registry, artifact_dir = _make_registry(tmp_path)
        sources = _sources(("plan", ["string"], False))

        with pytest.raises(ValueError, match="required input source"):
            seed_registry_from_sources(registry, {}, sources, artifact_dir)

    def test_filepath_only_no_file_raises(self, tmp_path: pathlib.Path) -> None:
        registry, artifact_dir = _make_registry(tmp_path)
        sources = _sources(("plan", ["filepath"], False))

        with pytest.raises(ValueError, match="required input source"):
            seed_registry_from_sources(
                registry, {"plan": "/nonexistent/plan.md"}, sources, artifact_dir
            )

    def test_unknown_key_in_input_values_ignored(self, tmp_path: pathlib.Path) -> None:
        registry, artifact_dir = _make_registry(tmp_path)
        sources = _sources(("plan", ["string"], True))

        seed_registry_from_sources(
            registry, {"plan": "ref", "extra": "ignored"}, sources, artifact_dir
        )

        assert "plan" in registry.data
        assert "extra" not in registry.data
