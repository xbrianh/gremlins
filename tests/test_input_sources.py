"""Tests for pipeline input sources."""

import pathlib
import textwrap

import pytest

from gremlins.pipeline import Pipeline
from gremlins.pipeline.inputs import InputSource, InputSources


class TestInputSources:
    def test_parse_sources(self) -> None:
        raw = {
            "plan": {"type": ["filepath", "string"], "optional": True},
            "instructions": {"type": "string"},
        }
        sources = InputSources.from_yaml(raw)
        plan = sources.get("plan")
        assert plan is not None
        assert plan.types == ["filepath", "string"]
        assert plan.optional is True
        instr = sources.get("instructions")
        assert instr is not None
        assert instr.optional is False

    def test_required_sources(self) -> None:
        raw = {
            "issue": {"type": "string"},
            "plan": {"type": "string", "optional": True},
        }
        sources = InputSources.from_yaml(raw)
        required = sources.required_sources()
        assert "issue" in required
        assert "plan" not in required

    def test_missing_type_field_rejected(self) -> None:
        with pytest.raises(ValueError, match="missing required 'type' field"):
            InputSources.from_yaml({"issue": {}})

    def test_invalid_type_rejected(self) -> None:
        with pytest.raises(ValueError, match="unknown type"):
            InputSources.from_yaml({"issue": {"type": "bad"}})

    def test_non_boolean_optional_rejected(self) -> None:
        with pytest.raises(ValueError, match="'optional' must be a boolean"):
            InputSources.from_yaml({"issue": {"type": "string", "optional": "yes"}})

    def test_type_list_non_string_rejected(self) -> None:
        with pytest.raises(ValueError, match="all type entries must be strings"):
            InputSources.from_yaml({"issue": {"type": ["string", 123]}})

    def test_empty_type_list_rejected(self) -> None:
        with pytest.raises(ValueError, match="type list must not be empty"):
            InputSources.from_yaml({"issue": {"type": []}})

    def test_non_mapping_source_rejected(self) -> None:
        with pytest.raises(ValueError, match="expected a mapping"):
            InputSources.from_yaml({"issue": ["string"]})

    def test_unknown_type_on_inputsource_rejected(self) -> None:
        with pytest.raises(ValueError, match="unknown type"):
            InputSource(name="bad", types=["unknown"])


class TestPipelineInputSources:
    def _write_pipeline(self, tmp_path: pathlib.Path, content: str) -> pathlib.Path:
        p = tmp_path / "pipeline.yaml"
        p.write_text(textwrap.dedent(content), encoding="utf-8")
        return p

    def test_parse_pipeline_with_input_sources(self, tmp_path: pathlib.Path) -> None:
        p = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              in:
                PLAN: plan-document?
              sources:
                plan:
                  type: [filepath, string]
                  optional: true
                instructions:
                  type: string
                  optional: true
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(p)
        assert pipeline.input_sources is not None
        plan = pipeline.input_sources.get("plan")
        assert plan is not None
        assert plan.types == ["filepath", "string"]
        assert plan.optional is True

    def test_parse_pipeline_without_input_sources(self, tmp_path: pathlib.Path) -> None:
        p = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              in:
                PLAN: plan-document?
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(p)
        assert pipeline.input_sources is None

    def test_invalid_sources_block_rejected(self, tmp_path: pathlib.Path) -> None:
        p = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              sources: ["not-a-mapping"]
            stages:
              - { name: plan, type: agent }
            """,
        )
        with pytest.raises(ValueError, match="'inputs.sources' must be a mapping"):
            Pipeline.from_yaml(p)
