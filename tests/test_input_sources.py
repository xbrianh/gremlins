"""Tests for pipeline input sources."""

import pathlib
import textwrap

import pytest

from gremlins.pipeline import Pipeline
from gremlins.pipeline.inputs import InputSource, InputSources


class TestInputSource:
    def test_single_type(self) -> None:
        source = InputSource(name="issue", types=["string"])
        assert source.name == "issue"
        assert source.types == ["string"]
        assert source.optional is False

    def test_union_type(self) -> None:
        source = InputSource(name="plan", types=["filepath", "string"])
        assert source.types == ["filepath", "string"]

    def test_optional(self) -> None:
        source = InputSource(name="plan", types=["string"], optional=True)
        assert source.optional is True

    def test_unknown_type_rejected(self) -> None:
        with pytest.raises(ValueError, match="unknown type"):
            InputSource(name="bad", types=["unknown"])

    def test_empty_types_rejected(self) -> None:
        with pytest.raises(ValueError, match="types list must not be empty"):
            InputSource(name="bad", types=[])


class TestInputSources:
    def test_parse_simple_string_source(self) -> None:
        raw = {
            "issue": {
                "type": "string",
            }
        }
        sources = InputSources.from_yaml(raw)
        issue = sources.get("issue")
        assert issue is not None
        assert issue.name == "issue"
        assert issue.types == ["string"]
        assert issue.optional is False

    def test_parse_union_type_source(self) -> None:
        raw = {
            "plan": {
                "type": ["filepath", "string"],
            }
        }
        sources = InputSources.from_yaml(raw)
        plan = sources.get("plan")
        assert plan is not None
        assert plan.types == ["filepath", "string"]

    def test_parse_optional_source(self) -> None:
        raw = {
            "instructions": {
                "type": "string",
                "optional": True,
            }
        }
        sources = InputSources.from_yaml(raw)
        instr = sources.get("instructions")
        assert instr is not None
        assert instr.optional is True

    def test_parse_multiple_sources(self) -> None:
        raw = {
            "issue": {"type": "string"},
            "plan": {"type": ["filepath", "string"], "optional": True},
            "instructions": {"type": "string", "optional": True},
        }
        sources = InputSources.from_yaml(raw)
        assert len(sources.all_sources()) == 3
        assert sources.get("issue") is not None
        assert sources.get("plan") is not None
        assert sources.get("instructions") is not None

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
        raw = {
            "issue": {},
        }
        with pytest.raises(ValueError, match="missing required 'type' field"):
            InputSources.from_yaml(raw)

    def test_invalid_type_value_rejected(self) -> None:
        raw = {
            "issue": {
                "type": "invalid-type",
            }
        }
        with pytest.raises(ValueError, match="unknown type"):
            InputSources.from_yaml(raw)

    def test_type_list_with_non_string_rejected(self) -> None:
        raw = {
            "issue": {
                "type": ["string", 123],
            }
        }
        with pytest.raises(ValueError, match="all type entries must be strings"):
            InputSources.from_yaml(raw)

    def test_empty_type_list_rejected(self) -> None:
        raw = {
            "issue": {
                "type": [],
            }
        }
        with pytest.raises(ValueError, match="type list must not be empty"):
            InputSources.from_yaml(raw)

    def test_non_mapping_source_rejected(self) -> None:
        raw = {
            "issue": ["string"],
        }
        with pytest.raises(ValueError, match="expected a mapping"):
            InputSources.from_yaml(raw)


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
                PLAN: plan?
              sources:
                issue:
                  type: string
                plan:
                  type: [filepath, string]
                  optional: true
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(p)
        assert pipeline.input_sources is not None
        assert pipeline.input_sources.get("issue") is not None
        assert pipeline.input_sources.get("plan") is not None
        issue = pipeline.input_sources.get("issue")
        assert issue.types == ["string"]
        plan = pipeline.input_sources.get("plan")
        assert plan.types == ["filepath", "string"]
        assert plan.optional is True

    def test_parse_pipeline_without_input_sources(self, tmp_path: pathlib.Path) -> None:
        p = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            inputs:
              in:
                PLAN: plan?
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(p)
        assert pipeline.input_sources is None

    def test_parse_pipeline_without_inputs(self, tmp_path: pathlib.Path) -> None:
        p = self._write_pipeline(
            tmp_path,
            """\
            default_client: claude:sonnet
            stages:
              - { name: plan, type: agent }
            """,
        )
        pipeline = Pipeline.from_yaml(p)
        assert pipeline.inputs is None
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
