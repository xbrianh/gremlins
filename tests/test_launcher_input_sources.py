"""Tests for launcher registry seeding from input sources."""

import pathlib
import tempfile
import textwrap

import pytest

from gremlins.launcher import _seed_registry_from_sources, _Inputs
from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.pipeline import Pipeline


class TestSeedRegistryFromSources:
    def _write_pipeline(
        self, tmp_path: pathlib.Path, yaml_content: str
    ) -> pathlib.Path:
        p = tmp_path / "pipeline.yaml"
        p.write_text(textwrap.dedent(yaml_content), encoding="utf-8")
        return p

    def _make_inputs(
        self,
        plan: str | None = None,
        instructions: str = "",
        project_root: str = "",
    ) -> _Inputs:
        return _Inputs(
            gremlin_id="test-id",
            kind="local",
            plan=plan,
            instructions=instructions,
            description="test",
            description_explicit=False,
            parent_id="",
            project_root=project_root or str(pathlib.Path.cwd()),
            pipeline_path="pipeline.yaml",
            pipeline_args=[],
            client_label="claude:sonnet",
            fetch_worktree=False,
            base_ref_name="",
            base_ref_sha="",
            stage_inputs={},
        )

    def test_legacy_fallback_when_no_input_sources(
        self, tmp_path: pathlib.Path
    ) -> None:
        """When pipeline has no input_sources, fall back to hardcoded plan_arg."""
        artifact_dir = tmp_path / "artifacts"
        artifact_dir.mkdir()
        registry = ArtifactRegistry(artifact_dir=artifact_dir)

        pipeline_path = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(pipeline_path)
        inputs = self._make_inputs(plan="test plan")

        _seed_registry_from_sources(registry, pipeline, inputs, artifact_dir)

        # Should have registered plan_arg in legacy mode
        assert "plan_arg" in registry.data
        assert registry.data["plan_arg"] == "file://session/plan-arg.txt"

    def test_legacy_fallback_when_loaded_pipeline_none(
        self, tmp_path: pathlib.Path
    ) -> None:
        """When loaded_pipeline is None, use legacy hardcoded seeding."""
        artifact_dir = tmp_path / "artifacts"
        artifact_dir.mkdir()
        registry = ArtifactRegistry(artifact_dir=artifact_dir)

        inputs = self._make_inputs(plan="test plan")

        _seed_registry_from_sources(registry, None, inputs, artifact_dir)

        # Should have registered plan_arg in legacy mode
        assert "plan_arg" in registry.data
        assert registry.data["plan_arg"] == "file://session/plan-arg.txt"

    def test_string_source_as_issue(self, tmp_path: pathlib.Path) -> None:
        """String source 'issue' registers issue reference."""
        artifact_dir = tmp_path / "artifacts"
        artifact_dir.mkdir()
        registry = ArtifactRegistry(artifact_dir=artifact_dir)

        pipeline_path = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              sources:
                issue:
                  type: string
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(pipeline_path)
        inputs = self._make_inputs(plan="#123")

        _seed_registry_from_sources(registry, pipeline, inputs, artifact_dir)

        # Issue should be registered as a string
        assert "issue" in registry.data
        assert registry.data["issue"] == "file://session/issue.txt"
        issue_file = artifact_dir / "issue.txt"
        assert issue_file.read_text(encoding="utf-8") == "#123"

    def test_string_source_as_plan(self, tmp_path: pathlib.Path) -> None:
        """String source 'plan' registers plan as a string."""
        artifact_dir = tmp_path / "artifacts"
        artifact_dir.mkdir()
        registry = ArtifactRegistry(artifact_dir=artifact_dir)

        pipeline_path = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              sources:
                plan:
                  type: string
                  optional: true
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(pipeline_path)
        inputs = self._make_inputs(plan="some plan text")

        _seed_registry_from_sources(registry, pipeline, inputs, artifact_dir)

        # Plan should be registered as a string
        assert "plan" in registry.data
        assert registry.data["plan"] == "file://session/plan.txt"
        plan_file = artifact_dir / "plan.txt"
        assert plan_file.read_text(encoding="utf-8") == "some plan text"

    def test_string_source_as_instructions(self, tmp_path: pathlib.Path) -> None:
        """String source 'instructions' registers instructions."""
        artifact_dir = tmp_path / "artifacts"
        artifact_dir.mkdir()
        registry = ArtifactRegistry(artifact_dir=artifact_dir)

        pipeline_path = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              sources:
                instructions:
                  type: string
                  optional: true
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(pipeline_path)
        inputs = self._make_inputs(instructions="some instructions")

        _seed_registry_from_sources(registry, pipeline, inputs, artifact_dir)

        # Instructions should be registered
        assert "instructions" in registry.data
        assert registry.data["instructions"] == "file://session/instructions.txt"
        instr_file = artifact_dir / "instructions.txt"
        assert instr_file.read_text(encoding="utf-8") == "some instructions"

    def test_optional_source_missing(self, tmp_path: pathlib.Path) -> None:
        """Optional source absent → key not in registry."""
        artifact_dir = tmp_path / "artifacts"
        artifact_dir.mkdir()
        registry = ArtifactRegistry(artifact_dir=artifact_dir)

        pipeline_path = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              sources:
                plan:
                  type: string
                  optional: true
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(pipeline_path)
        # No plan provided
        inputs = self._make_inputs()

        _seed_registry_from_sources(registry, pipeline, inputs, artifact_dir)

        # Plan should not be in registry since it's optional and not provided
        assert "plan" not in registry.data

    def test_required_source_missing_raises(self, tmp_path: pathlib.Path) -> None:
        """Required source missing → error at launch time."""
        artifact_dir = tmp_path / "artifacts"
        artifact_dir.mkdir()
        registry = ArtifactRegistry(artifact_dir=artifact_dir)

        pipeline_path = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              sources:
                issue:
                  type: string
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(pipeline_path)
        # No plan provided (issue requires it)
        inputs = self._make_inputs()

        with pytest.raises(ValueError, match="required input source"):
            _seed_registry_from_sources(registry, pipeline, inputs, artifact_dir)

    def test_union_type_filepath_first(self, tmp_path: pathlib.Path) -> None:
        """Union source given as resolvable file path → resolves as filepath."""
        artifact_dir = tmp_path / "artifacts"
        artifact_dir.mkdir()
        registry = ArtifactRegistry(artifact_dir=artifact_dir)

        # Create a test file
        plan_file = tmp_path / "plan.md"
        plan_file.write_text("# My Plan\nSome content", encoding="utf-8")

        pipeline_path = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              sources:
                plan:
                  type: [filepath, string]
                  optional: true
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(pipeline_path)
        inputs = self._make_inputs(plan=str(plan_file))

        _seed_registry_from_sources(registry, pipeline, inputs, artifact_dir)

        # Plan should be registered as a filepath URI
        assert "plan" in registry.data
        assert registry.data["plan"].startswith("file://")
        assert str(plan_file) in registry.data["plan"]

    def test_union_type_falls_back_to_string(self, tmp_path: pathlib.Path) -> None:
        """Union source given as plain string → registered as string."""
        artifact_dir = tmp_path / "artifacts"
        artifact_dir.mkdir()
        registry = ArtifactRegistry(artifact_dir=artifact_dir)

        pipeline_path = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              sources:
                plan:
                  type: [filepath, string]
                  optional: true
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(pipeline_path)
        # Provide a string that's not a file path (issue ref)
        inputs = self._make_inputs(plan="#123")

        _seed_registry_from_sources(registry, pipeline, inputs, artifact_dir)

        # Plan should be registered as a string (since file doesn't exist)
        assert "plan" in registry.data
        assert registry.data["plan"] == "file://session/plan.txt"
        plan_file = artifact_dir / "plan.txt"
        assert plan_file.read_text(encoding="utf-8") == "#123"

    def test_filepath_only_source_missing_file_raises(
        self, tmp_path: pathlib.Path
    ) -> None:
        """Filepath-only source with unresolvable path → error."""
        artifact_dir = tmp_path / "artifacts"
        artifact_dir.mkdir()
        registry = ArtifactRegistry(artifact_dir=artifact_dir)

        pipeline_path = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              sources:
                plan:
                  type: filepath
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(pipeline_path)
        # Provide a path that doesn't exist
        inputs = self._make_inputs(plan="/nonexistent/path.md")

        with pytest.raises(ValueError, match="required input source"):
            _seed_registry_from_sources(registry, pipeline, inputs, artifact_dir)

    def test_multiple_sources_partially_satisfied(
        self, tmp_path: pathlib.Path
    ) -> None:
        """Multiple sources with mixed required/optional - only instructions absent."""
        artifact_dir = tmp_path / "artifacts"
        artifact_dir.mkdir()
        registry = ArtifactRegistry(artifact_dir=artifact_dir)

        pipeline_path = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              sources:
                issue:
                  type: string
                plan:
                  type: string
                  optional: true
                instructions:
                  type: string
                  optional: true
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(pipeline_path)
        # Provide plan (which satisfies both issue and plan)
        inputs = self._make_inputs(plan="#456")

        _seed_registry_from_sources(registry, pipeline, inputs, artifact_dir)

        # Issue and plan should both be registered since plan satisfies both
        # Instructions should not be registered (optional and not provided)
        assert "issue" in registry.data
        assert "plan" in registry.data
        assert "instructions" not in registry.data
